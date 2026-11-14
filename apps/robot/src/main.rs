mod audio;
mod wifi;
use anyhow::{Context, Result, ensure};
use axum::{
    Router,
    extract::{DefaultBodyLimit, Form, State},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use rivetkit::{
    TypedActorHandle,
    client::{Client, ClientConfig, GetOptions},
};
use robot_buddy::{
    Bot,
    auth::{self, Identity},
    protocol::*,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    sync::RwLock,
};
#[derive(Default)]
struct Face {
    message: String,
    qr: String,
    code: String,
    confirm: bool,
}
#[derive(Clone)]
struct Screen {
    face: Arc<RwLock<Face>>,
    csrf: String,
}
#[derive(Default, Serialize, Deserialize)]
struct Cache {
    bot_id: String,
    cursor: i64,
    #[serde(default)]
    claim_url: String,
    #[serde(default)]
    claim_expires: i64,
    #[serde(default)]
    pending: Option<Pending>,
}
#[derive(Serialize, Deserialize)]
struct Pending {
    request_id: String,
    text: String,
}
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
fn svg(value: &str) -> Result<String> {
    Ok(qrcode::QrCode::new(value)?
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(320, 320)
        .build())
}
async fn face(State(screen): State<Screen>) -> Html<String> {
    let f = screen.face.read().await;
    Html(format!(
        "<!doctype html><html><head><meta name=viewport content='width=device-width,initial-scale=1'><meta http-equiv=refresh content=3><title>Robot Buddy</title></head><body><h1>{}</h1>{}<p>{}</p>{}</body></html>",
        escape(&f.message),
        f.qr,
        escape(&f.code),
        if f.code.is_empty() {
            String::new()
        } else {
            format!(
                "<form method=post action=/confirm><input type=hidden name=csrf value=\"{}\"><button>Confirm owner</button></form>",
                screen.csrf
            )
        }
    ))
}
#[derive(Deserialize)]
struct Confirm {
    csrf: String,
}
async fn confirm(State(screen): State<Screen>, Form(form): Form<Confirm>) -> Response {
    if form.csrf != screen.csrf {
        return (axum::http::StatusCode::FORBIDDEN, "invalid confirmation").into_response();
    }
    screen.face.write().await.confirm = true;
    axum::response::Redirect::to("/").into_response()
}
async fn set_face(screen: &Screen, message: &str, qr: &str) -> Result<()> {
    let mut f = screen.face.write().await;
    f.message = message.into();
    f.qr = if qr.is_empty() {
        String::new()
    } else {
        svg(qr)?
    };
    Ok(())
}
#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let args = std::env::args().collect::<Vec<_>>();
    let simulate = args.iter().any(|a| a == "--simulate");
    let server = std::env::var("BUDDY_SERVER_URL").context("BUDDY_SERVER_URL required")?;
    let url = url::Url::parse(&server)?;
    ensure!(
        url.scheme() == "https"
            || (simulate && matches!(url.host_str(), Some("localhost" | "127.0.0.1"))),
        "HTTPS required for robot server"
    );
    let identity: Identity = serde_json::from_slice(&std::fs::read(
        std::env::var("BUDDY_IDENTITY_FILE").context("BUDDY_IDENTITY_FILE required")?,
    )?)?;
    let cache_path = PathBuf::from(
        std::env::var("BUDDY_CACHE_FILE")
            .unwrap_or_else(|_| "/var/lib/robot-buddy/cache.json".into()),
    );
    let mut cache = std::fs::read(&cache_path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Cache>(&b).ok())
        .unwrap_or_default();
    let screen = Screen {
        face: Arc::new(RwLock::new(Face::default())),
        csrf: auth::random(),
    };
    let router = Router::new()
        .route("/", get(face))
        .route("/confirm", post(confirm))
        .layer(DefaultBodyLimit::max(4096))
        .with_state(screen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:8090").await?;
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, router).await {
            tracing::error!(error=%e,"face server failed");
        }
    });
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    if !simulate && !wifi::connected().await? {
        wifi::setup(screen.clone()).await?;
    }
    let (tx, mut input) = tokio::sync::mpsc::channel::<String>(8);
    tokio::spawn(async move {
        let mut lines = BufReader::new(tokio::io::stdin()).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if tx.send(line).await.is_err() {
                break;
            }
        }
    });
    let mut delay = 1;
    loop {
        set_face(&screen, "Connecting…", "").await?;
        match connect(&http, &server, &identity).await {
            Ok(bundle) => {
                if !cache.bot_id.is_empty() && cache.bot_id != bundle.bot_id {
                    anyhow::bail!("server returned a different bot identity");
                }
                cache.bot_id = bundle.bot_id.clone();
                save_cache(&cache_path, &cache)?;
                delay = 1;
                if let Err(_error) = session(
                    &bundle,
                    &server,
                    &screen,
                    &mut cache,
                    &cache_path,
                    &mut input,
                )
                .await
                {
                    tracing::warn!("connection interrupted; recovering same bot");
                }
            }
            Err(_error) => tracing::warn!("bootstrap unavailable"),
        }
        tokio::time::sleep(Duration::from_secs(delay)).await;
        delay = (delay * 2).min(30);
    }
}
async fn connect(http: &reqwest::Client, server: &str, identity: &Identity) -> Result<Bundle> {
    let response: Value = http
        .post(format!("{}/v1/challenge", server.trim_end_matches('/')))
        .json(&identity.certificate)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let challenge = response["challenge"]
        .as_str()
        .context("challenge missing")?
        .to_owned();
    Ok(http
        .post(format!("{}/v1/enroll", server.trim_end_matches('/')))
        .json(&EnrollmentRequest {
            certificate: identity.certificate.clone(),
            proof: identity.proof(&challenge)?,
            challenge,
        })
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}
async fn session(
    bundle: &Bundle,
    server: &str,
    screen: &Screen,
    cache: &mut Cache,
    path: &Path,
    input: &mut tokio::sync::mpsc::Receiver<String>,
) -> Result<()> {
    let client = Client::new(
        ClientConfig::new(&bundle.endpoint)
            .namespace(&bundle.namespace)
            .token(&bundle.gateway_token)
            .disable_metadata_lookup(true),
    );
    let handle = TypedActorHandle::<Bot>::new(client.get_for_id(
        "bot",
        &bundle.actor_id,
        GetOptions {
            params: Some(json!({"session":bundle.session})),
        },
    )?);
    let conversation =
        TypedActorHandle::<robot_buddy::conversation::Conversation>::new(client.get_for_id(
            "conversation",
            &bundle.conversation_actor_id,
            GetOptions {
                params: Some(json!({"session":bundle.session})),
            },
        )?);
    let connection = conversation.connect();
    let (notice, mut notices) = tokio::sync::mpsc::channel(1);
    let subscription = connection
        .on::<OutputEvent>(move |_| {
            let _ = notice.try_send(());
        })
        .await;

    let mut playback = audio::Playback::default();
    let mut live_turn: Option<String> = None;
    let mut bot_snapshot = Value::Null;
    let mut next_bot_check = tokio::time::Instant::now();
    loop {
        if auth::now() >= bundle.expires - 60 {
            drop(subscription);
            return Ok(());
        }
        if tokio::time::Instant::now() >= next_bot_check {
            bot_snapshot = handle
                .send(Call {
                    session: bundle.session.clone(),
                    command: Command::Snapshot,
                })
                .await?;
            next_bot_check = tokio::time::Instant::now() + Duration::from_secs(3);
        }
        let snapshot = &bot_snapshot;
        if snapshot["owner"].as_str() == Some("") {
            if auth::now() >= cache.claim_expires
                && (snapshot["pending_owner"].as_str() == Some("")
                    || snapshot["claim_expires"].as_i64().unwrap_or(0) <= auth::now())
            {
                let claim = handle
                    .send(Call {
                        session: bundle.session.clone(),
                        command: Command::BeginClaim,
                    })
                    .await?;
                cache.claim_expires = claim["expires"].as_i64().context("claim expiry missing")?;
                let mut url = url::Url::parse(&format!("{}/claim", server.trim_end_matches('/')))?;
                url.query_pairs_mut()
                    .append_pair("bot", &bundle.bot_id)
                    .append_pair(
                        "token",
                        claim["token"].as_str().context("claim token missing")?,
                    );
                set_face(screen, "Scan to register your robot", url.as_str()).await?;
                cache.claim_url = url.to_string();
                save_cache(path, cache)?;
            }
            if !cache.claim_url.is_empty() {
                set_face(screen, "Scan to register your robot", &cache.claim_url).await?;
            }
            let code = snapshot["confirm_code"].as_str().unwrap_or("");
            let confirmed = {
                let mut face = screen.face.write().await;
                face.code = code.into();
                let yes = face.confirm;
                face.confirm = false;
                yes
            };
            if confirmed && !code.is_empty() {
                handle
                    .send(Call {
                        session: bundle.session.clone(),
                        command: Command::ConfirmClaim { code: code.into() },
                    })
                    .await?;
            }
        } else {
            let mut f = screen.face.write().await;
            f.message = "Ready — hello!".into();
            f.qr.clear();
            f.code.clear();
        }
        loop {
            let events: Vec<OutputEvent> = serde_json::from_value(
                conversation
                    .send(Call {
                        session: bundle.session.clone(),
                        command: Command::Replay {
                            after: cache.cursor,
                        },
                    })
                    .await?,
            )?;
            if events.is_empty() {
                break;
            }
            for event in events {
                if event.sequence <= cache.cursor {
                    continue;
                }
                if event.kind == "speech.ready"
                    && event.payload["index"] == 0
                    && live_turn.as_deref() == Some(event.turn_id.as_str())
                {
                    playback.start(
                        server.to_owned(),
                        bundle.session.clone(),
                        event.turn_id.clone(),
                    );
                } else if event.kind == "turn.completed"
                    && live_turn.as_deref() == Some(event.turn_id.as_str())
                {
                    live_turn = None;
                } else if ["turn.cancelled", "turn.failed", "turn.interrupted"]
                    .contains(&event.kind.as_str())
                    && live_turn.as_deref() == Some(event.turn_id.as_str())
                {
                    playback.stop();
                    live_turn = None;
                }
                println!("{}", serde_json::to_string(&event)?);
                cache.cursor = event.sequence;
                save_cache(path, cache)?;
            }
        }
        if snapshot["owner"].as_str().is_some_and(|v| !v.is_empty())
            && let Some(pending) = &cache.pending
        {
            let submitted = conversation
                .send(Call {
                    session: bundle.session.clone(),
                    command: Command::Submit {
                        request_id: pending.request_id.clone(),
                        text: pending.text.clone(),
                    },
                })
                .await?;
            // Never replay audio from an old deduplicated submission after reconnect.
            if submitted["duplicate"] == false {
                live_turn = submitted["turn_id"].as_str().map(str::to_owned);
            }
            cache.pending = None;
            save_cache(path, cache)?;
        }
        tokio::select! {
            _=tokio::time::sleep(if live_turn.is_some(){Duration::from_millis(250)}else{Duration::from_secs(3)})=>{},
            _=notices.recv()=>{},
            line=input.recv(), if cache.pending.is_none()=>if let Some(text)=line {
                ensure!(!text.trim().is_empty() && text.len()<=8192,"invalid input");
                playback.stop();
                if let Some(turn_id)=live_turn.take(){conversation.send(Call { session:bundle.session.clone(),command:Command::Cancel{turn_id} }).await?;}
                cache.pending=Some(Pending{request_id:auth::random(),text});save_cache(path,cache)?;
            }else{tokio::time::sleep(Duration::from_secs(1)).await;},
        }
    }
}
fn save_cache(path: &Path, value: &Cache) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension(format!("{}.tmp", auth::random()));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    f.write_all(&serde_json::to_vec(value)?)?;
    f.sync_all()?;
    std::fs::rename(temp, path)?;
    Ok(())
}

//! One authenticated connection: bootstrap, Rivet actor handles, owner
//! registration, and the loop that feeds the controller.
use crate::{
    audio::{Media, Playback},
    cache::{Cache, CacheStore},
    config::EventDetail,
    controller::{Controller, OpenTurn, Remote, RobotState, Submitted},
    face::Face,
    input::Input,
    screen::Screen,
};
use anyhow::{Context, Result};
use rivetkit::{
    TypedActorHandle,
    client::{Client, ClientConfig, GetOptions},
};
use robot_buddy::{
    Bot,
    auth::{self, Identity},
    conversation::Conversation as ConversationActor,
    protocol::{Bundle, Call, Command, EnrollmentRequest, OutputEvent},
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::mpsc,
    time::{Instant, timeout},
};

const CALL_DEADLINE: Duration = Duration::from_secs(15);
const BOT_CHECK: Duration = Duration::from_secs(3);
/// Without pushed events, replay this often as a safety net.
const CATCH_UP_LIVE: Duration = Duration::from_secs(2);
const CATCH_UP_IDLE: Duration = Duration::from_secs(10);
const PUSHED_EVENTS: usize = 256;

/// Process-wide pieces a session borrows.
pub struct Shared<'a> {
    pub server: &'a str,
    pub screen: &'a Screen,
    pub face: &'a Face,
    pub store: &'a CacheStore,
    pub media: &'a Arc<Media>,
    pub detail: EventDetail,
}

/// Prove the device identity and receive a connection bundle.
pub async fn bootstrap(
    http: &reqwest::Client,
    server: &str,
    identity: &Identity,
) -> Result<Bundle> {
    let server = server.trim_end_matches('/');
    let response: Value = http
        .post(format!("{server}/v1/challenge"))
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
    let request = EnrollmentRequest {
        certificate: identity.certificate.clone(),
        proof: identity.proof(&challenge)?,
        challenge,
    };
    Ok(http
        .post(format!("{server}/v1/enroll"))
        .json(&request)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

struct Conversation {
    handle: TypedActorHandle<ConversationActor>,
    session: String,
}

impl Conversation {
    async fn call(&self, command: Command) -> Result<Value> {
        let call = Call {
            session: self.session.clone(),
            command,
        };
        timeout(CALL_DEADLINE, self.handle.send(call))
            .await
            .context("conversation call timed out")?
    }
}

impl Remote for Conversation {
    async fn submit(&self, request_id: &str, text: &str) -> Result<Submitted> {
        let command = Command::Submit {
            request_id: request_id.into(),
            text: text.into(),
        };
        let result = self.call(command).await?;
        Ok(Submitted {
            turn_id: result["turn_id"]
                .as_str()
                .context("turn id missing")?
                .into(),
            // Anything but an explicit `false` is treated as a replay: silent.
            duplicate: result["duplicate"] != false,
        })
    }

    async fn cancel(&self, turn_id: &str) -> Result<()> {
        let command = Command::Cancel {
            turn_id: turn_id.into(),
        };
        self.call(command).await.map(drop)
    }

    async fn replay(&self, after: i64) -> Result<Vec<OutputEvent>> {
        Ok(serde_json::from_value(
            self.call(Command::Replay { after }).await?,
        )?)
    }

    async fn open_turns(&self) -> Result<Vec<OpenTurn>> {
        let snapshot = self.call(Command::Snapshot).await?;
        let turns = snapshot["turns"].as_array().cloned().unwrap_or_default();
        Ok(turns
            .iter()
            .filter(|t| matches!(t["status"].as_str(), Some("queued" | "running")))
            .filter_map(|t| {
                Some(OpenTurn {
                    id: t["id"].as_str()?.to_owned(),
                    text: t["text"].as_str().unwrap_or_default().to_owned(),
                })
            })
            .collect())
    }
}

/// Run one session until the bundle nears expiry (Ok) or the connection fails.
pub async fn run(
    bundle: &Bundle,
    shared: &Shared<'_>,
    state: &mut RobotState,
    input: &mut Input,
) -> Result<()> {
    let client = Client::new(
        ClientConfig::new(&bundle.endpoint)
            .namespace(&bundle.namespace)
            .token(&bundle.gateway_token)
            .disable_metadata_lookup(true),
    );
    let options = || GetOptions {
        params: Some(json!({ "session": bundle.session })),
    };
    let bot =
        TypedActorHandle::<Bot>::new(client.get_for_id("bot", &bundle.actor_id, options())?);
    let conversation = TypedActorHandle::<ConversationActor>::new(client.get_for_id(
        "conversation",
        &bundle.conversation_actor_id,
        options(),
    )?);
    let connection = conversation.connect();
    let (push, mut pushed) = mpsc::channel(PUSHED_EVENTS);
    let overflow = Arc::new(AtomicBool::new(false));
    let overflowed = overflow.clone();
    let _subscription = connection
        .on::<OutputEvent>(move |event| {
            // Never block the client's event task. A dropped event shows up
            // as a gap and is replayed.
            if push.try_send(event).is_err() {
                overflowed.store(true, Ordering::Relaxed);
            }
        })
        .await;

    let media = shared.media.clone();
    tokio::spawn(async move { media.prewarm().await });
    let (playback_events, mut playback) = mpsc::channel(8);
    let speaker = Playback::new(
        shared.media.clone(),
        bundle.session.clone(),
        playback_events,
    );
    let remote = Conversation {
        handle: conversation,
        session: bundle.session.clone(),
    };
    let mut controller = Controller::new(
        remote,
        speaker,
        shared.face,
        shared.store,
        state,
        shared.detail,
    );
    let mut owned = false;
    let mut next_bot_check = Instant::now();
    let mut next_catch_up = Instant::now() + CATCH_UP_IDLE;
    loop {
        if auth::now() >= bundle.expires - 60 {
            return Ok(());
        }
        if Instant::now() >= next_bot_check {
            owned = registration(&bot, bundle, shared, controller.cache_mut()).await?;
            next_bot_check = Instant::now() + BOT_CHECK;
        }
        if overflow.swap(false, Ordering::Relaxed) || Instant::now() >= next_catch_up {
            controller.needs_replay = true;
        }
        if controller.needs_replay {
            // One page per pass, so input is serviced between pages.
            controller.replay_page().await?;
            next_catch_up = Instant::now() + catch_up(controller.is_live());
        }
        controller.send_intents(owned).await?;
        let paging = controller.needs_replay;
        let wake = next_bot_check.min(next_catch_up);
        tokio::select! {
            biased;
            control = input.next() => controller.control(control).await,
            Some(event) = playback.recv() => controller.playback(event),
            Some(event) = pushed.recv() => {
                controller.pushed(event).await;
                next_catch_up = Instant::now() + catch_up(controller.is_live());
            }
            () = std::future::ready(()), if paging => {}
            () = tokio::time::sleep_until(wake), if !paging => {}
        }
    }
}

fn catch_up(live: bool) -> Duration {
    if live { CATCH_UP_LIVE } else { CATCH_UP_IDLE }
}

/// Keep the loopback panel in step with ownership. Returns whether the bot
/// has an owner (only owned bots submit utterances).
async fn registration(
    bot: &TypedActorHandle<Bot>,
    bundle: &Bundle,
    shared: &Shared<'_>,
    cache: &mut Cache,
) -> Result<bool> {
    let call = |command| Call {
        session: bundle.session.clone(),
        command,
    };
    let snapshot = timeout(CALL_DEADLINE, bot.send(call(Command::Snapshot)))
        .await
        .context("bot snapshot timed out")??;
    let owner = snapshot["owner"].as_str();
    if owner != Some("") {
        shared.screen.ready().await;
        return Ok(owner.is_some_and(|o| !o.is_empty()));
    }
    let now = auth::now();
    let claim_lapsed = snapshot["pending_owner"].as_str() == Some("")
        || snapshot["claim_expires"].as_i64().unwrap_or(0) <= now;
    if now >= cache.claim_expires && claim_lapsed {
        let claim = timeout(CALL_DEADLINE, bot.send(call(Command::BeginClaim)))
            .await
            .context("claim timed out")??;
        cache.claim_expires = claim["expires"].as_i64().context("claim expiry missing")?;
        let token = claim["token"].as_str().context("claim token missing")?;
        let mut url = url::Url::parse(&format!("{}/claim", shared.server.trim_end_matches('/')))?;
        url.query_pairs_mut()
            .append_pair("bot", &bundle.bot_id)
            .append_pair("token", token);
        cache.claim_url = url.to_string();
        shared.store.checkpoint(cache, true);
    }
    if !cache.claim_url.is_empty() {
        shared
            .screen
            .show("Scan to register your robot", &cache.claim_url)
            .await?;
    }
    let code = snapshot["confirm_code"].as_str().unwrap_or("");
    if shared.screen.confirmation(code).await && !code.is_empty() {
        let confirm = call(Command::ConfirmClaim { code: code.into() });
        timeout(CALL_DEADLINE, bot.send(confirm))
            .await
            .context("confirm timed out")??;
    }
    Ok(false)
}

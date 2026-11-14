//! App-free SoftAP setup. Network credentials never leave this process for a cloud service.
use super::*;
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
};
#[derive(Clone)]
struct Setup {
    csrf: String,
    interface: String,
    done: tokio::sync::mpsc::Sender<()>,
    lock: Arc<tokio::sync::Mutex<()>>,
}
async fn nm(args: &[&str]) -> Result<String> {
    let output = tokio::time::timeout(
        Duration::from_secs(45),
        tokio::process::Command::new("nmcli").args(args).output(),
    )
    .await??;
    ensure!(output.status.success(), "NetworkManager operation failed");
    Ok(String::from_utf8(output.stdout)?)
}
pub async fn connected() -> Result<bool> {
    let interface = std::env::var("BUDDY_WIFI_INTERFACE").unwrap_or_else(|_| "wlan0".into());
    let state = nm(&["-g", "GENERAL.STATE", "device", "show", &interface]).await?;
    let profile = nm(&["-g", "GENERAL.CONNECTION", "device", "show", &interface]).await?;
    Ok(state.starts_with("100") && profile.trim() != "robot-buddy-setup")
}
pub async fn setup(screen: Screen) -> Result<()> {
    ensure!(
        cfg!(target_os = "linux"),
        "Wi-Fi setup requires Linux with NetworkManager; use --simulate elsewhere"
    );
    let interface = std::env::var("BUDDY_WIFI_INTERFACE").unwrap_or_else(|_| "wlan0".into());
    ensure!(
        interface
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-".contains(c)),
        "invalid interface"
    );
    let ssid = format!("RobotBuddy-{}", &auth::random()[..8]);
    let password = auth::random().replace('-', "");
    let profile = write_profile("setup", &ssid, &password, &interface, true)?;
    nm(&[
        "connection",
        "load",
        profile.to_str().context("profile path")?,
    ])
    .await?;
    nm(&["connection", "up", "id", "robot-buddy-setup"]).await?;
    set_face(
        &screen,
        "Scan to join setup Wi-Fi. Then open http://10.42.0.1",
        &format!("WIFI:T:WPA;S:{ssid};P:{password};;"),
    )
    .await?;
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let app = Router::new()
        .route("/", get(form))
        .route("/wifi", post(apply))
        .fallback(get(form))
        .layer(DefaultBodyLimit::max(4096))
        .with_state(Setup {
            csrf: auth::random(),
            interface,
            done: tx,
            lock: Arc::new(tokio::sync::Mutex::new(())),
        });
    let listener = tokio::net::TcpListener::bind("10.42.0.1:80").await?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    rx.recv().await.context("setup closed")?;
    server.abort();
    // The Wi-Fi radio has moved to the home profile, so the setup AP is no longer active.
    nm(&["connection", "delete", "id", "robot-buddy-setup"]).await?;
    Ok(())
}
async fn form(State(s): State<Setup>) -> Html<String> {
    Html(format!(
        "<!doctype html><html><head><meta name=viewport content='width=device-width,initial-scale=1'><title>Connect Robot Buddy</title></head><body><h1>Connect Robot Buddy</h1><p>Enter your home Wi-Fi details. They stay on this robot.</p><form method=post action=/wifi><input type=hidden name=csrf value=\"{}\"><label>Network name <input name=ssid required maxlength=32 autocomplete=off></label><label>Password <input name=password type=password required minlength=8 maxlength=63 autocomplete=off></label><button>Connect</button></form></body></html>",
        escape(&s.csrf)
    ))
}
#[derive(Deserialize)]
struct WifiForm {
    csrf: String,
    ssid: String,
    password: String,
}
async fn apply(State(s): State<Setup>, headers: HeaderMap, Form(f): Form<WifiForm>) -> Response {
    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if host != "10.42.0.1" && host != "10.42.0.1:80" {
        return (StatusCode::FORBIDDEN, "Invalid host").into_response();
    }
    if headers
        .get("origin")
        .is_some_and(|v| v != "http://10.42.0.1")
        || f.csrf != s.csrf
    {
        return (StatusCode::FORBIDDEN, "Invalid setup session").into_response();
    }
    if validate(&f.ssid, &f.password).is_err() {
        return (
            StatusCode::BAD_REQUEST,
            "Enter a valid network name and WPA password.",
        )
            .into_response();
    }
    let Ok(guard) = s.lock.clone().try_lock_owned() else {
        return (StatusCode::CONFLICT, "Connection attempt in progress").into_response();
    };
    tokio::spawn(async move {
        let _guard = guard;
        tokio::time::sleep(Duration::from_secs(1)).await;
        let result = async {
            let profile = write_profile("home", &f.ssid, &f.password, &s.interface, false)?;
            nm(&[
                "connection",
                "load",
                profile.to_str().context("invalid path")?,
            ])
            .await?;
            nm(&["--wait", "30", "connection", "up", "id", "robot-buddy-home"]).await?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if result.is_ok() {
            let _ = s.done.send(()).await;
        } else {
            tracing::warn!("Wi-Fi connection failed; restoring setup network");
            let _ = nm(&["connection", "up", "id", "robot-buddy-setup"]).await;
        }
    });
    Html("<h1>Connecting…</h1><p>Your phone will leave the robot’s setup network. Watch the robot’s face for the registration QR. If connection fails, rejoin its setup network and try again.</p>").into_response()
}
fn validate(ssid: &str, password: &str) -> Result<()> {
    ensure!(
        !ssid.is_empty() && ssid.len() <= 32 && !ssid.chars().any(|c| c.is_control()),
        "invalid SSID"
    );
    ensure!(
        (8..=63).contains(&password.len()) && !password.chars().any(|c| c.is_control()),
        "invalid password"
    );
    Ok(())
}
fn keyfile(value: &str) -> String {
    value.replace('\\', "\\\\").replace(' ', "\\s")
}
fn write_profile(
    name: &str,
    ssid: &str,
    password: &str,
    interface: &str,
    ap: bool,
) -> Result<PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    validate(ssid, password)?;
    let path = PathBuf::from(format!(
        "/etc/NetworkManager/system-connections/robot-buddy-{name}.nmconnection"
    ));
    let text = format!(
        "[connection]\nid=robot-buddy-{name}\nuuid={}\ntype=wifi\ninterface-name={interface}\nautoconnect={}\n\n[wifi]\nmode={}\nssid={}\n\n[wifi-security]\nkey-mgmt=wpa-psk\npsk={}\n\n[ipv4]\nmethod={}\n{}\n[ipv6]\nmethod=disabled\n",
        uuid::Uuid::new_v4(),
        !ap,
        if ap { "ap" } else { "infrastructure" },
        keyfile(ssid),
        keyfile(password),
        if ap { "shared" } else { "auto" },
        if ap { "address1=10.42.0.1/24" } else { "" }
    );
    let temp = path.with_extension(format!("{}.tmp", auth::random()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(temp, &path)?;
    Ok(path)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_keyfile_injection() {
        assert!(validate("home\n[connection]", "password123").is_err());
        assert!(validate("home", "password\n").is_err());
    }
    #[test]
    fn escapes_network_values() {
        assert_eq!(keyfile("a b\\c"), "a\\sb\\\\c");
    }
}

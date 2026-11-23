//! App-free SoftAP setup. Network credentials never leave this process for a cloud service.
use crate::screen::{Screen, escape};
use anyhow::{Context, Result, bail, ensure};
use axum::{
    Router,
    extract::{DefaultBodyLimit, Form, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use robot_buddy::auth;
use serde::Deserialize;
use std::{path::PathBuf, process::Stdio, sync::Arc, time::Duration};
use tokio::sync::{Mutex, mpsc};

const SETUP_ID: &str = "robot-buddy-setup";
const HOME_ID: &str = "robot-buddy-home";
const SETUP_HOST: &str = "10.42.0.1";
const NM_DEADLINE: Duration = Duration::from_secs(45);

#[derive(Clone)]
struct Setup {
    csrf: String,
    interface: String,
    done: mpsc::Sender<()>,
    lock: Arc<Mutex<()>>,
    screen: Screen,
}

/// Run `nmcli` with each argument passed separately (no shell). The child is
/// killed if it overruns the deadline.
async fn nm(args: &[&str]) -> Result<String> {
    let mut command = tokio::process::Command::new("nmcli");
    command.args(args).stdin(Stdio::null()).kill_on_drop(true);
    let output = tokio::time::timeout(NM_DEADLINE, command.output())
        .await
        .context("NetworkManager operation timed out")??;
    ensure!(
        output.status.success(),
        "NetworkManager operation failed (exit {:?})",
        output.status.code()
    );
    Ok(String::from_utf8(output.stdout)?)
}

fn interface() -> Result<String> {
    let interface = std::env::var("BUDDY_WIFI_INTERFACE").unwrap_or_else(|_| "wlan0".into());
    let valid = !interface.is_empty()
        && interface.len() <= 15
        && interface
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-".contains(c));
    ensure!(valid, "invalid interface");
    Ok(interface)
}

/// Whether the Wi-Fi interface is connected to a network other than the setup hotspot.
pub async fn connected() -> Result<bool> {
    let interface = interface()?;
    let state = nm(&["-g", "GENERAL.STATE", "device", "show", &interface]).await?;
    let profile = nm(&["-g", "GENERAL.CONNECTION", "device", "show", &interface]).await?;
    Ok(state.starts_with("100") && profile.trim() != SETUP_ID)
}

/// Host the setup hotspot and form until home Wi-Fi connects. With a
/// `window`, give up after it and remove the hotspot so NetworkManager can
/// rejoin a known network.
pub async fn setup(screen: &Screen, window: Option<Duration>) -> Result<()> {
    ensure!(
        cfg!(target_os = "linux"),
        "Wi-Fi setup requires Linux with NetworkManager; use --simulate elsewhere"
    );
    let interface = interface()?;
    let ssid = format!("RobotBuddy-{}", &auth::random()[..8]);
    let password = auth::random().replace('-', "");
    let profile = write_profile(&Profile {
        name: "setup",
        ssid: &ssid,
        password: &password,
        interface: &interface,
        access_point: true,
    })?;
    nm(&[
        "connection",
        "load",
        profile.to_str().context("profile path")?,
    ])
    .await?;
    nm(&["connection", "up", "id", SETUP_ID]).await?;
    let join = format!("WIFI:T:WPA;S:{ssid};P:{password};;");
    screen
        .show(
            "Scan to join setup Wi-Fi. Then open http://10.42.0.1",
            &join,
        )
        .await?;
    let (done, mut finished) = mpsc::channel(1);
    let app = Router::new()
        .route("/", get(form))
        .route("/wifi", post(apply))
        .fallback(get(form))
        .layer(DefaultBodyLimit::max(4096))
        .with_state(Setup {
            csrf: auth::random(),
            interface,
            done,
            lock: Arc::new(Mutex::new(())),
            screen: screen.clone(),
        });
    let listener = tokio::net::TcpListener::bind(format!("{SETUP_HOST}:80")).await?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let joined = match window {
        Some(window) => tokio::time::timeout(window, finished.recv())
            .await
            .unwrap_or(None),
        None => finished.recv().await,
    };
    server.abort();
    // After success the radio is on the home profile, so the hotspot is
    // already down. After a timeout, deleting it frees the radio.
    nm(&["connection", "delete", "id", SETUP_ID]).await?;
    match (joined, window) {
        (Some(()), _) => Ok(()),
        (None, Some(_)) => {
            tracing::info!("Wi-Fi setup window closed; retrying known networks");
            Ok(())
        }
        (None, None) => bail!("setup closed"),
    }
}

const FORM_HEAD: &str = "<!doctype html><html><head>\
    <meta name=viewport content='width=device-width,initial-scale=1'>\
    <title>Connect Robot Buddy</title></head><body>\
    <h1>Connect Robot Buddy</h1>\
    <p>Enter your home Wi-Fi details. They stay on this robot.</p>\
    <form method=post action=/wifi>";

const FORM_FIELDS: &str = "<label>Network name \
    <input name=ssid required maxlength=32 autocomplete=off></label>\
    <label>Password <input name=password type=password required \
    minlength=8 maxlength=63 autocomplete=off></label>\
    <button>Connect</button></form></body></html>";

const CONNECTING: &str = "<h1>Connecting…</h1>\
    <p>Your phone will leave the robot’s setup network. \
    Watch the robot’s face for the registration QR. \
    If connection fails, rejoin its setup network and try again.</p>";

async fn form(State(s): State<Setup>) -> Html<String> {
    Html(format!(
        "{FORM_HEAD}<input type=hidden name=csrf value=\"{}\">{FORM_FIELDS}",
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
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let host = header("host").unwrap_or("");
    if host != SETUP_HOST && host != "10.42.0.1:80" {
        return (StatusCode::FORBIDDEN, "Invalid host").into_response();
    }
    let foreign = header("origin").is_some_and(|o| o != "http://10.42.0.1")
        || headers.get("origin").is_some_and(|o| o.to_str().is_err());
    if foreign || f.csrf != s.csrf {
        return (StatusCode::FORBIDDEN, "Invalid setup session").into_response();
    }
    if validate(&f.ssid, &f.password).is_err() {
        let message = "Enter a valid network name and WPA password.";
        return (StatusCode::BAD_REQUEST, message).into_response();
    }
    let Ok(guard) = s.lock.clone().try_lock_owned() else {
        return (StatusCode::CONFLICT, "Connection attempt in progress").into_response();
    };
    tokio::spawn(async move {
        let _guard = guard;
        // Let the response reach the phone before the radio switches.
        tokio::time::sleep(Duration::from_secs(1)).await;
        match join_home(&s, &f).await {
            Ok(()) => {
                let _ = s.done.send(()).await;
            }
            Err(error) => {
                tracing::warn!(%error, "Wi-Fi connection failed; restoring setup network");
                restore_setup(&s.screen).await;
            }
        }
    });
    Html(CONNECTING).into_response()
}

async fn join_home(s: &Setup, f: &WifiForm) -> Result<()> {
    let profile = write_profile(&Profile {
        name: "home",
        ssid: &f.ssid,
        password: &f.password,
        interface: &s.interface,
        access_point: false,
    })?;
    nm(&[
        "connection",
        "load",
        profile.to_str().context("invalid path")?,
    ])
    .await?;
    nm(&["--wait", "30", "connection", "up", "id", HOME_ID]).await?;
    Ok(())
}

async fn restore_setup(screen: &Screen) {
    if let Err(error) = nm(&["connection", "up", "id", SETUP_ID]).await {
        tracing::error!(%error, "setup network could not be restored");
        let message = "Wi-Fi setup failed. Restart the robot to try again.";
        if let Err(error) = screen.show(message, "").await {
            tracing::warn!(%error, "could not update the setup screen");
        }
    }
}

/// SSID: 1–32 bytes without control characters. Password: a WPA passphrase
/// of 8–63 printable ASCII characters.
fn validate(ssid: &str, password: &str) -> Result<()> {
    ensure!(
        !ssid.is_empty() && ssid.len() <= 32 && !ssid.chars().any(char::is_control),
        "invalid SSID"
    );
    ensure!(
        (8..=63).contains(&password.len()) && password.chars().all(|c| (' '..='~').contains(&c)),
        "invalid password"
    );
    Ok(())
}

/// NetworkManager keyfile escaping for a string value.
fn keyfile(value: &str) -> String {
    value.replace('\\', "\\\\").replace(' ', "\\s")
}

struct Profile<'a> {
    name: &'a str,
    ssid: &'a str,
    password: &'a str,
    interface: &'a str,
    access_point: bool,
}

fn profile_text(p: &Profile<'_>, uuid: &str) -> String {
    let ap = p.access_point;
    let (mode, method, address) = if ap {
        ("ap", "shared", "address1=10.42.0.1/24\n")
    } else {
        ("infrastructure", "auto", "\n")
    };
    let lines = [
        "[connection]".to_owned(),
        format!("id=robot-buddy-{}", p.name),
        format!("uuid={uuid}"),
        "type=wifi".into(),
        format!("interface-name={}", p.interface),
        format!("autoconnect={}", !ap),
        String::new(),
        "[wifi]".into(),
        format!("mode={mode}"),
        format!("ssid={}", keyfile(p.ssid)),
        String::new(),
        "[wifi-security]".into(),
        "key-mgmt=wpa-psk".into(),
        format!("psk={}", keyfile(p.password)),
        String::new(),
        "[ipv4]".into(),
        format!("method={method}"),
    ];
    format!("{}\n{address}[ipv6]\nmethod=disabled\n", lines.join("\n"))
}

fn write_profile(profile: &Profile<'_>) -> Result<PathBuf> {
    validate(profile.ssid, profile.password)?;
    let path = PathBuf::from(format!(
        "/etc/NetworkManager/system-connections/robot-buddy-{}.nmconnection",
        profile.name
    ));
    let text = profile_text(profile, &uuid::Uuid::new_v4().to_string());
    crate::cache::persist(&path, text.as_bytes())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_keyfile_injection() {
        assert!(validate("home\n[connection]", "password123").is_err());
        assert!(validate("home", "password\n").is_err());
        assert!(validate("", "password123").is_err());
        assert!(validate(&"x".repeat(33), "password123").is_err());
        assert!(validate("home", "short").is_err());
        assert!(
            validate("home", "pässwörd123").is_err(),
            "WPA passphrases are ASCII"
        );
        assert!(validate("Café Wi-Fi", "correct horse 9").is_ok());
    }

    #[test]
    fn escapes_network_values() {
        assert_eq!(keyfile("a b\\c"), "a\\sb\\\\c");
    }

    /// The profile text must stay byte-for-byte what NetworkManager received
    /// before this module was reformatted.
    #[test]
    fn profile_text_is_unchanged() {
        for ap in [false, true] {
            let (ssid, password, uuid) = (
                "My Net",
                "pass word\\1",
                "00000000-0000-0000-0000-000000000000",
            );
            let expected = format!(
                "[connection]\nid=robot-buddy-{name}\nuuid={uuid}\ntype=wifi\n\
                 interface-name={interface}\nautoconnect={}\n\n[wifi]\nmode={}\nssid={}\n\n\
                 [wifi-security]\nkey-mgmt=wpa-psk\npsk={}\n\n[ipv4]\nmethod={}\n{}\n\
                 [ipv6]\nmethod=disabled\n",
                !ap,
                if ap { "ap" } else { "infrastructure" },
                keyfile(ssid),
                keyfile(password),
                if ap { "shared" } else { "auto" },
                if ap { "address1=10.42.0.1/24" } else { "" },
                name = "home",
                interface = "wlan0",
            );
            let profile = Profile {
                name: "home",
                ssid,
                password,
                interface: "wlan0",
                access_point: ap,
            };
            assert_eq!(profile_text(&profile, uuid), expected);
        }
    }
}

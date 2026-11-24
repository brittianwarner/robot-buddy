//! App-free `SoftAP` setup. Network credentials never leave this process for a cloud service.
use crate::screen::{Screen, escape, header_in, local_headers};
use anyhow::{Context, Result, bail, ensure};
use axum::{
    Router,
    extract::{DefaultBodyLimit, Form, State},
    http::{HeaderMap, StatusCode, header},
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
const SETUP_HOSTS: [&str; 2] = [SETUP_HOST, "10.42.0.1:80"];
/// Phone browsers send `Origin` on every form POST (same-origin included); a missing one
/// is refused. The setup page's `same-origin` referrer policy keeps it from being `null`.
const SETUP_ORIGIN: &str = "http://10.42.0.1";
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
///
/// No `--` separator: nmcli does not document one for these subcommands. Every positional
/// value is a constant, a path we built, or an interface name that cannot start with `-`.
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

/// `^[A-Za-z0-9][A-Za-z0-9_-]{0,14}$`: a Linux interface name that nmcli cannot mistake
/// for an option and that is safe inside a keyfile line.
fn valid_interface(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=15).contains(&bytes.len())
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

fn interface() -> Result<String> {
    let interface = std::env::var("BUDDY_WIFI_INTERFACE").unwrap_or_else(|_| "wlan0".into());
    ensure!(valid_interface(&interface), "invalid BUDDY_WIFI_INTERFACE");
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
/// `window`, give up after it and remove the hotspot so `NetworkManager` can
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

/// Served for any `Host`, so captive-portal probes land on the form.
async fn form(State(s): State<Setup>) -> Response {
    let mut response = Html(format!(
        "{FORM_HEAD}<input type=hidden name=csrf value=\"{}\">{FORM_FIELDS}",
        escape(&s.csrf)
    ))
    .into_response();
    local_headers(&mut response);
    response
}

fn reply(status: StatusCode, body: &'static str) -> Response {
    let mut response = (status, body).into_response();
    local_headers(&mut response);
    response
}

#[derive(Deserialize)]
struct WifiForm {
    csrf: String,
    ssid: String,
    password: String,
}

async fn apply(State(s): State<Setup>, headers: HeaderMap, Form(f): Form<WifiForm>) -> Response {
    if !header_in(&headers, header::HOST, &SETUP_HOSTS) {
        return reply(StatusCode::FORBIDDEN, "Invalid host");
    }
    let same_origin = header_in(&headers, header::ORIGIN, &[SETUP_ORIGIN]);
    if !same_origin || !auth::ct_eq(f.csrf.as_bytes(), s.csrf.as_bytes()) {
        return reply(StatusCode::FORBIDDEN, "Invalid setup session");
    }
    if validate(&f.ssid, &f.password).is_err() {
        let message = "Enter a valid network name and WPA password.";
        return reply(StatusCode::BAD_REQUEST, message);
    }
    let Ok(guard) = s.lock.clone().try_lock_owned() else {
        return reply(StatusCode::CONFLICT, "Connection attempt in progress");
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
    let mut response = Html(CONNECTING).into_response();
    local_headers(&mut response);
    response
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

/// `GLib` key-file escaping for a string value: `\\`, `\s` for spaces, `\n`, `\t`, `\r`.
///
/// `GLib` only needs `\s` for leading spaces but decodes it anywhere; escaping every space
/// keeps profiles byte-identical to earlier releases. `;` and `#` stay literal: `;` only
/// separates list values and `#` only starts a comment at the start of a line.
fn keyfile(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            ' ' => out.push_str("\\s"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}

/// `NetworkManager` reads `ssid=` as bytes and treats `;`-separated integers as a byte list,
/// with its own `\;` unescaping for strings. An SSID containing `;` or `\` is therefore
/// written as the unambiguous byte list (`ssid=97;59;98;`), as `NetworkManager` does itself.
fn ssid_value(ssid: &str) -> String {
    if ssid.contains([';', '\\']) {
        ssid.bytes().fold(String::new(), |mut out, b| {
            out.push_str(&b.to_string());
            out.push(';');
            out
        })
    } else {
        keyfile(ssid)
    }
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
        format!("ssid={}", ssid_value(p.ssid)),
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
    ensure!(valid_interface(profile.interface), "invalid interface");
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
        assert_eq!(keyfile(" lead"), "\\slead");
        assert_eq!(keyfile("a\nb\tc\rd"), "a\\nb\\tc\\rd");
        assert_eq!(keyfile("p;a#ss"), "p;a#ss");
        assert_eq!(ssid_value("My Net"), "My\\sNet");
        assert_eq!(ssid_value("Café"), "Café");
        assert_eq!(ssid_value("a;b"), "97;59;98;");
        assert_eq!(ssid_value("1;2;"), "49;59;50;59;");
        assert_eq!(ssid_value("a\\b"), "97;92;98;");
    }

    #[test]
    fn interface_names_cannot_be_options() {
        for ok in ["wlan0", "wlp2s0", "w", "a_b-c", "abcdefghijklmno"] {
            assert!(valid_interface(ok), "{ok}");
        }
        for bad in [
            "",
            "-wlan0",
            "_wlan0",
            "wlan 0",
            "wlan0\n",
            "abcdefghijklmnop",
            "wlån0",
        ] {
            assert!(!valid_interface(bad), "{bad:?}");
        }
    }

    fn setup() -> Setup {
        let (done, _) = mpsc::channel(1);
        Setup {
            csrf: "setup-csrf".into(),
            interface: "wlan0".into(),
            done,
            lock: Arc::new(Mutex::new(())),
            screen: Screen::new(),
        }
    }

    fn request(host: &str, origin: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, host.parse().unwrap());
        if let Some(origin) = origin {
            headers.insert(header::ORIGIN, origin.parse().unwrap());
        }
        headers
    }

    async fn post(headers: HeaderMap, csrf: &str, ssid: &str) -> Response {
        let form = WifiForm {
            csrf: csrf.into(),
            ssid: ssid.into(),
            password: "password123".into(),
        };
        apply(State(setup()), headers, Form(form)).await
    }

    #[tokio::test]
    async fn setup_form_requires_host_origin_and_csrf() {
        let origin = Some(SETUP_ORIGIN);
        for (headers, csrf) in [
            (request(SETUP_HOST, None), "setup-csrf"),
            (
                request(SETUP_HOST, Some("http://attacker.example")),
                "setup-csrf",
            ),
            (request(SETUP_HOST, Some("null")), "setup-csrf"),
            (request("attacker.example", origin), "setup-csrf"),
            (request(SETUP_HOST, origin), "forged"),
        ] {
            let response = post(headers, csrf, "home").await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
            assert_eq!(response.headers()[header::X_FRAME_OPTIONS], "DENY");
        }
        // Passing every check reaches validation (an invalid SSID stops before nmcli).
        let response = post(request("10.42.0.1:80", origin), "setup-csrf", "").await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let page = form(State(setup())).await;
        assert_eq!(page.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(page.headers()[header::REFERRER_POLICY], "same-origin");
    }

    #[test]
    fn profile_rejects_an_invalid_interface() {
        let profile = Profile {
            name: "home",
            ssid: "home",
            password: "password123",
            interface: "-x",
            access_point: false,
        };
        assert!(write_profile(&profile).is_err());
    }

    /// The profile text must stay byte-for-byte what `NetworkManager` received
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

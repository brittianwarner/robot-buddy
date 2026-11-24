//! App-free `SoftAP` setup. Network credentials never leave this process for a cloud service.
//!
//! Every `NetworkManager` change goes over D-Bus through `nmcli` (no shell, one
//! argv entry per value), so the service needs neither root nor write access to
//! `/etc/NetworkManager`: `infra/robot-buddy-nm.rules` lets the `robot-buddy`
//! account make exactly these calls. The home password never appears in argv:
//! nmcli reads it from a 0600 file that is removed as soon as activation ends,
//! and `NetworkManager` saves it with the profile.
//!
//! A new home network is staged as its own profile (its own UUID) and only
//! replaces the previous one once it has joined and its password reads back
//! from `NetworkManager`; a mistyped password leaves the old network in place.
pub mod card;

use crate::guard;
use crate::screen::{Screen, escape, header_in, local_headers};
use anyhow::{Context, Result, anyhow, bail, ensure};
use axum::{
    Router,
    extract::{DefaultBodyLimit, Form, State},
    http::{HeaderMap, StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use robot_buddy::auth::{self, Secret};
use serde::Deserialize;
use std::{
    io::Write,
    net::{Ipv4Addr, SocketAddr},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::{Output, Stdio},
    sync::Arc,
    time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::{Mutex, mpsc},
};

const SETUP_ID: &str = "robot-buddy-setup";
const HOME_ID: &str = "robot-buddy-home";
const SETUP_HOST: &str = "10.42.0.1";
const SETUP_ADDRESS: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::new(10, 42, 0, 1)), 80);
const SETUP_HOSTS: [&str; 2] = [SETUP_HOST, "10.42.0.1:80"];
/// Phone browsers send `Origin` on every form POST (same-origin included); a missing one
/// is refused. The setup page's `same-origin` referrer policy keeps it from being `null`.
const SETUP_ORIGIN: &str = "http://10.42.0.1";
const NM_DEADLINE: Duration = Duration::from_secs(45);
/// nmcli's exit status when the named connection does not exist.
const NOT_FOUND: i32 = 10;
/// Enough to clear duplicates left by older releases; more means something is
/// re-adding profiles behind our back.
const MAX_COPIES: usize = 8;
/// `10.42.0.1` appears a moment after the hotspot reports up.
const BIND_TRIES: u32 = 25;
const BIND_RETRY: Duration = Duration::from_millis(200);
/// nmcli error text kept for the log, from calls that carry no secret.
const ERROR_CAP: usize = 300;

#[derive(Clone)]
struct Setup {
    csrf: String,
    interface: String,
    done: mpsc::Sender<()>,
    lock: Arc<Mutex<()>>,
    screen: Screen,
}

#[cfg(not(test))]
fn program() -> &'static Path {
    Path::new("nmcli")
}

#[cfg(test)]
fn program() -> &'static Path {
    tests::fake_nmcli()
}

/// Run `nmcli` with each argument passed separately (no shell) and the C
/// locale, so its output parses. The child is killed if it overruns the deadline.
///
/// No `--` separator: nmcli does not document one for these subcommands. Every value is
/// a constant, a path we built, an interface name that cannot start with `-`, a UUID
/// nmcli printed, or an SSID that [`validate`] keeps from starting with `-`.
async fn run(args: &[&str]) -> Result<Output> {
    let mut command = tokio::process::Command::new(program());
    command
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    tokio::time::timeout(NM_DEADLINE, command.output())
        .await
        .context("NetworkManager operation timed out")?
        .context("nmcli could not start")
}

/// What nmcli's exit status means (`nmcli(1)`, "Exit status").
fn meaning(code: Option<i32>) -> &'static str {
    match code {
        Some(2) => "invalid input",
        Some(3) => "timed out",
        Some(4) => "activation failed",
        Some(5) => "deactivation failed",
        Some(7) => "deletion failed",
        Some(8) => "NetworkManager is not running",
        Some(NOT_FOUND) => "no such connection",
        _ => "error",
    }
}

fn failure(output: &Output) -> anyhow::Error {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail: String = stderr.trim().chars().take(ERROR_CAP).collect();
    anyhow!(
        "NetworkManager operation failed (exit {:?}, {}): {}",
        output.status.code(),
        meaning(output.status.code()),
        detail.replace('\n', " | ")
    )
}

async fn nm(args: &[&str]) -> Result<String> {
    let output = run(args).await?;
    if !output.status.success() {
        return Err(failure(&output));
    }
    Ok(String::from_utf8(output.stdout)?)
}

/// For calls that carry a secret in argv, in a passwd-file or in their output:
/// stdout and stderr are wiped, and a failure reports the exit status only.
async fn nm_secret(args: &[&str]) -> Result<Secret<String>> {
    let output = run(args).await?;
    let code = output.status.code();
    let success = output.status.success();
    drop(Secret::new(output.stderr));
    if !success {
        drop(Secret::new(output.stdout));
        bail!(
            "NetworkManager operation failed (exit {code:?}, {})",
            meaning(code)
        );
    }
    match String::from_utf8(output.stdout) {
        Ok(text) => Ok(Secret::new(text)),
        Err(error) => {
            drop(Secret::new(error.into_bytes()));
            bail!("NetworkManager output is not UTF-8")
        }
    }
}

/// Delete every profile named `id`. nmcli deletes one match per call on some
/// versions, so repeat until it reports none.
async fn delete_profiles(id: &str) -> Result<()> {
    for _ in 0..MAX_COPIES {
        let output = run(&["connection", "delete", "id", id]).await?;
        match output.status.code() {
            Some(0) => {}
            Some(NOT_FOUND) => return Ok(()),
            _ => return Err(failure(&output)),
        }
    }
    bail!("more than {MAX_COPIES} {id} profiles")
}

/// `^[A-Za-z0-9][A-Za-z0-9_-]{0,14}$`: a Linux interface name that nmcli cannot mistake
/// for an option.
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

/// `nmcli -t -f STATE general` is `connected` only with a global route:
/// any uplink counts (Wi-Fi, Ethernet, USB tethering). The setup hotspot alone
/// is `connected (local only)`; a captive portal is `connected (site only)`.
fn uplink(state: &str) -> bool {
    state.trim() == "connected"
}

/// Whether the robot has a working uplink on any interface.
pub async fn connected() -> Result<bool> {
    Ok(uplink(&nm(&["-t", "-f", "STATE", "general"]).await?))
}

/// The setup hotspot, held in `NetworkManager` memory only (`save no`). Dropping
/// it without [`Hotspot::remove`] (an error, or SIGTERM cancelling setup)
/// still deletes it from a detached nmcli; the unit's `ExecStopPost` is the
/// backstop when the service's processes are killed first.
struct Hotspot {
    armed: bool,
}

impl Hotspot {
    async fn start(interface: &str, ssid: &str, password: &str) -> Result<Self> {
        delete_profiles(SETUP_ID).await?;
        let hotspot = Self { armed: true };
        // The hotspot password is printed on the robot's setup card and only
        // admits a phone to the setup page, so it rides in argv (visible to local
        // accounts for the moment nmcli runs). The call's output is still treated
        // as secret.
        nm_secret(&[
            "connection",
            "add",
            "save",
            "no",
            "type",
            "wifi",
            "con-name",
            SETUP_ID,
            "ifname",
            interface,
            "autoconnect",
            "no",
            "ssid",
            ssid,
            "802-11-wireless.mode",
            "ap",
            "wifi-sec.key-mgmt",
            "wpa-psk",
            "wifi-sec.psk",
            password,
            "ipv4.method",
            "shared",
            "ipv4.addresses",
            "10.42.0.1/24",
            "ipv6.method",
            "disabled",
        ])
        .await?;
        nm(&["connection", "up", "id", SETUP_ID]).await?;
        Ok(hotspot)
    }

    async fn remove(mut self) -> Result<()> {
        delete_profiles(SETUP_ID).await?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for Hotspot {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let spawned = std::process::Command::new(program())
            .args(["connection", "delete", "id", SETUP_ID])
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        match spawned {
            Ok(mut child) => drop(std::thread::spawn(move || child.wait())),
            Err(error) => tracing::warn!(%error, "setup hotspot cleanup could not start"),
        }
    }
}

/// Bind `address`, retrying while the interface address is still arriving.
async fn bind_retrying(address: SocketAddr, tries: u32, delay: Duration) -> Result<TcpListener> {
    let mut attempt = 1;
    loop {
        match TcpListener::bind(address).await {
            Ok(listener) => return Ok(listener),
            Err(_) if attempt < tries => {
                attempt += 1;
                tokio::time::sleep(delay).await;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("setup page cannot listen on {address}"));
            }
        }
    }
}

/// The robot's own setup network: the one on its card, or a one-time one
/// (panel QR only) when the card file cannot be read or made.
fn setup_network() -> card::SetupWifi {
    match card::load_or_create(&card::path()) {
        Ok(setup) => setup,
        Err(error) => {
            tracing::warn!(%error, "setup card credentials unavailable; using a one-time network");
            card::SetupWifi::generate()
        }
    }
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
    let network = setup_network();
    let hotspot = Hotspot::start(&interface, &network.ssid, network.password.expose()).await?;
    let listener = bind_retrying(SETUP_ADDRESS, BIND_TRIES, BIND_RETRY).await?;
    screen
        .show(
            "Join the setup Wi-Fi on the robot's card, or scan this. Then open http://10.42.0.1",
            network.join_payload().expose(),
        )
        .await?;
    drop(network);
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
    let listener = guard::guard(listener, guard::Limits::PAGE);
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
    hotspot.remove().await?;
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
    password: Secret<String>,
}

async fn apply(State(s): State<Setup>, headers: HeaderMap, Form(f): Form<WifiForm>) -> Response {
    if !header_in(&headers, header::HOST, &SETUP_HOSTS) {
        return reply(StatusCode::FORBIDDEN, "Invalid host");
    }
    let same_origin = header_in(&headers, header::ORIGIN, &[SETUP_ORIGIN]);
    if !same_origin || !auth::ct_eq(f.csrf.as_bytes(), s.csrf.as_bytes()) {
        return reply(StatusCode::FORBIDDEN, "Invalid setup session");
    }
    if validate(&f.ssid, f.password.expose()).is_err() {
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
        match join_home(&s.interface, &f).await {
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

/// A 0600 file in the service's runtime directory, removed on drop.
struct SecretFile(PathBuf);

impl SecretFile {
    fn create(contents: &str) -> Result<Self> {
        let directory = std::env::var("RUNTIME_DIRECTORY")
            .ok()
            .and_then(|dirs| dirs.split(':').next().map(PathBuf::from))
            .unwrap_or_else(std::env::temp_dir);
        let path = directory.join(format!("nm-secret-{}", uuid::Uuid::new_v4().simple()));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .context("could not create the Wi-Fi secret file")?;
        let secret = Self(path);
        file.write_all(contents.as_bytes())?;
        Ok(secret)
    }

    fn path(&self) -> Result<&str> {
        self.0.to_str().context("secret file path is not UTF-8")
    }
}

impl Drop for SecretFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// One nmcli `passwd-file` line. nmcli strips unescaped leading and trailing
/// white space and decodes backslash escapes, so `\` and spaces are escaped.
/// [`validate`] already limits the password to printable ASCII.
fn passwd_line(password: &str) -> Secret<String> {
    // Sized up front so the buffer never reallocates and leaves a copy behind.
    let mut line = String::with_capacity(32 + 2 * password.len());
    line.push_str("802-11-wireless-security.psk:");
    for c in password.chars() {
        match c {
            '\\' => line.push_str("\\\\"),
            ' ' => line.push_str("\\ "),
            c => line.push(c),
        }
    }
    line.push('\n');
    Secret::new(line)
}

/// Terse (`-g`) nmcli output escapes `\` and `:`.
fn terse(value: &str) -> Secret<String> {
    Secret::new(value.replace('\\', "\\\\").replace(':', "\\:"))
}

/// `Connection 'robot-buddy-home' (<uuid>) successfully added.` (C locale).
fn added_uuid(stdout: &str) -> Result<String> {
    let uuid = stdout
        .rsplit_once(") successfully added")
        .and_then(|(head, _)| head.rsplit_once(" ("))
        .map(|(_, uuid)| uuid)
        .context("nmcli did not report the new profile")?;
    Ok(uuid::Uuid::parse_str(uuid)
        .context("nmcli reported an invalid profile UUID")?
        .hyphenated()
        .to_string())
}

async fn join_home(interface: &str, f: &WifiForm) -> Result<()> {
    let password = f.password.expose();
    validate(&f.ssid, password)?;
    ensure!(valid_interface(interface), "invalid interface");
    // Staged beside the current home profile, without autoconnect, so it never
    // competes for the radio until it has proved itself.
    let added = nm(&[
        "connection",
        "add",
        "type",
        "wifi",
        "con-name",
        HOME_ID,
        "ifname",
        interface,
        "autoconnect",
        "no",
        "ssid",
        &f.ssid,
        "wifi-sec.key-mgmt",
        "wpa-psk",
        "ipv4.method",
        "auto",
        "ipv6.method",
        "disabled",
    ])
    .await?;
    let candidate = added_uuid(&added)?;
    if let Err(error) = prove(&candidate, password).await {
        // The previous home profile is untouched and keeps its autoconnect.
        if let Err(cleanup) = delete_uuid(&candidate).await {
            tracing::warn!(error = %cleanup, "could not remove the failed home profile");
        }
        return Err(error);
    }
    retire_others(&candidate).await;
    Ok(())
}

/// Join with the candidate, require `NetworkManager` to have saved the password
/// (otherwise the robot would be lost after a restart), then let it autoconnect.
async fn prove(candidate: &str, password: &str) -> Result<()> {
    let secret = SecretFile::create(passwd_line(password).expose())?;
    let up = nm_secret(&[
        "--wait",
        "30",
        "connection",
        "up",
        "uuid",
        candidate,
        "passwd-file",
        secret.path()?,
    ])
    .await;
    drop(secret);
    up?;
    let stored = nm_secret(&[
        "-s",
        "-g",
        "802-11-wireless-security.psk",
        "connection",
        "show",
        "uuid",
        candidate,
    ])
    .await?;
    let stored = stored.expose();
    let stored = stored.strip_suffix('\n').unwrap_or(stored);
    ensure!(
        auth::ct_eq(stored.as_bytes(), terse(password).expose().as_bytes()),
        "NetworkManager did not save the home Wi-Fi password"
    );
    nm(&[
        "connection",
        "modify",
        "uuid",
        candidate,
        "connection.autoconnect",
        "yes",
    ])
    .await?;
    Ok(())
}

async fn delete_uuid(uuid: &str) -> Result<()> {
    let output = run(&["connection", "delete", "uuid", uuid]).await?;
    match output.status.code() {
        Some(0 | NOT_FOUND) => Ok(()),
        _ => Err(failure(&output)),
    }
}

/// Remove every other home profile: the one the candidate replaces, copies
/// from older releases, and candidates a crash left without autoconnect. A
/// failure here leaves a spare profile behind, never the robot offline.
async fn retire_others(candidate: &str) {
    let listed = run(&["-g", "connection.uuid", "connection", "show", "id", HOME_ID]).await;
    let uuids = match listed {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| uuid::Uuid::parse_str(line.trim()).ok())
            .map(|uuid| uuid.hyphenated().to_string())
            .collect::<Vec<_>>(),
        Ok(output) => {
            tracing::warn!(error = %failure(&output), "could not list old home profiles");
            return;
        }
        Err(error) => {
            tracing::warn!(%error, "could not list old home profiles");
            return;
        }
    };
    for uuid in uuids
        .iter()
        .filter(|uuid| *uuid != candidate)
        .take(MAX_COPIES)
    {
        if let Err(error) = delete_uuid(uuid).await {
            tracing::warn!(%error, "could not remove an old home profile");
        }
    }
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

/// SSID: 1–32 bytes without control characters, not starting with `-` (nmcli
/// could read it as an option). Password: a WPA passphrase of 8–63 printable
/// ASCII characters.
fn validate(ssid: &str, password: &str) -> Result<()> {
    ensure!(
        !ssid.is_empty()
            && ssid.len() <= 32
            && !ssid.starts_with('-')
            && !ssid.chars().any(char::is_control),
        "invalid SSID"
    );
    ensure!(
        (8..=63).contains(&password.len()) && password.chars().all(|c| (' '..='~').contains(&c)),
        "invalid password"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    /// A stand-in `nmcli` that records its argv and the passwd-file it was given,
    /// and keeps profiles as `p/<uuid>.name` and `p/<uuid>.auto`. One test at a time.
    pub(super) fn fake_nmcli() -> &'static Path {
        static SCRIPT: OnceLock<PathBuf> = OnceLock::new();
        SCRIPT.get_or_init(|| {
            let dir = std::env::temp_dir().join(format!("buddy-nmcli-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            let script = dir.join("nmcli");
            std::fs::write(&script, FAKE_NMCLI).unwrap();
            std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o700))
                .unwrap();
            script
        })
    }

    const FAKE_NMCLI: &str = r#"#!/bin/sh
dir=$(dirname "$0")
printf '%s\n' "$*" >> "$dir/log"
mkdir -p "$dir/p"
prev=""; con=""; id=""; uuid=""; auto=""; secret=""
for a in "$@"; do
  case "$prev" in
    passwd-file)
      if [ -n "$(find "$a" -perm 600)" ]; then echo 600 > "$dir/mode"; else echo other > "$dir/mode"; fi
      cat "$a" > "$dir/secret"; echo "$a" > "$dir/secret-path"; secret=$a;;
    con-name) con=$a;;
    id) id=$a;;
    uuid) uuid=$a;;
    autoconnect|connection.autoconnect) auto=$a;;
  esac
  prev=$a
done
named() { for f in "$dir"/p/*.name; do [ -e "$f" ] && [ "$(cat "$f")" = "$1" ] && basename "$f" .name; done; }
missing() { echo "Error: unknown connection '$1'." >&2; exit 10; }
case " $* " in
  *" connection add "*)
    n=$(( $(cat "$dir/next" 2>/dev/null || echo 0) + 1 )); echo $n > "$dir/next"
    u=$(printf '00000000-0000-4000-8000-%012d' "$n")
    echo "$con" > "$dir/p/$u.name"; echo "$auto" > "$dir/p/$u.auto"
    echo "Connection '$con' ($u) successfully added.";;
  *" connection delete "*)
    if [ -n "$uuid" ]; then u=$uuid; else u=$(named "$id" | head -n 1); fi
    [ -n "$u" ] && [ -e "$dir/p/$u.name" ] || missing "$uuid$id"
    rm "$dir/p/$u.name" "$dir/p/$u.auto";;
  *" connection up "*)
    if [ -e "$dir/fail-up" ]; then
      [ -n "$secret" ] && cat "$secret" >&2
      echo "Error: Connection activation failed: Secrets were required" >&2; exit 4
    fi;;
  *" connection modify "*) [ -e "$dir/p/$uuid.name" ] || missing "$uuid"; echo "$auto" > "$dir/p/$uuid.auto";;
  *" connection show "*)
    if [ -n "$uuid" ]; then [ -e "$dir/p/$uuid.name" ] || missing "$uuid"; cat "$dir/stored" 2>/dev/null
    else us=$(named "$id"); [ -n "$us" ] || missing "$id"; echo "$us"; fi;;
  *" general "*) cat "$dir/state";;
esac
exit 0
"#;

    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    struct Fake(PathBuf);

    impl Fake {
        /// Clean state; hold the guard for the whole test.
        fn reset() -> Self {
            let dir = fake_nmcli().parent().unwrap().to_owned();
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    std::fs::remove_dir_all(path).unwrap();
                } else if path.file_name().unwrap() != "nmcli" {
                    std::fs::remove_file(path).unwrap();
                }
            }
            std::fs::create_dir(dir.join("p")).unwrap();
            Self(dir)
        }

        fn set(&self, name: &str, value: &str) {
            std::fs::write(self.0.join(name), value).unwrap();
        }

        fn read(&self, name: &str) -> String {
            std::fs::read_to_string(self.0.join(name)).unwrap_or_default()
        }

        fn log(&self) -> Vec<String> {
            self.read("log").lines().map(str::to_owned).collect()
        }

        /// A profile that exists before the test, like a working home network.
        fn profile(&self, uuid: &str, name: &str, autoconnect: &str) {
            self.set(&format!("p/{uuid}.name"), &format!("{name}\n"));
            self.set(&format!("p/{uuid}.auto"), &format!("{autoconnect}\n"));
        }

        /// `(uuid, autoconnect)` of every profile named `name`, sorted.
        fn profiles(&self, name: &str) -> Vec<(String, String)> {
            let mut found: Vec<_> = std::fs::read_dir(self.0.join("p"))
                .unwrap()
                .filter_map(|entry| {
                    let path = entry.unwrap().path();
                    let uuid = path
                        .file_name()?
                        .to_str()?
                        .strip_suffix(".name")?
                        .to_owned();
                    (std::fs::read_to_string(&path).ok()?.trim() == name).then(|| {
                        let auto = self.read(&format!("p/{uuid}.auto")).trim().to_owned();
                        (uuid, auto)
                    })
                })
                .collect();
            found.sort();
            found
        }
    }

    const OLD_HOME: &str = "aaaaaaaa-0000-4000-8000-000000000001";
    const CANDIDATE: &str = "00000000-0000-4000-8000-000000000001";

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
        assert!(validate("-home", "password123").is_err(), "nmcli option");
        assert!(validate("Café Wi-Fi", "correct horse 9").is_ok());
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
            password: Secret::new("password123".into()),
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
    fn only_a_global_route_counts_as_connected() {
        assert!(uplink("connected\n"));
        for state in [
            "connected (local only)",
            "connected (site only)",
            "connecting",
            "disconnected",
            "asleep",
            "",
        ] {
            assert!(!uplink(state), "{state}");
        }
    }

    #[tokio::test]
    async fn connected_reads_the_general_state() {
        let _serial = SERIAL.lock().await;
        let fake = Fake::reset();
        fake.set("state", "connected\n");
        assert!(connected().await.unwrap());
        fake.set("state", "connected (local only)\n");
        assert!(!connected().await.unwrap());
        assert_eq!(fake.log(), ["-t -f STATE general", "-t -f STATE general"]);
    }

    #[test]
    fn passwords_are_escaped_for_passwd_file_and_terse_output() {
        assert_eq!(
            passwd_line(" a\\b c ").expose(),
            "802-11-wireless-security.psk:\\ a\\\\b\\ c\\ \n"
        );
        assert_eq!(
            passwd_line("p:a#s;s").expose(),
            "802-11-wireless-security.psk:p:a#s;s\n"
        );
        assert_eq!(terse("a:b\\c").expose(), "a\\:b\\\\c");
    }

    #[test]
    fn the_new_profile_uuid_comes_from_nmcli() {
        assert_eq!(
            added_uuid(
                "Connection 'robot-buddy-home' (5D3F2C9A-1B2E-4C3D-8E4F-5A6B7C8D9E0F) successfully added.\n"
            )
            .unwrap(),
            "5d3f2c9a-1b2e-4c3d-8e4f-5a6b7c8d9e0f"
        );
        assert!(added_uuid("Connection 'x' (--help) successfully added.").is_err());
        assert!(added_uuid("").is_err());
    }

    fn home(password: &str) -> WifiForm {
        WifiForm {
            csrf: String::new(),
            ssid: "My Net".into(),
            password: Secret::new(password.into()),
        }
    }

    #[tokio::test]
    async fn joining_home_proves_the_new_profile_before_retiring_the_old() {
        let _serial = SERIAL.lock().await;
        let fake = Fake::reset();
        fake.profile(OLD_HOME, HOME_ID, "yes");
        let password = "correct horse\\9";
        fake.set("stored", &format!("{}\n", terse(password).expose()));
        join_home("wlan0", &home(password)).await.unwrap();

        let log = fake.log();
        assert!(
            log[0].starts_with("connection add type wifi con-name robot-buddy-home ifname wlan0")
        );
        assert!(log[0].contains("autoconnect no ssid My Net wifi-sec.key-mgmt wpa-psk"));
        assert!(log[1].starts_with(&format!(
            "--wait 30 connection up uuid {CANDIDATE} passwd-file "
        )));
        assert_eq!(
            log[2],
            format!("-s -g 802-11-wireless-security.psk connection show uuid {CANDIDATE}")
        );
        assert_eq!(
            log[3],
            format!("connection modify uuid {CANDIDATE} connection.autoconnect yes")
        );
        assert_eq!(
            log[4],
            "-g connection.uuid connection show id robot-buddy-home"
        );
        // The old profile goes only after the new one joined and saved its password.
        assert_eq!(log[5], format!("connection delete uuid {OLD_HOME}"));
        assert_eq!(log.len(), 6, "{log:?}");
        assert!(log.iter().all(|line| !line.contains("horse")), "{log:?}");
        assert_eq!(
            fake.profiles(HOME_ID),
            [(CANDIDATE.to_owned(), "yes".to_owned())]
        );
        assert_eq!(fake.read("mode"), "600\n");
        assert_eq!(fake.read("secret"), *passwd_line(password).expose());
        let secret = PathBuf::from(fake.read("secret-path").trim());
        assert!(!secret.exists(), "the secret file is removed");
    }

    /// Regression: the old home profile was deleted before its replacement was
    /// tried, so one mistyped password left the robot with no way home.
    #[tokio::test]
    async fn a_mistyped_password_keeps_the_working_home_network() {
        let _serial = SERIAL.lock().await;
        let fake = Fake::reset();
        fake.profile(OLD_HOME, HOME_ID, "yes");
        fake.set("fail-up", "");
        let error = join_home("wlan0", &home("mistyped-password"))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("activation failed"), "{error}");
        // nmcli's stderr from a secret-bearing call never reaches the log.
        assert!(!error.contains("mistyped"), "{error}");
        assert!(!error.contains("Secrets were required"), "{error}");
        let log = fake.log();
        assert_eq!(
            log.last().unwrap(),
            &format!("connection delete uuid {CANDIDATE}")
        );
        assert!(log.iter().all(|line| !line.contains(OLD_HOME)), "{log:?}");
        assert_eq!(
            fake.profiles(HOME_ID),
            [(OLD_HOME.to_owned(), "yes".to_owned())]
        );
        let secret = PathBuf::from(fake.read("secret-path").trim());
        assert!(!secret.exists());
    }

    /// A join whose password was not saved would be lost at the next boot:
    /// it fails, and the old profile stays.
    #[tokio::test]
    async fn an_unsaved_password_fails_the_join_and_keeps_the_old_profile() {
        let _serial = SERIAL.lock().await;
        let fake = Fake::reset();
        fake.profile(OLD_HOME, HOME_ID, "yes");
        fake.set("stored", "\n");
        let error = join_home("wlan0", &home("password123")).await.unwrap_err();
        assert!(error.to_string().contains("did not save"), "{error}");
        assert_eq!(
            fake.profiles(HOME_ID),
            [(OLD_HOME.to_owned(), "yes".to_owned())]
        );
        assert!(fake.log().iter().all(|l| !l.contains("autoconnect yes")));
    }

    /// Copies from older releases and candidates a crash left behind (never
    /// autoconnecting) are cleared once a new network is proven.
    #[tokio::test]
    async fn a_proven_join_clears_stale_home_profiles() {
        let _serial = SERIAL.lock().await;
        let fake = Fake::reset();
        fake.profile(OLD_HOME, HOME_ID, "yes");
        fake.profile("bbbbbbbb-0000-4000-8000-000000000002", HOME_ID, "yes");
        fake.profile("cccccccc-0000-4000-8000-000000000003", HOME_ID, "no");
        fake.profile("dddddddd-0000-4000-8000-000000000004", SETUP_ID, "no");
        fake.set("stored", "password123\n");
        join_home("wlan0", &home("password123")).await.unwrap();
        assert_eq!(
            fake.profiles(HOME_ID),
            [(CANDIDATE.to_owned(), "yes".to_owned())]
        );
        assert_eq!(fake.profiles(SETUP_ID).len(), 1, "other profiles are kept");
    }

    #[tokio::test]
    async fn the_hotspot_is_deleted_when_setup_is_abandoned() {
        let _serial = SERIAL.lock().await;
        let fake = Fake::reset();
        fake.profile("dddddddd-0000-4000-8000-000000000004", SETUP_ID, "no");
        let hotspot = Hotspot::start("wlan0", "RobotBuddy-x", "hotspotpass")
            .await
            .unwrap();
        let log = fake.log();
        assert_eq!(log[0], "connection delete id robot-buddy-setup");
        assert_eq!(log[1], "connection delete id robot-buddy-setup");
        assert!(log[2].starts_with("connection add save no type wifi con-name robot-buddy-setup"));
        assert_eq!(log[3], "connection up id robot-buddy-setup");
        assert_eq!(fake.profiles(SETUP_ID).len(), 1);
        // SIGTERM or an error drops the setup future with the hotspot up.
        drop(hotspot);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !fake.profiles(SETUP_ID).is_empty() {
            assert!(std::time::Instant::now() < deadline, "{:?}", fake.log());
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // A clean removal does not leave a second, detached delete behind.
        let hotspot = Hotspot::start("wlan0", "RobotBuddy-y", "hotspotpass")
            .await
            .unwrap();
        hotspot.remove().await.unwrap();
        let before = fake.log().len();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(fake.log().len(), before);
    }

    #[tokio::test]
    async fn the_setup_bind_waits_for_its_address() {
        let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = held.local_addr().unwrap();
        let release = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            drop(held);
        });
        let listener = bind_retrying(address, 250, Duration::from_millis(20)).await;
        assert_eq!(listener.unwrap().local_addr().unwrap(), address);
        release.await.unwrap();

        let busy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = busy.local_addr().unwrap();
        let error = bind_retrying(address, 2, Duration::from_millis(10))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("cannot listen"), "{error}");
    }
}

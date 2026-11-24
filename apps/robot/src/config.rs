//! Process settings, read once at startup.
use crate::audio::PlayerConfig;
use anyhow::{Context, Result, ensure};
use robot_buddy::auth::{Identity, Secret};
use std::{
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

/// An identity file is a certificate and a key: a few KiB at most.
const MAX_IDENTITY: u64 = 64 * 1024;

/// A server or engine URL the robot may send credentials to: HTTPS (or WSS),
/// without credentials or a fragment. Plain HTTP (or WS) only to
/// `localhost`/`127.0.0.1` and only when `insecure_local` (`--simulate`),
/// mirroring the server's `BUDDY_ALLOW_INSECURE_LOCAL` rule.
pub fn safe_url(raw: &str, insecure_local: bool) -> Result<url::Url> {
    let url = url::Url::parse(raw)?;
    ensure!(
        url.username().is_empty() && url.password().is_none() && url.fragment().is_none(),
        "URL contains credentials or a fragment"
    );
    let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1"));
    let plain = matches!(url.scheme(), "http" | "ws");
    ensure!(
        matches!(url.scheme(), "https" | "wss") || (plain && insecure_local && local),
        "HTTPS required for robot server"
    );
    Ok(url)
}

/// Read the device identity. Refuses a file anyone but its owner can read or
/// write (it holds the private key), and keeps the bytes in a buffer that is
/// cleared on drop.
pub fn read_identity(path: &Path) -> Result<Identity> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("identity file {}", path.display()))?;
    let meta = file.metadata()?;
    ensure!(meta.is_file(), "identity file is not a regular file");
    // Group or other may not read, write or run it.
    let shared = meta.mode() & 0o077;
    ensure!(
        shared == 0,
        "identity file must be mode 0600 (it is {:o})",
        meta.mode() & 0o777
    );
    ensure!(meta.len() <= MAX_IDENTITY, "identity file too large");
    // Sized up front so the buffer does not reallocate and leave a copy
    // behind, and wrapped before any error can drop it uncleared.
    let mut buffer = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or(0) + 1);
    let read = (&mut file).take(MAX_IDENTITY + 1).read_to_end(&mut buffer);
    let bytes = Secret::new(buffer);
    read?;
    ensure!(
        bytes.as_ref().len() <= usize::try_from(MAX_IDENTITY).unwrap_or(usize::MAX),
        "identity file too large"
    );
    Ok(serde_json::from_slice(bytes.as_ref())?)
}

/// How much of each conversation event reaches stdout (and so the journal).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventDetail {
    /// Sequence, turn ID and kind only. No user or assistant text.
    Compact,
    /// The full event JSON, for `--simulate` and explicit debugging.
    Full,
}

pub struct Settings {
    pub simulate: bool,
    pub server: String,
    pub identity_file: PathBuf,
    pub cache_file: PathBuf,
    pub events: EventDetail,
    pub speech: bool,
    pub player: PlayerConfig,
    pub face_serial: Option<PathBuf>,
    /// How long Wi-Fi must stay down before the setup hotspot comes back.
    pub wifi_grace: Duration,
}

impl Settings {
    pub fn from_env() -> Result<Self> {
        let args = std::env::args().collect::<Vec<_>>();
        Self::from_lookup(&args, |key| std::env::var(key).ok())
    }

    pub fn from_lookup(args: &[String], get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let simulate = args.iter().any(|a| a == "--simulate");
        let server = get("BUDDY_SERVER_URL").context("BUDDY_SERVER_URL required")?;
        let url = safe_url(&server, simulate)?;
        ensure!(
            matches!(url.scheme(), "https" | "http"),
            "BUDDY_SERVER_URL must be an HTTP(S) URL"
        );
        // On hardware stdout is the journal, which never carries conversation text.
        let events = match (get("BUDDY_EVENTS").as_deref(), simulate) {
            (Some("full" | "") | None, true) => EventDetail::Full,
            (Some("compact"), _) | (None | Some(""), false) => EventDetail::Compact,
            (Some("full"), false) => anyhow::bail!("BUDDY_EVENTS=full requires --simulate"),
            (Some(_), _) => anyhow::bail!("BUDDY_EVENTS must be full or compact"),
        };
        let wifi_grace = match get("BUDDY_WIFI_GRACE_SECS") {
            Some(v) => Duration::from_secs(v.parse().context("BUDDY_WIFI_GRACE_SECS")?),
            None => Duration::from_mins(5),
        };
        Ok(Self {
            simulate,
            server,
            identity_file: get("BUDDY_IDENTITY_FILE")
                .context("BUDDY_IDENTITY_FILE required")?
                .into(),
            cache_file: get("BUDDY_CACHE_FILE")
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| crate::cache::DEFAULT_CACHE_FILE.into())
                .into(),
            events,
            speech: get("BUDDY_SPEECH").as_deref() == Some("true"),
            player: PlayerConfig::from_lookup(&get)?,
            face_serial: get("BUDDY_FACE_SERIAL")
                .filter(|v| !v.is_empty())
                .map(Into::into),
            wifi_grace,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn settings(args: &[&str], vars: &[(&str, &str)]) -> Result<Settings> {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        let args = args.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>();
        Settings::from_lookup(&args, |k| vars.get(k).cloned())
    }

    const BASE: [(&str, &str); 2] = [
        ("BUDDY_SERVER_URL", "https://buddy.example"),
        ("BUDDY_IDENTITY_FILE", "/etc/robot-buddy/id.json"),
    ];

    #[test]
    fn hardware_mode_keeps_the_journal_compact() -> Result<()> {
        let s = settings(&["robot"], &BASE)?;
        assert_eq!(s.events, EventDetail::Compact);
        assert!(!s.speech);
        assert!(s.face_serial.is_none());
        let s = settings(&["robot", "--simulate"], &BASE)?;
        assert_eq!(s.events, EventDetail::Full);
        let mut vars = BASE.to_vec();
        vars.push(("BUDDY_FACE_SERIAL", "/dev/ttyACM0"));
        let s = settings(&["robot"], &vars)?;
        assert_eq!(s.face_serial.as_deref(), Some("/dev/ttyACM0".as_ref()));
        // Full event lines (with conversation text) never reach the journal.
        vars.push(("BUDDY_EVENTS", "full"));
        assert!(settings(&["robot"], &vars).is_err());
        let s = settings(&["robot", "--simulate"], &vars)?;
        assert_eq!(s.events, EventDetail::Full);
        Ok(())
    }

    #[test]
    fn engine_endpoints_need_tls_unless_local_and_opted_in() {
        for good in ["https://engine.example", "wss://engine.example/gw"] {
            assert!(safe_url(good, false).is_ok(), "{good}");
        }
        for local in ["http://127.0.0.1:6420", "ws://localhost:6420"] {
            assert!(safe_url(local, false).is_err(), "{local} without opt-in");
            assert!(safe_url(local, true).is_ok(), "{local}");
        }
        for bad in [
            "http://engine.example",
            "http://10.0.0.5:6420",
            "https://user:pw@engine.example",
            "https://engine.example/#frag",
            "ftp://engine.example",
            "not a url",
        ] {
            assert!(safe_url(bad, true).is_err(), "{bad}");
        }
    }

    fn identity_file(mode: u32) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!("buddy-id-{}.json", uuid::Uuid::new_v4()));
        let identity = br#"{"certificate":{"device_id":"d","public_key":"k",
            "issuer_signature":"s"},"private_key":"p"}"#;
        std::fs::write(&path, identity).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    #[test]
    fn the_identity_file_must_be_private() {
        let private = identity_file(0o600);
        assert!(read_identity(&private).is_ok());
        for mode in [0o640, 0o644, 0o604, 0o620] {
            let open = identity_file(mode);
            let Err(refused) = read_identity(&open) else {
                panic!("{mode:o} was accepted");
            };
            let refused = refused.to_string();
            assert!(refused.contains("0600"), "{mode:o}: {refused}");
            std::fs::remove_file(open).unwrap();
        }
        std::fs::remove_file(private).unwrap();
    }

    #[test]
    fn rejects_plain_http_outside_local_simulation() {
        let vars = [
            ("BUDDY_SERVER_URL", "http://buddy.example"),
            ("BUDDY_IDENTITY_FILE", "/x"),
        ];
        assert!(settings(&["robot"], &vars).is_err());
        let vars = [
            ("BUDDY_SERVER_URL", "http://127.0.0.1:8080"),
            ("BUDDY_IDENTITY_FILE", "/x"),
        ];
        assert!(settings(&["robot"], &vars).is_err());
        assert!(settings(&["robot", "--simulate"], &vars).is_ok());
        let mut bad = BASE.to_vec();
        bad.push(("BUDDY_EVENTS", "verbose"));
        assert!(settings(&["robot"], &bad).is_err());
    }
}

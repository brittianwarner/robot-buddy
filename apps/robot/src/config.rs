//! Process settings, read once at startup.
use crate::audio::PlayerConfig;
use anyhow::{Context, Result, ensure};
use std::{path::PathBuf, time::Duration};

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
        let url = url::Url::parse(&server)?;
        let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1"));
        ensure!(
            url.scheme() == "https" || (simulate && local),
            "HTTPS required for robot server"
        );
        let events = match get("BUDDY_EVENTS").as_deref() {
            Some("full") => EventDetail::Full,
            Some("compact") => EventDetail::Compact,
            None | Some("") if simulate => EventDetail::Full,
            None | Some("") => EventDetail::Compact,
            Some(_) => anyhow::bail!("BUDDY_EVENTS must be full or compact"),
        };
        let wifi_grace = match get("BUDDY_WIFI_GRACE_SECS") {
            Some(v) => Duration::from_secs(v.parse().context("BUDDY_WIFI_GRACE_SECS")?),
            None => Duration::from_secs(300),
        };
        Ok(Self {
            simulate,
            server,
            identity_file: get("BUDDY_IDENTITY_FILE")
                .context("BUDDY_IDENTITY_FILE required")?
                .into(),
            cache_file: get("BUDDY_CACHE_FILE")
                .unwrap_or_else(|| "/var/lib/robot-buddy/cache.json".into())
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
        vars.push(("BUDDY_EVENTS", "full"));
        vars.push(("BUDDY_FACE_SERIAL", "/dev/ttyACM0"));
        let s = settings(&["robot"], &vars)?;
        assert_eq!(s.events, EventDetail::Full);
        assert_eq!(s.face_serial.as_deref(), Some("/dev/ttyACM0".as_ref()));
        Ok(())
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

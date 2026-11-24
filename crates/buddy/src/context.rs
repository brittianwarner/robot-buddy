//! Robot-local settings and server-clock context. Location labels never imply GPS access.
#[cfg(feature = "server")]
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
#[cfg(feature = "server")]
use serde_json::{Value, json};
/// Tolerates unknown fields on read; writes decode through `protocol::strict`.
#[derive(Clone, Serialize, Deserialize)]
pub struct ContextConfig {
    pub timezone: String,
    pub location: Option<String>,
}
impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            timezone: "UTC".into(),
            location: None,
        }
    }
}
/// Timezone checks and local time need the timezone database, which only the server links.
#[cfg(feature = "server")]
impl ContextConfig {
    pub fn validate(&self) -> Result<()> {
        self.timezone
            .parse::<chrono_tz::Tz>()
            .context("use a valid IANA timezone, for example America/New_York")?;
        ensure!(
            self.location.as_ref().is_none_or(|v| !v.trim().is_empty()
                && v.len() <= 200
                && !v.chars().any(char::is_control)),
            "location must be a plain label of 1-200 bytes"
        );
        Ok(())
    }
    pub fn at(&self, timestamp: i64) -> Result<Value> {
        self.validate()?;
        let utc = chrono::DateTime::from_timestamp(timestamp, 0).context("invalid timestamp")?;
        let local = utc.with_timezone(&self.timezone.parse::<chrono_tz::Tz>()?);
        let location_source = if self.location.is_some() {
            "saved_setting"
        } else {
            "unknown"
        };
        Ok(json!({
            "utc": utc.to_rfc3339(),
            "local_datetime": local.to_rfc3339(),
            "local_date": local.format("%Y-%m-%d").to_string(),
            "weekday": local.format("%A").to_string(),
            "timezone": self.timezone,
            "location_label": self.location,
            "clock_source": "server",
            "location_source": location_source,
        }))
    }
}
#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;
    #[test]
    fn local_time_handles_dst_and_date_boundary() {
        let config = ContextConfig {
            timezone: "America/New_York".into(),
            location: None,
        };
        let at = |s: &str| {
            config
                .at(chrono::DateTime::parse_from_rfc3339(s).unwrap().timestamp())
                .unwrap()
        };
        assert_eq!(
            at("2026-03-08T06:59:00Z")["local_datetime"],
            "2026-03-08T01:59:00-05:00"
        );
        assert_eq!(
            at("2026-03-08T07:00:00Z")["local_datetime"],
            "2026-03-08T03:00:00-04:00"
        );
        assert_eq!(at("2026-11-13T02:00:00Z")["local_date"], "2026-11-12");
    }
    #[test]
    fn rejects_unknown_timezone() {
        assert!(
            ContextConfig {
                timezone: "Mars/Olympus".into(),
                location: None
            }
            .validate()
            .is_err()
        );
    }
}

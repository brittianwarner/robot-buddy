//! Event lines on stdout. In hardware mode stdout is the systemd journal, so
//! the compact form carries no user or assistant text.
use crate::config::EventDetail;
use robot_buddy::protocol::OutputEvent;
use std::io::Write;

/// The stdout line for `event`, or `None` when compact mode skips it.
/// Compact lines carry identifiers only, and skip `text.delta` entirely: a
/// streamed answer is many events and none of them is worth a journal line.
pub fn line(event: &OutputEvent, detail: EventDetail) -> Option<String> {
    match detail {
        EventDetail::Full => Some(serde_json::to_string(event).unwrap_or_default()),
        EventDetail::Compact if event.kind == "text.delta" => None,
        EventDetail::Compact => Some(format!(
            "event sequence={} turn={} kind={}",
            event.sequence,
            safe(&event.turn_id),
            safe(&event.kind)
        )),
    }
}

/// Identifiers only; anything unexpected is replaced rather than printed.
fn safe(value: &str) -> &str {
    let ok = value.len() <= 128
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
    if ok { value } else { "?" }
}

pub fn print(event: &OutputEvent, detail: EventDetail) {
    if cfg!(test) {
        return;
    }
    let Some(line) = line(event, detail) else {
        return;
    };
    let mut out = std::io::stdout().lock();
    // A closed stdout must not take the robot down.
    let _ = writeln!(out, "{line}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn compact_lines_carry_no_conversation_text() {
        const PROMPT: &str = "SENTINEL-PROMPT-7f3a my secret question";
        let payloads = [
            json!({"text": PROMPT}),
            json!({"delta": PROMPT}),
            json!({"answer": PROMPT, "text": PROMPT}),
            json!({"index": 0, "text": PROMPT}),
        ];
        let kinds = [
            "turn.started",
            "text.delta",
            "turn.completed",
            "speech.ready",
            "turn.failed",
        ];
        for kind in kinds {
            for payload in &payloads {
                let event = OutputEvent {
                    sequence: 42,
                    turn_id: "0193-abc".into(),
                    kind: kind.into(),
                    payload: payload.clone(),
                };
                let compact = line(&event, EventDetail::Compact);
                assert!(!compact.unwrap_or_default().contains("SENTINEL"), "{kind}");
            }
        }
        let event = OutputEvent {
            sequence: 42,
            turn_id: "0193-abc".into(),
            kind: "turn.started".into(),
            payload: json!({"text": PROMPT}),
        };
        assert_eq!(
            line(&event, EventDetail::Compact).as_deref(),
            Some("event sequence=42 turn=0193-abc kind=turn.started")
        );
        let delta = OutputEvent {
            kind: "text.delta".into(),
            ..event.clone()
        };
        assert_eq!(
            line(&delta, EventDetail::Compact),
            None,
            "deltas are skipped"
        );
        let odd = OutputEvent {
            turn_id: "a b\nc".into(),
            ..event
        };
        assert!(line(&odd, EventDetail::Compact).unwrap().contains("turn=?"));
    }
}

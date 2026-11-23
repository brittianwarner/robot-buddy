//! Event lines on stdout. In hardware mode stdout is the systemd journal, so
//! the compact form carries no user or assistant text.
use crate::config::EventDetail;
use robot_buddy::protocol::OutputEvent;
use std::io::Write;

pub fn line(event: &OutputEvent, detail: EventDetail) -> String {
    match detail {
        EventDetail::Full => serde_json::to_string(event).unwrap_or_default(),
        EventDetail::Compact => format!(
            "event sequence={} turn={} kind={}",
            event.sequence,
            safe(&event.turn_id),
            safe(&event.kind)
        ),
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
    let mut out = std::io::stdout().lock();
    // A closed stdout must not take the robot down.
    let _ = writeln!(out, "{}", line(event, detail));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn compact_lines_carry_no_conversation_text() {
        let event = OutputEvent {
            sequence: 42,
            turn_id: "0193-abc".into(),
            kind: "turn.started".into(),
            payload: json!({"text": "my secret question"}),
        };
        let compact = line(&event, EventDetail::Compact);
        assert_eq!(compact, "event sequence=42 turn=0193-abc kind=turn.started");
        assert!(line(&event, EventDetail::Full).contains("my secret question"));
        let odd = OutputEvent {
            turn_id: "a b\nc".into(),
            ..event
        };
        assert!(line(&odd, EventDetail::Compact).contains("turn=?"));
    }
}

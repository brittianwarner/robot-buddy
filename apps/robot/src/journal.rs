//! Event lines on stdout. In hardware mode stdout is the systemd journal, so
//! the compact form carries no user or assistant text.
//!
//! The device loop never writes stdout itself. It hands each line to a
//! bounded queue drained by one OS thread, so a stalled reader (journald
//! paused, a terminal held with Ctrl-S) cannot hold up "Hey Pip". A full
//! queue drops the line and counts it; the writer reports the count as
//! `journal dropped=<n>` before its next line.
use crate::config::EventDetail;
use robot_buddy::protocol::OutputEvent;
use std::{
    io::Write,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, SyncSender, TrySendError},
    },
};

/// Lines queued for the writer at most. A turn's compact lines are a handful;
/// this covers a long replay page without dropping while stdout keeps up.
const QUEUE: usize = 256;

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

/// Event lines for the journal. Clones share one queue and one writer.
#[derive(Clone)]
pub struct Journal {
    detail: EventDetail,
    queue: SyncSender<String>,
    /// Lines dropped since the writer last reported them.
    dropped: Arc<AtomicU64>,
}

impl Journal {
    /// Lines go to stdout.
    pub fn stdout(detail: EventDetail) -> std::io::Result<Self> {
        Self::to(detail, std::io::stdout())
    }

    /// Lines go to `out`, written by a dedicated thread.
    pub fn to(detail: EventDetail, mut out: impl Write + Send + 'static) -> std::io::Result<Self> {
        let (queue, lines) = mpsc::sync_channel::<String>(QUEUE);
        let dropped = Arc::new(AtomicU64::new(0));
        let lost = dropped.clone();
        std::thread::Builder::new()
            .name("journal".into())
            .spawn(move || {
                // Ends when every `Journal` is gone. A closed stdout must not
                // take the robot down, so write errors are ignored.
                let report = |out: &mut dyn Write| {
                    let missed = lost.swap(0, Ordering::Relaxed);
                    if missed > 0 {
                        let _ = writeln!(out, "journal dropped={missed}");
                    }
                };
                for line in lines {
                    report(&mut out);
                    let _ = writeln!(out, "{line}");
                    let _ = out.flush();
                }
                report(&mut out);
                let _ = out.flush();
            })?;
        Ok(Self {
            detail,
            queue,
            dropped,
        })
    }

    /// Queue the line for `event`. Never blocks.
    pub fn record(&self, event: &OutputEvent) {
        let Some(line) = line(event, self.detail) else {
            return;
        };
        match self.queue.try_send(line) {
            Ok(()) | Err(TrySendError::Disconnected(_)) => {}
            Err(TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{
        io::{BufRead, BufReader},
        time::Duration,
    };

    fn big(sequence: i64) -> OutputEvent {
        OutputEvent {
            sequence,
            turn_id: "t".into(),
            kind: "text.delta".into(),
            payload: json!({ "delta": "x".repeat(1024) }),
        }
    }

    /// A reader that stops reading (stdout piped to a paused process) fills
    /// the pipe and then the queue: lines are dropped and counted instead of
    /// blocking the caller, and the count is reported once it reads again.
    #[test]
    fn a_paused_reader_never_blocks_the_caller() {
        let (reader, writer) = std::io::pipe().unwrap();
        let journal = Journal::to(EventDetail::Full, writer).unwrap();
        let (done, finished) = mpsc::channel();
        let recording = journal.clone();
        std::thread::spawn(move || {
            // Far more than the pipe buffer plus the queue hold.
            for sequence in 0..2_000 {
                recording.record(&big(sequence));
            }
            done.send(()).unwrap();
        });
        // A hang detector, not a latency bound: blocking would never finish.
        finished
            .recv_timeout(Duration::from_secs(30))
            .expect("record returned while the reader was paused");
        assert!(journal.dropped.load(Ordering::Relaxed) > 0, "lines dropped");
        drop(journal);
        let lines: Vec<String> = BufReader::new(reader).lines().map(Result::unwrap).collect();
        let reported: u64 = lines
            .iter()
            .filter_map(|l| l.strip_prefix("journal dropped="))
            .map(|n| n.parse::<u64>().unwrap())
            .sum();
        let written = lines.iter().filter(|l| l.starts_with('{')).count();
        assert!(reported > 0, "the drop is reported");
        assert_eq!(
            u64::try_from(written).unwrap() + reported,
            2_000,
            "every line is either written or counted"
        );
    }

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

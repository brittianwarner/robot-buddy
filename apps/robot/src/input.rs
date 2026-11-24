//! Line-delimited control input on stdin (from the wake word and speech
//! recognition process). See docs/architecture/robot-client.md for the contract.
//!
//! The reader is a plain OS thread with blocking reads, not a Tokio task: a
//! read on an open pipe never returns, and a blocking-pool task would make
//! runtime shutdown wait for it (SIGTERM would then hang until SIGKILL).
use serde_json::Value;
use std::io::BufRead;
use tokio::sync::mpsc;

/// Longest accepted input line, in bytes, newline excluded.
pub const MAX_LINE: usize = 8 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Control {
    /// A finalized utterance. Replaces any turn in progress.
    Say(String),
    /// The wake word fired: stop talking, cancel the turn, show listening.
    Wake,
    /// Show (true) or clear (false) the listening face without an utterance.
    Listening(bool),
    /// Mute stops playback and suppresses automatic speech until unmuted.
    Mute(bool),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rejected {
    Empty,
    TooLong,
    NotUtf8,
    BadJson,
    UnknownCommand,
    BadUtterance,
}

impl Rejected {
    fn reason(self) -> &'static str {
        match self {
            Self::Empty => "empty line",
            Self::TooLong => "line longer than 8 KiB",
            Self::NotUtf8 => "line is not UTF-8",
            Self::BadJson => "line starting with { is not valid JSON",
            Self::UnknownCommand => "unknown control command",
            Self::BadUtterance => "utterance is empty or has control characters",
        }
    }
}

/// Parse one line (without its newline). Lines starting with `{` are control
/// commands; anything else is a plain utterance.
pub fn parse(line: &[u8]) -> Result<Control, Rejected> {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    if line.len() > MAX_LINE {
        return Err(Rejected::TooLong);
    }
    let line = std::str::from_utf8(line).map_err(|_| Rejected::NotUtf8)?;
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Err(Rejected::Empty);
    }
    if !trimmed.starts_with('{') {
        return utterance(trimmed).map(Control::Say);
    }
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(trimmed) else {
        return Err(Rejected::BadJson);
    };
    let mut fields = map.into_iter();
    let (Some((key, value)), None) = (fields.next(), fields.next()) else {
        return Err(Rejected::UnknownCommand);
    };
    match (key.as_str(), value) {
        ("say", Value::String(text)) => utterance(text.trim()).map(Control::Say),
        ("wake", Value::Bool(true)) => Ok(Control::Wake),
        ("listening", Value::Bool(on)) => Ok(Control::Listening(on)),
        ("mute", Value::Bool(on)) => Ok(Control::Mute(on)),
        _ => Err(Rejected::UnknownCommand),
    }
}

fn utterance(text: &str) -> Result<String, Rejected> {
    let clean = !text.is_empty()
        && text.len() <= MAX_LINE
        && !text.chars().any(|c| c.is_control() && c != '\t');
    if clean {
        Ok(text.to_owned())
    } else {
        Err(Rejected::BadUtterance)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Read {
    /// A complete line is in the buffer.
    Line,
    /// The line exceeded `max` bytes and was discarded through its newline.
    TooLong,
    Eof,
}

/// Read one line into `line`, holding at most `max` bytes of it in memory.
pub fn read_bounded<R: BufRead>(
    reader: &mut R,
    line: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<Read> {
    line.clear();
    let mut overflow = false;
    loop {
        let available = match reader.fill_buf() {
            Ok(available) => available,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if available.is_empty() {
            return Ok(match (overflow, line.is_empty()) {
                (true, _) => Read::TooLong,
                (false, true) => Read::Eof,
                (false, false) => Read::Line,
            });
        }
        let newline = available.iter().position(|b| *b == b'\n');
        let chunk = &available[..newline.unwrap_or(available.len())];
        if !overflow {
            if line.len() + chunk.len() > max {
                overflow = true;
                line.clear();
            } else {
                line.extend_from_slice(chunk);
            }
        }
        let used = newline.map_or(available.len(), |i| i + 1);
        reader.consume(used);
        if newline.is_some() {
            return Ok(if overflow { Read::TooLong } else { Read::Line });
        }
    }
}

/// Control commands from a reader thread. Once the input closes, `next` never
/// resolves again, so a closed stdin cannot spin the session loop.
pub struct Input {
    commands: mpsc::Receiver<Control>,
    open: bool,
}

impl Input {
    /// Read `reader` on a detached OS thread. The thread holds no state that
    /// needs cleanup, so process exit may end it mid-read.
    pub fn spawn<R: BufRead + Send + 'static>(mut reader: R) -> std::io::Result<Self> {
        let (tx, commands) = mpsc::channel(16);
        let reader_thread = move || {
            let mut line = Vec::with_capacity(1024);
            loop {
                let parsed = match read_bounded(&mut reader, &mut line, MAX_LINE) {
                    Ok(Read::Line) => parse(&line),
                    Ok(Read::TooLong) => Err(Rejected::TooLong),
                    Ok(Read::Eof) => break,
                    Err(error) => {
                        tracing::warn!(%error, "control input failed");
                        break;
                    }
                };
                match parsed {
                    Ok(command) => {
                        if tx.blocking_send(command).is_err() {
                            break;
                        }
                    }
                    // Never log the line itself: it may be the user's speech.
                    Err(rejected) => tracing::warn!(
                        reason = rejected.reason(),
                        bytes = line.len(),
                        "ignored control input line"
                    ),
                }
            }
        };
        std::thread::Builder::new()
            .name("control-input".into())
            .spawn(reader_thread)?;
        Ok(Self {
            commands,
            open: true,
        })
    }

    pub async fn next(&mut self) -> Control {
        if self.open {
            if let Some(command) = self.commands.recv().await {
                return command;
            }
            self.open = false;
            tracing::info!("control input closed; continuing without it");
        }
        std::future::pending().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::BufReader, time::Duration};

    #[test]
    fn plain_lines_are_utterances() {
        assert_eq!(parse(b"hello pip"), Ok(Control::Say("hello pip".into())));
        assert_eq!(parse(b"  hi there \r"), Ok(Control::Say("hi there".into())));
        assert_eq!(parse(b"tab\tok"), Ok(Control::Say("tab\tok".into())));
    }

    #[test]
    fn json_lines_are_commands() {
        assert_eq!(
            parse(br#"{"say":"what time is it"}"#),
            Ok(Control::Say("what time is it".into()))
        );
        assert_eq!(parse(br#"{"wake":true}"#), Ok(Control::Wake));
        assert_eq!(
            parse(br#" {"listening":false} "#),
            Ok(Control::Listening(false))
        );
        assert_eq!(parse(br#"{"mute":true}"#), Ok(Control::Mute(true)));
        assert_eq!(parse(br#"{"mute":false}"#), Ok(Control::Mute(false)));
    }

    #[test]
    fn invalid_lines_are_rejected() {
        assert_eq!(parse(b""), Err(Rejected::Empty));
        assert_eq!(parse(b"   \r"), Err(Rejected::Empty));
        assert_eq!(parse(b"\xff\xfe"), Err(Rejected::NotUtf8));
        assert_eq!(parse(b"{say: hi}"), Err(Rejected::BadJson));
        assert_eq!(parse(b"[1]"), Ok(Control::Say("[1]".into())));
        assert_eq!(parse(br#"{"say":""}"#), Err(Rejected::BadUtterance));
        assert_eq!(parse(br#"{"say":"a\u0007b"}"#), Err(Rejected::BadUtterance));
        assert_eq!(parse(br#"{"say":7}"#), Err(Rejected::UnknownCommand));
        assert_eq!(parse(br#"{"wake":false}"#), Err(Rejected::UnknownCommand));
        assert_eq!(parse(br#"{"dance":true}"#), Err(Rejected::UnknownCommand));
        assert_eq!(
            parse(br#"{"wake":true,"mute":true}"#),
            Err(Rejected::UnknownCommand)
        );
        assert_eq!(parse(&vec![b'a'; MAX_LINE + 1]), Err(Rejected::TooLong));
    }

    #[test]
    fn bounded_reads_discard_oversized_lines_and_keep_going() {
        let mut data = vec![b'x'; MAX_LINE * 3];
        data.extend_from_slice(b"\nnext\nlast");
        let mut reader = BufReader::with_capacity(512, &data[..]);
        let mut line = Vec::new();
        assert_eq!(
            read_bounded(&mut reader, &mut line, MAX_LINE).unwrap(),
            Read::TooLong
        );
        assert!(
            line.capacity() <= MAX_LINE + 512,
            "never buffers the long line"
        );
        assert_eq!(
            read_bounded(&mut reader, &mut line, MAX_LINE).unwrap(),
            Read::Line
        );
        assert_eq!(line, b"next");
        assert_eq!(
            read_bounded(&mut reader, &mut line, MAX_LINE).unwrap(),
            Read::Line
        );
        assert_eq!(line, b"last");
        assert_eq!(
            read_bounded(&mut reader, &mut line, MAX_LINE).unwrap(),
            Read::Eof
        );
    }

    #[tokio::test]
    async fn reader_skips_bad_lines_and_eof_disables_the_branch() {
        let mut data = b"hello\n\n{\"wake\":true}\n{oops\n".to_vec();
        data.extend_from_slice(&vec![b'z'; MAX_LINE + 10]);
        data.extend_from_slice(b"\n{\"say\":\"bye\"}\n");
        let mut input = Input::spawn(BufReader::new(std::io::Cursor::new(data))).unwrap();
        assert_eq!(input.next().await, Control::Say("hello".into()));
        assert_eq!(input.next().await, Control::Wake);
        assert_eq!(input.next().await, Control::Say("bye".into()));
        // After EOF the branch stays pending instead of resolving in a loop.
        let after = tokio::time::timeout(Duration::from_millis(100), input.next()).await;
        assert!(after.is_err());
        let again = tokio::time::timeout(Duration::from_millis(50), input.next()).await;
        assert!(again.is_err());
    }

    /// Regression: an input that never closes (the device's speech pipe)
    /// must not hold up runtime shutdown, or SIGTERM waits for SIGKILL.
    #[test]
    fn an_open_input_does_not_delay_runtime_shutdown() {
        let (reader, mut writer) = std::io::pipe().unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut input = runtime
            .block_on(async { Input::spawn(BufReader::new(reader)) })
            .unwrap();
        std::io::Write::write_all(&mut writer, b"hello\n").unwrap();
        assert_eq!(runtime.block_on(input.next()), Control::Say("hello".into()));
        let stopping = std::time::Instant::now();
        drop(input);
        drop(runtime);
        assert!(stopping.elapsed() < Duration::from_millis(200));
        drop(writer);
    }
}

//! Line-delimited control input from the wake word and speech recognition
//! process. See docs/architecture/robot-client.md for the contract.
//!
//! Lines arrive on stdin (`--simulate`, tests) and, on the device, on a Unix
//! control socket (`BUDDY_CONTROL_SOCKET`) that accepts a new writer whenever
//! the speech process restarts. If that path is an existing FIFO it is read
//! instead and reopened after each writer closes it.
//!
//! Readers are plain OS threads with blocking reads, not Tokio tasks: a read
//! on an open pipe never returns, and a blocking-pool task would make runtime
//! shutdown wait for it (SIGTERM would then hang until SIGKILL).
use serde_json::Value;
use std::{
    io::{BufRead, BufReader},
    os::unix::{
        fs::{FileTypeExt, PermissionsExt},
        net::UnixListener,
    },
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::mpsc;

/// Longest accepted input line, in bytes, newline excluded.
pub const MAX_LINE: usize = 8 * 1024;
/// Socket writers connected at once. The speech process needs one; a few more
/// let a restarted process connect before the old connection is noticed dead.
const MAX_WRITERS: usize = 4;
/// Owner and group (the speech process's account) may connect.
const SOCKET_MODE: u32 = 0o660;
const FIFO_RETRY: Duration = Duration::from_secs(1);

/// The control socket path from `BUDDY_CONTROL_SOCKET`, if set and non-empty.
fn control_path() -> Option<PathBuf> {
    control_path_from(|key| std::env::var(key).ok())
}

fn control_path_from(get: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    get("BUDDY_CONTROL_SOCKET")
        .filter(|v| !v.is_empty())
        .map(Into::into)
}

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
    /// The input ended after a line without its newline; it is in the buffer.
    Unterminated,
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
                (false, false) => Read::Unterminated,
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

/// How a source treats a last line without its newline. Stdin keeps it
/// (a here-doc or `echo -n`); a socket or FIFO writer that dies mid-line must
/// not have half an utterance submitted.
#[derive(Clone, Copy)]
enum Tail {
    Keep,
    Drop,
}

/// Why a source stopped.
enum Stopped {
    Eof,
    /// The session side is gone; nothing will read again.
    Closed,
}

/// Forward every valid line of `reader` until it ends.
fn pump<R: BufRead>(reader: &mut R, tx: &mpsc::Sender<Control>, tail: Tail) -> Stopped {
    let mut line = Vec::with_capacity(1024);
    loop {
        let parsed = match read_bounded(reader, &mut line, MAX_LINE) {
            Ok(Read::Line) => parse(&line),
            Ok(Read::Unterminated) => match tail {
                Tail::Keep => parse(&line),
                Tail::Drop => {
                    tracing::warn!(bytes = line.len(), "control writer closed mid-line");
                    return Stopped::Eof;
                }
            },
            Ok(Read::TooLong) => Err(Rejected::TooLong),
            Ok(Read::Eof) => return Stopped::Eof,
            Err(error) => {
                tracing::warn!(%error, "control input failed");
                return Stopped::Eof;
            }
        };
        match parsed {
            Ok(command) => {
                if tx.blocking_send(command).is_err() {
                    return Stopped::Closed;
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
}

fn thread(name: &str, work: impl FnOnce() + Send + 'static) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name(name.into())
        .spawn(work)
        .map(drop)
}

/// Control commands from reader threads. Once every source has closed, `next`
/// never resolves again, so a closed stdin cannot spin the session loop. A
/// control socket or FIFO never closes.
pub struct Input {
    commands: mpsc::Receiver<Control>,
    open: bool,
}

impl Input {
    /// Read `reader` on a detached OS thread, plus the control socket or FIFO
    /// named by `BUDDY_CONTROL_SOCKET` when it is set. Threads hold no state
    /// that needs cleanup, so process exit may end them mid-read.
    pub fn spawn<R: BufRead + Send + 'static>(reader: R) -> std::io::Result<Self> {
        Self::open(reader, control_path().as_deref())
    }

    /// Read `stdin`, and also the control socket or FIFO at `control` when given.
    pub fn open<R: BufRead + Send + 'static>(
        mut stdin: R,
        control: Option<&Path>,
    ) -> std::io::Result<Self> {
        let (tx, commands) = mpsc::channel(16);
        if let Some(path) = control {
            let fifo = std::fs::metadata(path).is_ok_and(|m| m.file_type().is_fifo());
            if fifo {
                read_fifo(path.to_owned(), tx.clone())?;
            } else {
                listen(path, tx.clone())?;
            }
        }
        thread("control-input", move || {
            pump(&mut stdin, &tx, Tail::Keep);
        })?;
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

/// Bind the control socket, replacing a stale socket left by an earlier run
/// (never any other kind of file), and accept writers for the process lifetime.
fn listen(path: &Path, tx: mpsc::Sender<Control>) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => std::fs::remove_file(path)?,
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "BUDDY_CONTROL_SOCKET exists and is not a socket or FIFO",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(SOCKET_MODE))?;
    tracing::info!(path = %path.display(), "control socket listening");
    let writers = Arc::new(AtomicUsize::new(0));
    thread("control-accept", move || {
        for stream in listener.incoming() {
            let stream = match stream {
                Ok(stream) => stream,
                Err(error) => {
                    tracing::warn!(%error, "control socket accept failed");
                    std::thread::sleep(FIFO_RETRY);
                    continue;
                }
            };
            if writers.fetch_add(1, Ordering::SeqCst) >= MAX_WRITERS {
                writers.fetch_sub(1, Ordering::SeqCst);
                tracing::warn!("control socket busy; refused a writer");
                continue;
            }
            let (sender, count) = (tx.clone(), writers.clone());
            let reader = move || {
                let mut reader = BufReader::with_capacity(MAX_LINE, stream);
                let stopped = pump(&mut reader, &sender, Tail::Drop);
                count.fetch_sub(1, Ordering::SeqCst);
                if matches!(stopped, Stopped::Eof) {
                    tracing::info!("control writer disconnected");
                }
            };
            if let Err(error) = thread("control-writer", reader) {
                tracing::warn!(%error, "control writer thread failed");
            }
            if tx.is_closed() {
                return;
            }
        }
    })
}

/// Read a FIFO, reopening it after each writer closes it. Opening blocks until
/// the next writer arrives, so an absent writer costs nothing.
fn read_fifo(path: PathBuf, tx: mpsc::Sender<Control>) -> std::io::Result<()> {
    thread("control-fifo", move || {
        let mut warned = false;
        loop {
            match std::fs::File::open(&path) {
                Ok(file) => {
                    warned = false;
                    let mut reader = BufReader::with_capacity(MAX_LINE, file);
                    if matches!(pump(&mut reader, &tx, Tail::Drop), Stopped::Closed) {
                        return;
                    }
                }
                Err(error) => {
                    if !warned {
                        tracing::warn!(%error, "control FIFO unavailable; retrying");
                        warned = true;
                    }
                    std::thread::sleep(FIFO_RETRY);
                }
            }
        }
    })
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
            Read::Unterminated
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

    fn scratch(name: &str) -> PathBuf {
        // Unix socket paths are limited to ~104 bytes; macOS temp dirs are long.
        let id = uuid::Uuid::new_v4().simple().to_string();
        PathBuf::from(format!("/tmp/buddy-{name}-{}", &id[..12]))
    }

    async fn next_within(input: &mut Input) -> Control {
        tokio::time::timeout(Duration::from_secs(2), input.next())
            .await
            .expect("control line arrives")
    }

    #[test]
    fn the_control_path_comes_from_the_environment() {
        let path = control_path_from(|_| Some("/run/robot-buddy/control.sock".into()));
        assert_eq!(path, Some(PathBuf::from("/run/robot-buddy/control.sock")));
        assert_eq!(control_path_from(|_| Some(String::new())), None);
        assert_eq!(control_path_from(|_| None), None);
    }

    #[tokio::test]
    async fn the_control_socket_survives_its_writer_restarting() {
        use std::{io::Write, os::unix::net::UnixStream};
        let path = scratch("ctl");
        let mut input = Input::open(std::io::empty(), Some(&path)).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, SOCKET_MODE);

        let mut first = UnixStream::connect(&path).unwrap();
        first.write_all(b"{\"wake\":true}\nhalf an utter").unwrap();
        assert_eq!(next_within(&mut input).await, Control::Wake);
        // The speech process dies mid-line and restarts.
        drop(first);
        let mut second = UnixStream::connect(&path).unwrap();
        second.write_all(b"hello pip\n").unwrap();
        assert_eq!(
            next_within(&mut input).await,
            Control::Say("hello pip".into())
        );
        // The dead writer's partial line was never submitted.
        let extra = tokio::time::timeout(Duration::from_millis(100), input.next()).await;
        assert!(extra.is_err());
        drop(second);
        let mut third = UnixStream::connect(&path).unwrap();
        third.write_all(b"{\"mute\":true}\n").unwrap();
        assert_eq!(next_within(&mut input).await, Control::Mute(true));
        std::fs::remove_file(&path).unwrap();
    }

    #[tokio::test]
    async fn a_stale_socket_is_replaced_but_other_files_are_kept() {
        let path = scratch("stale");
        drop(UnixListener::bind(&path).unwrap());
        let input = Input::open(std::io::empty(), Some(&path)).unwrap();
        drop(input);
        std::fs::remove_file(&path).unwrap();

        std::fs::write(&path, b"keep me").unwrap();
        let refused = Input::open(std::io::empty(), Some(&path));
        assert!(refused.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"keep me");
        std::fs::remove_file(&path).unwrap();
    }

    #[tokio::test]
    async fn a_control_fifo_is_reopened_after_each_writer() {
        use std::io::Write;
        let path = scratch("fifo");
        let made = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap();
        assert!(made.success());
        let mut input = Input::open(std::io::empty(), Some(&path)).unwrap();
        for (line, expected) in [
            (&b"{\"listening\":true}\n"[..], Control::Listening(true)),
            (&b"good night\n"[..], Control::Say("good night".into())),
        ] {
            let mut writer = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            writer.write_all(line).unwrap();
            drop(writer);
            assert_eq!(next_within(&mut input).await, expected);
        }
        std::fs::remove_file(&path).unwrap();
    }
}

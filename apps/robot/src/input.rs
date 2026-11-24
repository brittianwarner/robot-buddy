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
//!
//! The socket is created under umask 0077 (owner only), then given the group
//! `BUDDY_CONTROL_GROUP` (default `pip-control`, the speech account's group)
//! and mode 0660, so it is never wider than that at any moment.
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
/// The group that may write control lines when `BUDDY_CONTROL_GROUP` is unset.
pub const DEFAULT_CONTROL_GROUP: &str = "pip-control";
const FIFO_RETRY: Duration = Duration::from_secs(1);

/// Where device control lines come from, and which group may write them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControlSource {
    pub path: PathBuf,
    pub group: String,
}

/// The control socket from `BUDDY_CONTROL_SOCKET` (if set and non-empty) and
/// `BUDDY_CONTROL_GROUP`.
fn control_source() -> Option<ControlSource> {
    control_source_from(|key| std::env::var(key).ok())
}

fn control_source_from(get: impl Fn(&str) -> Option<String>) -> Option<ControlSource> {
    let path = get("BUDDY_CONTROL_SOCKET").filter(|v| !v.is_empty())?;
    let group = get("BUDDY_CONTROL_GROUP")
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_CONTROL_GROUP.into());
    Some(ControlSource {
        path: path.into(),
        group,
    })
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
    /// The owner spoke the six-digit confirmation code shown on their phone.
    /// Only accepted while a claim waits for confirmation.
    Confirm(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rejected {
    Empty,
    TooLong,
    NotUtf8,
    BadJson,
    UnknownCommand,
    BadUtterance,
    BadCode,
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
            Self::BadCode => "confirmation code is not six digits",
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
        ("confirm", Value::String(code)) => confirm_code(&code).map(Control::Confirm),
        _ => Err(Rejected::UnknownCommand),
    }
}

/// Six ASCII digits. Speech recognition may space or hyphenate them
/// ("123 456", "12-34-56"); those separators are dropped and nothing else is.
fn confirm_code(spoken: &str) -> Result<String, Rejected> {
    let code: String = spoken.chars().filter(|c| !matches!(c, ' ' | '-')).collect();
    if code.len() == 6 && code.bytes().all(|b| b.is_ascii_digit()) {
        Ok(code)
    } else {
        Err(Rejected::BadCode)
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

/// Starts a reader thread; injectable so tests can make spawning fail.
type Spawn = fn(&str, Box<dyn FnOnce() + Send>) -> std::io::Result<()>;

fn spawn_thread(name: &str, work: Box<dyn FnOnce() + Send>) -> std::io::Result<()> {
    thread(name, work)
}

/// One connected socket writer. Released on drop, so a reader thread that
/// ends, panics or never starts gives its slot back.
struct WriterSlot(Arc<AtomicUsize>);

impl WriterSlot {
    fn claim(writers: &Arc<AtomicUsize>) -> Option<Self> {
        let claimed = writers
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < MAX_WRITERS).then_some(n + 1)
            })
            .is_ok();
        claimed.then(|| Self(writers.clone()))
    }
}

impl Drop for WriterSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
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
        Self::open(reader, control_source().as_ref())
    }

    /// Read `stdin`, and also the control socket or FIFO at `control` when given.
    pub fn open<R: BufRead + Send + 'static>(
        mut stdin: R,
        control: Option<&ControlSource>,
    ) -> std::io::Result<Self> {
        let (tx, commands) = mpsc::channel(16);
        if let Some(control) = control {
            let path = &control.path;
            let fifo = std::fs::metadata(path).is_ok_and(|m| m.file_type().is_fifo());
            if fifo {
                read_fifo(path.clone(), tx.clone())?;
            } else {
                listen(control, tx.clone())?;
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
fn listen(control: &ControlSource, tx: mpsc::Sender<Control>) -> std::io::Result<()> {
    let path = control.path.as_path();
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
    let listener = bind_private(path)?;
    give_to_group(path, &control.group);
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(SOCKET_MODE))?;
    tracing::info!(path = %path.display(), "control socket listening");
    let writers = Arc::new(AtomicUsize::new(0));
    thread("control-accept", move || {
        accept_writers(&listener, &tx, &writers, spawn_thread);
    })
}

/// Bind with umask 0077, so the socket is owner-only from the moment it
/// exists. The umask is process-wide: other threads creating files in this
/// short window get narrower permissions, never wider ones. Binds are
/// serialized: two overlapping ones would restore each other's umask early.
fn bind_private(path: &Path) -> std::io::Result<UnixListener> {
    use nix::sys::stat::{Mode, umask};
    static UMASK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _held = UMASK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let previous = umask(Mode::from_bits_truncate(0o077));
    let bound = UnixListener::bind(path);
    umask(previous);
    bound
}

/// Hand the socket to `group`. A group that does not exist (a development
/// machine) keeps the process's group, logged once; so does a `chown` the
/// service account is not allowed (it must be a member of the group).
fn give_to_group(path: &Path, group: &str) {
    use nix::unistd::{Group, chown};
    let gid = match Group::from_name(group) {
        Ok(Some(found)) => found.gid,
        Ok(None) => {
            tracing::warn!(
                group,
                "control group missing; socket keeps the process group"
            );
            return;
        }
        Err(error) => {
            tracing::warn!(group, %error, "control group lookup failed; socket keeps the process group");
            return;
        }
    };
    if let Err(error) = chown(path, None, Some(gid)) {
        tracing::warn!(group, %error, "control socket group not set; socket keeps the process group");
    }
}

/// Accept writers until the session side is gone. Each gets a reader thread
/// and one of [`MAX_WRITERS`] slots.
fn accept_writers(
    listener: &UnixListener,
    tx: &mpsc::Sender<Control>,
    writers: &Arc<AtomicUsize>,
    spawn: Spawn,
) {
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(error) => {
                tracing::warn!(%error, "control socket accept failed");
                std::thread::sleep(FIFO_RETRY);
                continue;
            }
        };
        let Some(slot) = WriterSlot::claim(writers) else {
            tracing::warn!("control socket busy; refused a writer");
            continue;
        };
        let sender = tx.clone();
        let reader = Box::new(move || {
            let _slot = slot;
            let mut reader = BufReader::with_capacity(MAX_LINE, stream);
            if matches!(pump(&mut reader, &sender, Tail::Drop), Stopped::Eof) {
                tracing::info!("control writer disconnected");
            }
        });
        // A failed spawn drops the closure, and with it the slot.
        if let Err(error) = spawn("control-writer", reader) {
            tracing::warn!(%error, "control writer thread failed");
        }
        if tx.is_closed() {
            return;
        }
    }
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
        assert_eq!(
            parse(br#"{"confirm":"123456"}"#),
            Ok(Control::Confirm("123456".into()))
        );
        assert_eq!(
            parse(br#"{"confirm":"12 34-56"}"#),
            Ok(Control::Confirm("123456".into()))
        );
    }

    #[test]
    fn confirmation_codes_are_six_digits_only() {
        for bad in [
            r#"{"confirm":""}"#,
            r#"{"confirm":"12345"}"#,
            r#"{"confirm":"1234567"}"#,
            r#"{"confirm":"12345a"}"#,
            r#"{"confirm":"１２３４５６"}"#,
            r#"{"confirm":"123.456"}"#,
        ] {
            assert_eq!(parse(bad.as_bytes()), Err(Rejected::BadCode), "{bad}");
        }
        assert_eq!(
            parse(br#"{"confirm":123456}"#),
            Err(Rejected::UnknownCommand)
        );
        assert_eq!(parse(br#"{"confirm":true}"#), Err(Rejected::UnknownCommand));
        // A plain line is an utterance, never a confirmation.
        assert_eq!(parse(b"123456"), Ok(Control::Say("123456".into())));
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
        let took = stopping.elapsed();
        // The regression hung until the reader saw EOF; this bound only
        // catches that. The latency bound is checked under strict timing.
        assert!(took < Duration::from_secs(10), "{took:?}");
        if std::env::var("BUDDY_STRICT_TIMING").as_deref() == Ok("1") {
            assert!(took < Duration::from_millis(200), "{took:?}");
        }
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

    fn source(path: &Path) -> ControlSource {
        ControlSource {
            path: path.to_owned(),
            group: DEFAULT_CONTROL_GROUP.into(),
        }
    }

    #[test]
    fn the_control_socket_comes_from_the_environment() {
        let at =
            |key: &str| (key == "BUDDY_CONTROL_SOCKET").then(|| "/run/pip/control.sock".into());
        assert_eq!(
            control_source_from(at),
            Some(ControlSource {
                path: "/run/pip/control.sock".into(),
                group: "pip-control".into(),
            })
        );
        let grouped = |key: &str| match key {
            "BUDDY_CONTROL_SOCKET" => Some("/s".into()),
            "BUDDY_CONTROL_GROUP" => Some("stt".into()),
            _ => None,
        };
        assert_eq!(control_source_from(grouped).unwrap().group, "stt");
        assert_eq!(control_source_from(|_| Some(String::new())), None);
        assert_eq!(control_source_from(|_| None), None);
    }

    fn owner_and_mode(path: &Path) -> (u32, u32) {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(path).unwrap();
        (meta.gid(), meta.mode() & 0o777)
    }

    #[test]
    fn the_socket_is_owner_only_from_the_moment_it_is_bound() {
        let path = scratch("bind");
        let listener = bind_private(&path).unwrap();
        let (_, mode) = owner_and_mode(&path);
        assert_eq!(mode & 0o077, 0, "bound as {mode:o}");
        drop(listener);
        std::fs::remove_file(&path).unwrap();
    }

    #[tokio::test]
    async fn the_socket_belongs_to_the_control_group_with_mode_0660() {
        use nix::unistd::{Group, getegid};
        let own = Group::from_gid(getegid()).unwrap().expect("own group").name;
        // Without a group change the socket keeps the group it was created
        // with: the process group on Linux, the directory's group on macOS.
        let probe = scratch("probe");
        let bound = bind_private(&probe).unwrap();
        let (created, _) = owner_and_mode(&probe);
        drop(bound);
        std::fs::remove_file(&probe).unwrap();
        for (group, gid) in [
            (own.as_str(), getegid().as_raw()),
            ("pip-control-missing-for-test", created),
        ] {
            let path = scratch("group");
            let control = ControlSource {
                path: path.clone(),
                group: group.into(),
            };
            let input = Input::open(std::io::empty(), Some(&control)).unwrap();
            // An existing group is used; a missing one keeps the process group.
            assert_eq!(owner_and_mode(&path), (gid, SOCKET_MODE), "{group}");
            drop(input);
            std::fs::remove_file(&path).unwrap();
        }
    }

    /// A reader thread that cannot start gives its writer slot back, so
    /// failed spawns can never lock the speech process out.
    #[test]
    fn a_failed_reader_spawn_releases_its_slot() {
        use std::os::unix::net::UnixStream;
        fn failing(_: &str, _: Box<dyn FnOnce() + Send>) -> std::io::Result<()> {
            Err(std::io::Error::other("no threads left"))
        }
        let path = scratch("slots");
        let listener = UnixListener::bind(&path).unwrap();
        let writers = Arc::new(AtomicUsize::new(0));
        let (tx, _commands) = mpsc::channel(16);
        let counted = writers.clone();
        let accepting = std::thread::spawn(move || {
            accept_writers(&listener, &tx, &counted, failing);
        });
        let (seen, accepted) = std::sync::mpsc::channel();
        for _ in 0..MAX_WRITERS * 3 {
            let stream = UnixStream::connect(&path).unwrap();
            // The accept loop has handled this writer once it closes the
            // stream: a read then sees EOF instead of blocking.
            // macOS refuses the option (EINVAL) once the peer has closed;
            // the read below then sees EOF at once.
            let _closed_already = stream.set_read_timeout(Some(Duration::from_secs(10)));
            let mut byte = [0u8; 1];
            let eof = std::io::Read::read(&mut &stream, &mut byte).unwrap();
            assert_eq!(eof, 0);
            seen.send(()).unwrap();
        }
        drop(seen);
        assert_eq!(accepted.iter().count(), MAX_WRITERS * 3);
        assert_eq!(writers.load(Ordering::SeqCst), 0, "every slot released");
        std::fs::remove_file(&path).unwrap();
        drop(accepting);
    }

    #[test]
    fn writer_slots_are_bounded_and_released_on_drop() {
        let writers = Arc::new(AtomicUsize::new(0));
        let slots: Vec<_> = (0..MAX_WRITERS)
            .map(|_| WriterSlot::claim(&writers).unwrap())
            .collect();
        assert!(WriterSlot::claim(&writers).is_none());
        assert_eq!(writers.load(Ordering::SeqCst), MAX_WRITERS);
        drop(slots);
        assert_eq!(writers.load(Ordering::SeqCst), 0);
        assert!(WriterSlot::claim(&writers).is_some());
    }

    #[tokio::test]
    async fn the_control_socket_survives_its_writer_restarting() {
        use std::{io::Write, os::unix::net::UnixStream};
        let path = scratch("ctl");
        let mut input = Input::open(std::io::empty(), Some(&source(&path))).unwrap();
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
        let input = Input::open(std::io::empty(), Some(&source(&path))).unwrap();
        drop(input);
        std::fs::remove_file(&path).unwrap();

        std::fs::write(&path, b"keep me").unwrap();
        let refused = Input::open(std::io::empty(), Some(&source(&path)));
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
        let mut input = Input::open(std::io::empty(), Some(&source(&path))).unwrap();
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

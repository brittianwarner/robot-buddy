//! ESP32-S3 face bridge: one compact JSON object per line over USB serial.
//!
//! The session sets a `Mood`; a writer task turns it into face commands,
//! re-sends it every `HEARTBEAT` (the ESP32 falls back to a calm idle face
//! after 5 s of silence), and reopens the device if it disappears. Only the
//! latest mood matters, so the session never waits on the serial port.
//!
//! The device is opened `O_RDWR | O_NOCTTY | O_NONBLOCK` and set to raw
//! 115200 baud with `CLOCAL` and without `HUPCL` on every open, because a
//! re-enumerated CDC-ACM port comes back at the kernel defaults (9600, cooked,
//! echo). Writes are non-blocking and driven by reactor readiness, so a
//! stalled port times out and is closed without parking a blocking-pool
//! thread on `write(2)`.
use nix::sys::termios::{self, BaudRate, ControlFlags, SetArg, Termios};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    future::Future,
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::unix::pipe,
    sync::{mpsc, watch},
    time::Instant,
};

pub const HEARTBEAT: Duration = Duration::from_millis(1500);
const WRITE_DEADLINE: Duration = Duration::from_millis(500);
const REOPEN_DELAY: Duration = Duration::from_secs(1);
/// Bounds `open` plus `tcsetattr`. Both are non-blocking syscalls, but the
/// CDC-ACM driver sends USB control requests during them, each with a 5 s
/// kernel timeout, so a wedged board can hold the opening thread briefly.
const OPEN_DEADLINE: Duration = Duration::from_secs(2);
/// After an open overran its deadline, wait past the kernel's own USB timeouts
/// so at most one opening thread is ever outstanding.
const STALLED_OPEN_RETRY: Duration = Duration::from_secs(15);
pub const SORRY: Duration = Duration::from_millis(1500);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Expression {
    Neutral,
    Happy,
    Thinking,
    Curious,
    Surprised,
    Angry,
    Love,
    Asleep,
    Sad,
    Sleepy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Led {
    Idle,
    Listening,
    Thinking,
    Speaking,
    Muted,
}

/// Wire shapes: `{"e":"happy","i":0.8}`, `{"look":[0.3,-0.2]}`, `{"led":"listening"}`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum FaceCommand {
    Expression { e: Expression, i: f32 },
    Look { look: [f32; 2] },
    Led { led: Led },
    Gesture { g: FaceGesture },
    Ping { ping: u32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FaceGesture {
    Blink,
    EarWiggle,
}

impl FaceCommand {
    pub fn line(&self) -> Vec<u8> {
        let mut line = serde_json::to_vec(self).unwrap_or_default();
        line.push(b'\n');
        line
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mood {
    /// Not connected to the server.
    Offline,
    Idle,
    /// Wake word heard; waiting for the utterance.
    Listening,
    /// Utterance submitted; no audio yet.
    Thinking,
    /// PCM is reaching the speaker.
    Speaking,
    Muted,
    /// Brief apology after a failed or interrupted turn.
    Sorry,
    /// An owner signed in on their phone; the robot waits for them to speak
    /// the six-digit code it shows them.
    Confirming,
}

impl Mood {
    /// In discriminant order: `frame` indexes it by `self as usize`.
    const ALL: [Self; 8] = [
        Self::Offline,
        Self::Idle,
        Self::Listening,
        Self::Thinking,
        Self::Speaking,
        Self::Muted,
        Self::Sorry,
        Self::Confirming,
    ];

    pub fn commands(self) -> Vec<FaceCommand> {
        use FaceCommand::{Expression as E, Led as L, Look};
        let (led, e, i) = match self {
            Self::Offline => (Led::Idle, Expression::Sleepy, 0.5),
            Self::Idle => (Led::Idle, Expression::Neutral, 0.5),
            Self::Listening => (Led::Listening, Expression::Curious, 0.8),
            Self::Thinking => (Led::Thinking, Expression::Thinking, 0.6),
            Self::Speaking => (Led::Speaking, Expression::Happy, 0.7),
            Self::Muted => (Led::Muted, Expression::Neutral, 0.3),
            Self::Sorry => (Led::Idle, Expression::Sad, 0.6),
            // Listening for the owner's code, glad to meet them: the LED asks
            // for speech, the expression tells it apart from the wake word.
            Self::Confirming => (Led::Listening, Expression::Happy, 0.8),
        };
        let mut commands = vec![L { led }, E { e, i }];
        if matches!(self, Self::Listening | Self::Speaking | Self::Confirming) {
            commands.push(Look { look: [0.0, 0.0] });
        }
        commands
    }

    /// Every line of this mood, serialized once per process and sent in one write.
    pub fn frame(self) -> &'static [u8] {
        static FRAMES: OnceLock<Vec<Vec<u8>>> = OnceLock::new();
        let frames = FRAMES.get_or_init(|| {
            Self::ALL
                .iter()
                .map(|mood| mood.commands().iter().flat_map(FaceCommand::line).collect())
                .collect()
        });
        &frames[self as usize]
    }

    /// States the user is acting on. A `Sorry` flash never hides them.
    fn active(self) -> bool {
        matches!(self, Self::Listening | Self::Speaking | Self::Muted)
    }
}

/// Conversation facts the face is derived from.
// Independent facts from separate sources; `mood` ranks them, so an enum would lose data.
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent facts from separate sources; mood ranks them"
)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Signals {
    pub connected: bool,
    pub muted: bool,
    pub listening: bool,
    pub speaking: bool,
    pub thinking: bool,
    /// A claim waits for the owner's spoken confirmation code.
    pub confirming: bool,
}

pub fn mood(s: Signals) -> Mood {
    if !s.connected {
        Mood::Offline
    } else if s.muted {
        Mood::Muted
    } else if s.speaking {
        Mood::Speaking
    } else if s.listening {
        Mood::Listening
    } else if s.confirming {
        Mood::Confirming
    } else if s.thinking {
        Mood::Thinking
    } else {
        Mood::Idle
    }
}

#[derive(Clone, PartialEq)]
pub struct Target {
    mood: Mood,
    flash: Option<(Mood, Instant)>,
    expression: Option<(Expression, f32, Instant)>,
    gaze: Option<[f32; 2]>,
    qr: Option<Arc<[u8]>>,
}

impl Target {
    fn current(&self, now: Instant) -> Mood {
        match self.flash {
            Some((mood, until)) if now < until && !self.mood.active() => mood,
            _ => self.mood,
        }
    }
}

/// Cheap, cloneable handle. Without a serial device nothing listens.
#[derive(Clone)]
pub struct Face {
    target: watch::Sender<Target>,
    impulses: mpsc::Sender<(Instant, FaceGesture)>,
}

pub struct FaceRx {
    state: watch::Receiver<Target>,
    impulses: mpsc::Receiver<(Instant, FaceGesture)>,
}

impl Face {
    pub fn new() -> (Self, FaceRx) {
        let (target, rx) = watch::channel(Target {
            mood: Mood::Offline,
            flash: None,
            expression: None,
            gaze: None,
            qr: None,
        });
        let (tx, impulses) = mpsc::channel(4);
        (
            Self {
                target,
                impulses: tx,
            },
            FaceRx {
                state: rx,
                impulses,
            },
        )
    }

    /// Board overrides never replace the controller-owned chest LED.
    pub fn express(&self, emotion: Expression, intensity: f32) {
        if intensity.is_finite() {
            self.target.send_modify(|s| {
                s.expression = Some((
                    emotion,
                    intensity.clamp(0., 1.),
                    Instant::now() + Duration::from_secs(4),
                ))
            });
        }
    }
    pub fn gaze(&self, x: f32, y: f32) {
        if x.is_finite() && y.is_finite() {
            let gaze = Some([x.clamp(-1., 1.), y.clamp(-1., 1.)]);
            self.target.send_if_modified(|s| {
                let changed = s.gaze != gaze;
                s.gaze = gaze;
                changed
            });
        }
    }
    pub fn gesture(&self, g: FaceGesture) {
        let _ = self.impulses.try_send((Instant::now(), g));
    }
    /// Retain the desired display across USB re-enumeration. Encoded QR lines may
    /// contain network credentials and are deliberately absent from diagnostics.
    pub fn qr(&self, left: &str, right: &str, width: u16) -> io::Result<()> {
        if [left, right]
            .iter()
            .any(|s| s.len() > 255 || s.chars().any(char::is_control))
        {
            return Err(io::Error::other("invalid eye QR text"));
        }
        let frame = if left.is_empty() && right.is_empty() {
            None
        } else {
            let mut bytes = serde_json::to_vec(
                &serde_json::json!({"qr":[left,right],"w":width.clamp(60,200)}),
            )?;
            if bytes.len() > 560 {
                return Err(io::Error::other("eye QR command too long"));
            }
            bytes.push(b'\n');
            Some(Arc::from(bytes))
        };
        self.target.send_if_modified(|s| {
            let changed = s.qr != frame;
            s.qr = frame;
            changed
        });
        Ok(())
    }

    /// Set the steady mood. Listening, speaking and muted end any flash, so an
    /// apology for the previous turn cannot cover the orange listening LED or
    /// come back after it.
    pub fn set(&self, mood: Mood) {
        self.target.send_if_modified(|t| {
            let cleared = mood.active() && t.flash.take().is_some();
            let changed = t.mood != mood || cleared;
            if changed
                && matches!(
                    mood,
                    Mood::Muted | Mood::Listening | Mood::Offline | Mood::Sorry | Mood::Confirming
                )
            {
                t.expression = None;
            }
            t.mood = mood;
            changed
        });
    }

    /// Show `mood` for `duration`, then return to the steady mood. Ignored
    /// while the steady mood is listening, speaking or muted.
    pub fn flash(&self, mood: Mood, duration: Duration) {
        self.target.send_if_modified(|t| {
            if t.mood.active() {
                return false;
            }
            t.flash = Some((mood, Instant::now() + duration));
            true
        });
    }

    #[cfg(test)]
    pub fn steady(&self) -> Mood {
        self.target.borrow().mood
    }

    #[cfg(test)]
    pub fn flashing(&self) -> Option<Mood> {
        let target = self.target.borrow();
        target.flash.map(|(mood, _)| mood)
    }
}

pub trait LineSink: Send {
    /// Write one frame: one or more complete lines.
    fn write_frame(&mut self, frame: &[u8]) -> impl Future<Output = io::Result<()>> + Send;
    fn read_status(&mut self, _buf: &mut [u8]) -> impl Future<Output = io::Result<usize>> + Send {
        std::future::pending()
    }
    fn has_board(&self) -> bool {
        false
    }
}

pub trait Opener: Send + 'static {
    type Sink: LineSink;
    fn open(&mut self) -> impl Future<Output = io::Result<Self::Sink>> + Send;
}

/// A serial device (or any file) opened for writing. Never creates the path.
/// Prefer a stable `/dev/serial/by-id/...` path over `/dev/ttyACM0`.
pub struct Device(pub PathBuf);

pub enum DeviceSink {
    /// A tty or FIFO: non-blocking writes driven by readiness. Tokio's pipe
    /// sender is only `write(2)` on a registered non-blocking descriptor, so it
    /// serves a tty as well as a FIFO.
    Poll((pipe::Sender, Option<pipe::Receiver>)),
    /// A regular file (debugging, tests): writes never wait.
    File(File),
}

impl Opener for Device {
    type Sink = DeviceSink;
    async fn open(&mut self) -> io::Result<DeviceSink> {
        let path = self.0.clone();
        let opening = tokio::task::spawn_blocking(move || open_serial(&path));
        let file = match tokio::time::timeout(OPEN_DEADLINE, opening).await {
            Ok(joined) => joined.map_err(io::Error::other)??,
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "face device open stalled",
                ));
            }
        };
        if file.metadata()?.is_file() {
            return Ok(DeviceSink::File(file));
        }
        let reader = if nix::unistd::isatty(&file)? {
            Some(pipe::Receiver::from_file_unchecked(file.try_clone()?)?)
        } else {
            None
        };
        Ok(DeviceSink::Poll((
            pipe::Sender::from_file_unchecked(file)?,
            reader,
        )))
    }
}

/// Open without becoming the controlling terminal and without waiting for
/// carrier, then apply the line settings when the file is a tty.
fn open_serial(path: &Path) -> io::Result<File> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(nix::libc::O_NOCTTY | nix::libc::O_NONBLOCK)
        .open(path)?;
    match termios::tcgetattr(&file) {
        Ok(mut settings) => {
            serial_settings(&mut settings)?;
            termios::tcsetattr(&file, SetArg::TCSANOW, &settings)?;
        }
        // A FIFO or a plain file: no line settings to apply.
        Err(nix::errno::Errno::ENOTTY) => {}
        Err(error) => return Err(error.into()),
    }
    Ok(file)
}

/// `stty 115200 raw -echo clocal -hupcl`: no echo, no line editing or output
/// translation, ignore modem lines, and leave DTR up on close so a reopen
/// does not reset the ESP32.
pub fn serial_settings(settings: &mut Termios) -> nix::Result<()> {
    termios::cfmakeraw(settings);
    termios::cfsetspeed(settings, BaudRate::B115200)?;
    settings
        .control_flags
        .insert(ControlFlags::CLOCAL | ControlFlags::CREAD);
    settings.control_flags.remove(ControlFlags::HUPCL);
    Ok(())
}

impl LineSink for DeviceSink {
    async fn write_frame(&mut self, frame: &[u8]) -> io::Result<()> {
        match self {
            Self::File(file) => file.write_all(frame),
            Self::Poll((port, _)) => port.write_all(frame).await,
        }
    }
    async fn read_status(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Self::Poll((_, Some(port))) = self {
            port.read(buf).await
        } else {
            std::future::pending().await
        }
    }
    fn has_board(&self) -> bool {
        matches!(self, Self::Poll((_, Some(_))))
    }
}

/// Drive the face until every `Face` handle is dropped.
pub async fn run<O: Opener>(
    FaceRx {
        state: mut target,
        mut impulses,
    }: FaceRx,
    mut opener: O,
) {
    let mut warned = false;
    loop {
        let wait = match opener.open().await {
            Ok(mut port) => {
                tracing::info!("face device open");
                warned = false;
                if let Err(error) = drive(&mut port, &mut target, &mut impulses).await {
                    tracing::warn!(%error, "face link lost; reopening");
                } else {
                    return;
                }
                REOPEN_DELAY
            }
            Err(error) => {
                if !warned {
                    tracing::warn!(%error, "face device unavailable; retrying");
                    warned = true;
                }
                if error.kind() == io::ErrorKind::TimedOut {
                    STALLED_OPEN_RETRY
                } else {
                    REOPEN_DELAY
                }
            }
        };
        let deadline = Instant::now() + wait;
        loop {
            tokio::select! {
                ()=tokio::time::sleep_until(deadline)=>break,
                changed=target.changed()=>if changed.is_err(){return},
                gesture=impulses.recv()=>if gesture.is_none(){return},
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
struct Display {
    mood: Mood,
    expression: Option<(Expression, f32)>,
    gaze: Option<[f32; 2]>,
}

impl Display {
    fn append(self, previous: Option<Self>, beat: bool, frame: &mut Vec<u8>) {
        let base =
            beat || previous.is_none_or(|p| p.mood != self.mood || p.expression != self.expression);
        if base && self.expression.is_none() && self.gaze.is_none() {
            frame.extend_from_slice(self.mood.frame());
            return;
        }
        if base {
            for command in self.mood.commands() {
                match command {
                    FaceCommand::Look { .. } if self.gaze.is_some() => {}
                    FaceCommand::Expression { .. } if self.expression.is_some() => {}
                    _ => frame.extend(command.line()),
                }
            }
            if let Some((e, i)) = self.expression {
                frame.extend(FaceCommand::Expression { e, i }.line());
            }
        }
        if let Some(look) = self.gaze
            && (base || previous.is_none_or(|p| p.gaze != self.gaze))
        {
            frame.extend(FaceCommand::Look { look }.line());
        }
    }
}

async fn drive<S: LineSink>(
    device: &mut S,
    target: &mut watch::Receiver<Target>,
    impulses: &mut mpsc::Receiver<(Instant, FaceGesture)>,
) -> io::Result<()> {
    let mut shown = None;
    let mut shown_qr = None;
    let mut status = StatusLines::default();
    let mut buf = [0; 512];
    let mut next_beat = Instant::now();
    let mut frame = Vec::with_capacity(256);
    loop {
        let wanted = target.borrow_and_update().clone();
        let now = Instant::now();
        let mood = wanted.current(now);
        let expression = wanted.expression.filter(|(_, _, until)| *until > now);
        let desired = Display {
            mood,
            expression: expression.map(|(e, i, _)| (e, i)),
            gaze: wanted.gaze,
        };
        frame.clear();
        desired.append(shown, now >= next_beat, &mut frame);
        if shown_qr != wanted.qr || (shown.is_none() && device.has_board()) {
            frame.extend_from_slice(wanted.qr.as_deref().unwrap_or(b"{\"qr\":[]}\n"));
        }
        if shown.is_none() && device.has_board() {
            frame.extend(FaceCommand::Ping { ping: 0 }.line());
        }
        if !frame.is_empty() {
            send(device, &frame).await?;
        }
        if now >= next_beat {
            next_beat = Instant::now() + HEARTBEAT;
        }
        shown = Some(desired);
        shown_qr = wanted.qr;
        let mut wake = next_beat;
        if let Some((_, _, until)) = expression {
            wake = wake.min(until);
        }
        if let Some((_, until)) = wanted.flash
            && until > Instant::now()
        {
            wake = wake.min(until);
        }
        tokio::select! {
            changed = target.changed() => if changed.is_err() { return Ok(()) },
            () = tokio::time::sleep_until(wake) => {},
            event = impulses.recv() => {
                let Some((at,g))=event else {return Ok(())};
                if at.elapsed()<WRITE_DEADLINE {send(device,&FaceCommand::Gesture{g}.line()).await?;}
            },
            result = device.read_status(&mut buf) => {
                let n=result?;
                if n==0{return Err(io::ErrorKind::UnexpectedEof.into());}
                status.push(&buf[..n]);
            },
        }
    }
}

async fn send<S: LineSink>(sink: &mut S, frame: &[u8]) -> io::Result<()> {
    tokio::time::timeout(WRITE_DEADLINE, sink.write_frame(frame))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "face write stalled"))?
}

/// Boot ROM noise and truncated lines are normal after a USB reconnect. Never
/// log raw board data: a misconfigured serial adapter could echo Wi-Fi QR text.
#[derive(Default)]
struct StatusLines {
    line: Vec<u8>,
    overflow: bool,
}
impl StatusLines {
    fn push(&mut self, bytes: &[u8]) {
        for &b in bytes {
            match b {
                b'\n' => {
                    if !self.overflow
                        && let Ok(v) = serde_json::from_slice::<serde_json::Value>(&self.line)
                    {
                        if v["hb"].is_u64() {
                            tracing::debug!(fps = v["fps"].as_u64(), "face heartbeat");
                        } else if v["hello"].is_string() {
                            tracing::info!(version = v["v"].as_u64(), "face hello");
                        } else if v["err"].is_string() {
                            tracing::warn!("face board rejected command");
                        }
                    }
                    self.line.clear();
                    self.overflow = false;
                }
                _ if self.line.len() < 512 => self.line.push(b),
                _ => self.overflow = true,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    type Log = Arc<Mutex<Vec<(Instant, String)>>>;

    struct Memory {
        log: Log,
        fail_opens: usize,
        fail_writes: usize,
    }
    struct MemorySink {
        log: Log,
        fail_writes: usize,
    }
    impl Opener for Memory {
        type Sink = MemorySink;
        async fn open(&mut self) -> io::Result<MemorySink> {
            if self.fail_opens > 0 {
                self.fail_opens -= 1;
                return Err(io::Error::new(io::ErrorKind::NotFound, "unplugged"));
            }
            Ok(MemorySink {
                log: self.log.clone(),
                fail_writes: std::mem::take(&mut self.fail_writes),
            })
        }
    }
    impl LineSink for MemorySink {
        async fn write_frame(&mut self, frame: &[u8]) -> io::Result<()> {
            if self.fail_writes > 0 {
                self.fail_writes -= 1;
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "re-enumerated"));
            }
            let now = Instant::now();
            let text = String::from_utf8(frame.to_vec()).unwrap();
            let mut log = self.log.lock().unwrap();
            for line in text.split_inclusive('\n') {
                log.push((now, line.to_owned()));
            }
            Ok(())
        }
    }

    fn lines(log: &Log) -> Vec<String> {
        log.lock().unwrap().iter().map(|(_, l)| l.clone()).collect()
    }

    #[test]
    fn commands_serialize_to_the_exact_wire_shapes() {
        let e = FaceCommand::Expression {
            e: Expression::Happy,
            i: 0.8,
        };
        assert_eq!(e.line(), b"{\"e\":\"happy\",\"i\":0.8}\n");
        let look = FaceCommand::Look { look: [0.3, -0.2] };
        assert_eq!(look.line(), b"{\"look\":[0.3,-0.2]}\n");
        let led = FaceCommand::Led {
            led: Led::Listening,
        };
        assert_eq!(led.line(), b"{\"led\":\"listening\"}\n");
    }

    #[test]
    fn mood_follows_conversation_signals() {
        let on = Signals {
            connected: true,
            ..Signals::default()
        };
        assert_eq!(mood(Signals::default()), Mood::Offline);
        assert_eq!(mood(on), Mood::Idle);
        let listening = Signals {
            listening: true,
            ..on
        };
        assert_eq!(mood(listening), Mood::Listening);
        let thinking = Signals {
            thinking: true,
            ..on
        };
        assert_eq!(mood(thinking), Mood::Thinking);
        let speaking = Signals {
            speaking: true,
            ..thinking
        };
        assert_eq!(mood(speaking), Mood::Speaking);
        let muted = Signals {
            muted: true,
            ..speaking
        };
        assert_eq!(mood(muted), Mood::Muted);
        // A claim waiting for the owner's code has its own face, above a
        // waiting utterance but never hiding listening, speaking or mute.
        let confirming = Signals {
            confirming: true,
            ..thinking
        };
        assert_eq!(mood(confirming), Mood::Confirming);
        for (over, want) in [
            (
                Signals {
                    listening: true,
                    ..confirming
                },
                Mood::Listening,
            ),
            (
                Signals {
                    speaking: true,
                    ..confirming
                },
                Mood::Speaking,
            ),
            (
                Signals {
                    muted: true,
                    ..confirming
                },
                Mood::Muted,
            ),
            (
                Signals {
                    connected: false,
                    ..confirming
                },
                Mood::Offline,
            ),
        ] {
            assert_eq!(mood(over), want, "{over:?}");
        }
        // Only existing wire lines, distinct from every other mood.
        assert_eq!(
            Mood::Confirming.frame(),
            b"{\"led\":\"listening\"}\n{\"e\":\"happy\",\"i\":0.8}\n{\"look\":[0.0,0.0]}\n"
        );
        for other in Mood::ALL.into_iter().filter(|m| *m != Mood::Confirming) {
            assert_ne!(other.frame(), Mood::Confirming.frame(), "{other:?}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn heartbeat_refreshes_expression_but_never_replays_gestures_or_qr() {
        let log = Log::default();
        let (face, rx) = Face::new();
        let task = tokio::spawn(run(
            rx,
            Memory {
                log: log.clone(),
                fail_opens: 0,
                fail_writes: 0,
            },
        ));
        face.set(Mood::Speaking);
        face.express(Expression::Love, 1.5);
        face.gaze(-2., 0.25);
        face.gesture(FaceGesture::EarWiggle);
        face.qr("left", "right", 132).unwrap();
        assert!(face.qr(&"x".repeat(256), "", 132).is_err());
        tokio::time::sleep(Duration::from_secs(5)).await;
        face.qr("", "", 132).unwrap();
        tokio::time::sleep(Duration::from_millis(1)).await;
        let wire = lines(&log).concat();
        assert_eq!(wire.matches("ear_wiggle").count(), 1);
        assert_eq!(wire.matches("\"qr\":[\"left\",\"right\"]").count(), 1);
        assert!(wire.contains("\"qr\":[]"));
        assert!(wire.contains("\"look\":[-1.0,0.25]"));
        assert!(wire.contains("\"e\":\"love\",\"i\":1.0"));
        assert!(wire.rfind("happy") > wire.rfind("love"));
        drop(face);
        task.await.unwrap();
    }

    #[tokio::test]
    async fn reads_board_status_from_the_same_raw_tty() {
        let (pair, path) = pty();
        let mut port = Device(path).open().await.unwrap();
        let mut master = File::from(pair.master);
        let hb = b"{\"hb\":123,\"fps\":22}\n";
        master.write_all(hb).unwrap();
        let mut bytes = [0; 128];
        let n = tokio::time::timeout(Duration::from_secs(1), port.read_status(&mut bytes))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&bytes[..n], hb);
    }

    #[tokio::test(start_paused = true)]
    async fn heartbeat_keeps_the_esp32_out_of_its_5s_fallback() {
        let log = Log::default();
        let (face, rx) = Face::new();
        let opener = Memory {
            log: log.clone(),
            fail_opens: 0,
            fail_writes: 0,
        };
        let task = tokio::spawn(run(rx, opener));
        face.set(Mood::Idle);
        tokio::time::sleep(Duration::from_secs(20)).await;
        let leds: Vec<Instant> = log
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, l)| l.contains("\"led\""))
            .map(|(t, _)| *t)
            .collect();
        assert!(leds.len() >= 13, "{} heartbeats in 20 s", leds.len());
        for pair in leds.windows(2) {
            assert!(pair[1] - pair[0] <= Duration::from_secs(2));
        }
        drop(face);
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn state_changes_are_written_immediately_and_flash_returns_to_steady() {
        let log = Log::default();
        let (face, rx) = Face::new();
        let opener = Memory {
            log: log.clone(),
            fail_opens: 0,
            fail_writes: 0,
        };
        tokio::spawn(run(rx, opener));
        face.set(Mood::Idle);
        tokio::time::sleep(Duration::from_millis(10)).await;
        log.lock().unwrap().clear();
        face.set(Mood::Listening);
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(
            lines(&log),
            [
                "{\"led\":\"listening\"}\n",
                "{\"e\":\"curious\",\"i\":0.8}\n",
                "{\"look\":[0.0,0.0]}\n"
            ]
        );
        log.lock().unwrap().clear();
        face.set(Mood::Idle);
        face.flash(Mood::Sorry, SORRY);
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(
            lines(&log),
            ["{\"led\":\"idle\"}\n", "{\"e\":\"sad\",\"i\":0.6}\n"]
        );
        log.lock().unwrap().clear();
        tokio::time::sleep(SORRY).await;
        assert_eq!(
            lines(&log)[..2],
            ["{\"led\":\"idle\"}\n", "{\"e\":\"neutral\",\"i\":0.5}\n"]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn reopens_the_device_after_it_disappears() {
        let log = Log::default();
        let (face, rx) = Face::new();
        let opener = Memory {
            log: log.clone(),
            fail_opens: 2,
            fail_writes: 1,
        };
        tokio::spawn(run(rx, opener));
        face.set(Mood::Thinking);
        tokio::time::sleep(Duration::from_secs(5)).await;
        let written = lines(&log);
        assert!(written.contains(&"{\"led\":\"thinking\"}\n".to_owned()));
    }

    #[tokio::test]
    async fn device_sink_writes_lines_to_a_file() {
        let path = std::env::temp_dir().join(format!("buddy-face-{}", uuid::Uuid::new_v4()));
        std::fs::write(&path, b"").unwrap();
        let (face, rx) = Face::new();
        let task = tokio::spawn(run(rx, Device(path.clone())));
        face.set(Mood::Muted);
        tokio::time::sleep(Duration::from_millis(200)).await;
        drop(face);
        task.await.unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("{\"led\":\"muted\"}\n{\"e\":\"neutral\",\"i\":0.3}\n"));
        for line in text.lines() {
            serde_json::from_str::<serde_json::Value>(line).unwrap();
        }
        std::fs::remove_file(path).unwrap();
        // A missing device is never created as a regular file.
        let missing = std::env::temp_dir().join(format!("buddy-tty-{}", uuid::Uuid::new_v4()));
        assert!(Device(missing.clone()).open().await.is_err());
        assert!(!missing.exists());
    }

    #[test]
    fn frames_are_every_command_line_in_one_buffer() {
        for mood in Mood::ALL {
            let lines: Vec<u8> = mood.commands().iter().flat_map(FaceCommand::line).collect();
            assert_eq!(mood.frame(), &lines[..], "{mood:?}");
        }
        assert_eq!(
            Mood::Speaking.frame(),
            b"{\"led\":\"speaking\"}\n{\"e\":\"happy\",\"i\":0.7}\n{\"look\":[0.0,0.0]}\n"
        );
        assert_eq!(
            Mood::Sorry.frame(),
            b"{\"led\":\"idle\"}\n{\"e\":\"sad\",\"i\":0.6}\n"
        );
    }

    /// Regression: a 1.5 s `Sorry` flash hid the orange listening LED after
    /// "Hey Pip", and came back once listening ended.
    #[tokio::test(start_paused = true)]
    async fn a_flash_never_hides_listening_speaking_or_muted() {
        let log = Log::default();
        let (face, rx) = Face::new();
        let opener = Memory {
            log: log.clone(),
            fail_opens: 0,
            fail_writes: 0,
        };
        tokio::spawn(run(rx, opener));
        face.set(Mood::Idle);
        face.flash(Mood::Sorry, SORRY);
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(lines(&log)[0], "{\"led\":\"idle\"}\n");
        assert_eq!(lines(&log)[1], "{\"e\":\"sad\",\"i\":0.6}\n");
        log.lock().unwrap().clear();
        // Wake word 100 ms into the apology: listening shows at once.
        face.set(Mood::Listening);
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(lines(&log)[0], "{\"led\":\"listening\"}\n");
        assert_eq!(face.flashing(), None, "listening ends the flash");
        // Listening ends while the old flash would still be running: no apology.
        face.set(Mood::Idle);
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(
            lines(&log).last().unwrap(),
            "{\"e\":\"neutral\",\"i\":0.5}\n"
        );
        // A turn that fails while the user is speaking to Pip, or muted, stays hidden.
        for steady in [Mood::Listening, Mood::Speaking, Mood::Muted] {
            face.set(steady);
            face.flash(Mood::Sorry, SORRY);
            tokio::time::sleep(Duration::from_millis(10)).await;
            assert_eq!(face.flashing(), None);
            let wanted = String::from_utf8(steady.frame().to_vec()).unwrap();
            let shown: String = lines(&log).concat();
            assert!(shown.ends_with(&wanted), "{steady:?}: {shown}");
        }
        assert!(!lines(&log).iter().any(|l| l.contains("sad")));
    }

    fn pty() -> (nix::pty::OpenptyResult, PathBuf) {
        let pty = nix::pty::openpty(None, None).unwrap();
        let path = nix::unistd::ttyname(&pty.slave).unwrap();
        (pty, path)
    }

    /// What `cdc_acm` gives a freshly enumerated port: 9600, cooked, echo, HUPCL.
    fn kernel_defaults(fd: &impl std::os::fd::AsFd) {
        use termios::{InputFlags, LocalFlags, OutputFlags};
        let mut t = termios::tcgetattr(fd).unwrap();
        termios::cfsetspeed(&mut t, BaudRate::B9600).unwrap();
        t.local_flags
            .insert(LocalFlags::ECHO | LocalFlags::ICANON | LocalFlags::ISIG);
        t.output_flags
            .insert(OutputFlags::OPOST | OutputFlags::ONLCR);
        t.input_flags.insert(InputFlags::ICRNL);
        t.control_flags.insert(ControlFlags::HUPCL);
        t.control_flags.remove(ControlFlags::CLOCAL);
        termios::tcsetattr(fd, SetArg::TCSANOW, &t).unwrap();
    }

    #[expect(
        clippy::useless_conversion,
        reason = "cfgetospeed is a u32 on Apple targets and a BaudRate on Linux"
    )]
    fn assert_serial_settings(fd: &impl std::os::fd::AsFd) {
        use termios::{LocalFlags, OutputFlags};
        let t = termios::tcgetattr(fd).unwrap();
        assert_eq!(
            u32::from(termios::cfgetospeed(&t)),
            u32::from(BaudRate::B115200)
        );
        assert!(
            !t.local_flags
                .intersects(LocalFlags::ECHO | LocalFlags::ICANON)
        );
        assert!(!t.output_flags.contains(OutputFlags::OPOST));
        assert!(t.control_flags.contains(ControlFlags::CLOCAL));
        assert!(!t.control_flags.contains(ControlFlags::HUPCL));
    }

    #[tokio::test]
    async fn every_open_applies_raw_115200_clocal_without_hupcl() {
        let (pty, path) = pty();
        for _ in 0..2 {
            kernel_defaults(&pty.slave);
            let sink = Device(path.clone()).open().await.unwrap();
            assert!(matches!(sink, DeviceSink::Poll(_)));
            assert_serial_settings(&pty.slave);
        }
    }

    struct Counting {
        device: Device,
        opens: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl Opener for Counting {
        type Sink = DeviceSink;
        async fn open(&mut self) -> io::Result<DeviceSink> {
            let sink = self.device.open().await?;
            self.opens.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(sink)
        }
    }

    /// Regression: writes ran on the blocking pool, so a stalled port parked a
    /// thread in `write(2)` every heartbeat and kept the old fd open. With a
    /// single blocking thread allowed, a leaked write would starve it.
    #[test]
    fn a_stalled_port_times_out_without_leaking_threads_and_reopens_raw() {
        use std::sync::atomic::Ordering;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let (pty, path) = pty();
        // Nobody reads the master: fill the line until a write would block.
        let filler = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(nix::libc::O_NOCTTY | nix::libc::O_NONBLOCK)
            .open(&path)
            .unwrap();
        let block = [b'x'; 1024];
        while (&filler).write(&block).is_ok() {}
        runtime.block_on(async {
            let opens = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let (face, rx) = Face::new();
            let opener = Counting {
                device: Device(path.clone()),
                opens: opens.clone(),
            };
            let task = tokio::spawn(run(rx, opener));
            face.set(Mood::Listening);
            let started = Instant::now();
            while opens.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            // The board re-enumerates: the kernel resets the line.
            tokio::time::sleep(Duration::from_millis(50)).await;
            kernel_defaults(&pty.slave);
            while opens.load(Ordering::SeqCst) < 3 {
                assert!(started.elapsed() < Duration::from_secs(6), "no reopen");
                let probe = tokio::task::spawn_blocking(|| 7);
                let free = tokio::time::timeout(Duration::from_secs(1), probe).await;
                assert_eq!(free.expect("blocking pool starved").unwrap(), 7);
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            // Two stalls of 500 ms plus two 1 s reopen delays.
            let elapsed = started.elapsed();
            assert!(elapsed < Duration::from_secs(4), "{elapsed:?}");
            assert_serial_settings(&pty.slave);
            drop(face);
            tokio::time::timeout(Duration::from_secs(2), task)
                .await
                .expect("writer task stops with the face")
                .unwrap();
        });
        drop(filler);
        runtime.shutdown_timeout(Duration::from_millis(100));
    }
}

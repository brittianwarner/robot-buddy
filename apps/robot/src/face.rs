//! ESP32-S3 face bridge: one compact JSON object per line over USB serial.
//!
//! The session sets a `Mood`; a writer task turns it into face commands,
//! re-sends it every `HEARTBEAT` (the ESP32 falls back to a calm idle face
//! after 5 s of silence), and reopens the device if it disappears. Only the
//! latest mood matters, so the session never waits on the serial port.
use serde::Serialize;
use std::{future::Future, io, path::PathBuf, time::Duration};
use tokio::{io::AsyncWriteExt, sync::watch, time::Instant};

pub const HEARTBEAT: Duration = Duration::from_millis(1500);
const WRITE_DEADLINE: Duration = Duration::from_millis(500);
const REOPEN_DELAY: Duration = Duration::from_secs(1);
pub const SORRY: Duration = Duration::from_millis(1500);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Expression {
    Neutral,
    Happy,
    Thinking,
    Listening,
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
}

impl Mood {
    pub fn commands(self) -> Vec<FaceCommand> {
        use FaceCommand::{Expression as E, Led as L, Look};
        let (led, e, i) = match self {
            Self::Offline => (Led::Idle, Expression::Sleepy, 0.5),
            Self::Idle => (Led::Idle, Expression::Neutral, 0.5),
            Self::Listening => (Led::Listening, Expression::Listening, 0.8),
            Self::Thinking => (Led::Thinking, Expression::Thinking, 0.6),
            Self::Speaking => (Led::Speaking, Expression::Happy, 0.7),
            Self::Muted => (Led::Muted, Expression::Neutral, 0.3),
            Self::Sorry => (Led::Idle, Expression::Sad, 0.6),
        };
        let mut commands = vec![L { led }, E { e, i }];
        if matches!(self, Self::Listening | Self::Speaking) {
            commands.push(Look { look: [0.0, 0.0] });
        }
        commands
    }
}

/// Conversation facts the face is derived from.
#[derive(Clone, Copy, Debug, Default)]
pub struct Signals {
    pub connected: bool,
    pub muted: bool,
    pub listening: bool,
    pub speaking: bool,
    pub thinking: bool,
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
    } else if s.thinking {
        Mood::Thinking
    } else {
        Mood::Idle
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Target {
    mood: Mood,
    flash: Option<(Mood, Instant)>,
}

impl Target {
    fn current(&self, now: Instant) -> Mood {
        match self.flash {
            Some((mood, until)) if now < until => mood,
            _ => self.mood,
        }
    }
}

/// Cheap, cloneable handle. Without a serial device nothing listens.
#[derive(Clone)]
pub struct Face {
    target: watch::Sender<Target>,
}

impl Face {
    pub fn new() -> (Self, watch::Receiver<Target>) {
        let (target, rx) = watch::channel(Target {
            mood: Mood::Offline,
            flash: None,
        });
        (Self { target }, rx)
    }

    pub fn set(&self, mood: Mood) {
        self.target.send_if_modified(|t| {
            let changed = t.mood != mood;
            t.mood = mood;
            changed
        });
    }

    /// Show `mood` for `duration`, then return to the steady mood.
    pub fn flash(&self, mood: Mood, duration: Duration) {
        self.target
            .send_modify(|t| t.flash = Some((mood, Instant::now() + duration)));
    }

    #[cfg(test)]
    pub fn steady(&self) -> Mood {
        self.target.borrow().mood
    }

    #[cfg(test)]
    pub fn flashing(&self) -> Option<Mood> {
        let target = *self.target.borrow();
        target.flash.map(|(mood, _)| mood)
    }
}

pub trait LineSink: Send {
    fn write_line(&mut self, line: &[u8]) -> impl Future<Output = io::Result<()>> + Send;
}

pub trait Opener: Send + 'static {
    type Sink: LineSink;
    fn open(&mut self) -> impl Future<Output = io::Result<Self::Sink>> + Send;
}

/// A serial device (or any file) opened for writing. Never creates the path.
pub struct Device(pub PathBuf);

pub struct DeviceSink(tokio::fs::File);

impl Opener for Device {
    type Sink = DeviceSink;
    async fn open(&mut self) -> io::Result<DeviceSink> {
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .open(&self.0)
            .await?;
        Ok(DeviceSink(file))
    }
}

impl LineSink for DeviceSink {
    async fn write_line(&mut self, line: &[u8]) -> io::Result<()> {
        self.0.write_all(line).await?;
        self.0.flush().await
    }
}

/// Drive the face until every `Face` handle is dropped.
pub async fn run<O: Opener>(mut target: watch::Receiver<Target>, mut opener: O) {
    let mut sink: Option<O::Sink> = None;
    let mut shown: Option<Mood> = None;
    let mut next_beat = Instant::now();
    let mut reopen_at = Instant::now();
    let mut warned = false;
    loop {
        let wanted = *target.borrow_and_update();
        let now = Instant::now();
        let mood = wanted.current(now);
        if shown != Some(mood) || now >= next_beat {
            if sink.is_none() && now >= reopen_at {
                match opener.open().await {
                    Ok(opened) => {
                        sink = Some(opened);
                        warned = false;
                        tracing::info!("face device open");
                    }
                    Err(error) => {
                        if !warned {
                            tracing::warn!(%error, "face device unavailable; retrying");
                            warned = true;
                        }
                        reopen_at = now + REOPEN_DELAY;
                    }
                }
            }
            match sink.as_mut() {
                Some(device) => match send(device, mood).await {
                    Ok(()) => {
                        shown = Some(mood);
                        next_beat = Instant::now() + HEARTBEAT;
                    }
                    Err(error) => {
                        tracing::warn!(%error, "face write failed; reopening");
                        sink = None;
                        shown = None;
                        reopen_at = Instant::now() + REOPEN_DELAY;
                        next_beat = reopen_at;
                    }
                },
                None => next_beat = reopen_at,
            }
        }
        let mut wake = next_beat;
        if let Some((_, until)) = wanted.flash
            && until > Instant::now()
        {
            wake = wake.min(until);
        }
        tokio::select! {
            changed = target.changed() => if changed.is_err() { return },
            () = tokio::time::sleep_until(wake) => {},
        }
    }
}

async fn send<S: LineSink>(sink: &mut S, mood: Mood) -> io::Result<()> {
    for command in mood.commands() {
        tokio::time::timeout(WRITE_DEADLINE, sink.write_line(&command.line()))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "face write stalled"))??;
    }
    Ok(())
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
        async fn write_line(&mut self, line: &[u8]) -> io::Result<()> {
            if self.fail_writes > 0 {
                self.fail_writes -= 1;
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "re-enumerated"));
            }
            let text = String::from_utf8(line.to_vec()).unwrap();
            self.log.lock().unwrap().push((Instant::now(), text));
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
    }

    #[tokio::test(start_paused = true)]
    async fn heartbeat_keeps_the_esp32_out_of_its_5s_fallback() {
        let log: Log = Default::default();
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
        let log: Log = Default::default();
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
                "{\"e\":\"listening\",\"i\":0.8}\n",
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
        let log: Log = Default::default();
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
}

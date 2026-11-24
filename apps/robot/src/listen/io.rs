//! Local ALSA and whisper.cpp adapters. PCM stays in pipes; neither child inherits credentials.
use super::{
    Config,
    vad::{FRAME, RATE},
};
use anyhow::{Context, Result, ensure};
use std::{
    ffi::{OsStr, OsString},
    io,
    process::{ExitStatus, Stdio},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, ChildStdout, Command},
    time::timeout,
};

const CHILD_ENV: [&str; 7] = [
    "PATH",
    "HOME",
    "XDG_RUNTIME_DIR",
    "ALSA_CONFIG_PATH",
    "LD_LIBRARY_PATH",
    "OMP_NUM_THREADS",
    "OPENBLAS_NUM_THREADS",
];
const EXIT_WAIT: Duration = Duration::from_millis(200);

pub(super) trait Microphone {
    async fn frame(&mut self) -> Result<[i16; FRAME]>;
}

pub(super) trait Recognizer {
    async fn transcript(&self, pcm: &[i16]) -> Result<String>;
}

fn local_command(program: &OsStr) -> Command {
    local_command_with(program, |key| std::env::var_os(key))
}

fn local_command_with(program: &OsStr, lookup: impl Fn(&str) -> Option<OsString>) -> Command {
    let mut cmd = Command::new(program);
    cmd.env_clear().env("LC_ALL", "C");
    for key in CHILD_ENV {
        if let Some(value) = lookup(key) {
            cmd.env(key, value);
        }
    }
    cmd.kill_on_drop(true)
        .stderr(Stdio::null())
        .stdout(Stdio::piped());
    cmd
}

fn format_exit_status(prefix: &str, status: ExitStatus) -> String {
    if let Some(code) = status.code() {
        return format!("{prefix} (exit code {code})");
    }
    #[cfg(unix)]
    if let Some(signal) = std::os::unix::process::ExitStatusExt::signal(&status) {
        return format!("{prefix} (signal {signal})");
    }
    format!("{prefix} (exited)")
}

fn format_io_error(action: &str, err: &io::Error) -> anyhow::Error {
    match err.raw_os_error() {
        Some(code) => anyhow::anyhow!("{action} ({:?}, os error {code})", err.kind()),
        None => anyhow::anyhow!("{action} ({:?})", err.kind()),
    }
}

pub(super) struct Alsa {
    child: Child,
    pipe: ChildStdout,
    bytes: Vec<u8>,
    channel: usize,
    channels: usize,
}

impl Alsa {
    pub fn open(cfg: &Config) -> Result<Self> {
        let mut cmd = local_command("arecord".as_ref());
        cmd.arg("-q")
            .arg("-D")
            .arg(&cfg.device)
            .args(["-t", "raw", "-f", "S16_LE", "-r", "16000", "-c"])
            .arg(cfg.channels.to_string())
            .stdin(Stdio::null());
        Self::spawn(cmd, cfg.channel, cfg.channels)
    }

    fn spawn(mut cmd: Command, channel: usize, channels: usize) -> Result<Self> {
        let mut child = cmd
            .spawn()
            .map_err(|err| format_io_error("starting ALSA capture", &err))?;
        Ok(Self {
            pipe: child.stdout.take().context("capture stdout")?,
            child,
            bytes: vec![0; FRAME * 2 * channels],
            channel,
            channels,
        })
    }

    async fn describe_failure(&mut self, stalled: bool, kind: io::ErrorKind) -> anyhow::Error {
        if stalled {
            if let Ok(Some(status)) = self.child.try_wait() {
                return anyhow::anyhow!("{}", format_exit_status("ALSA capture stalled", status));
            }
            return anyhow::anyhow!("ALSA capture stalled");
        }
        match timeout(EXIT_WAIT, self.child.wait()).await {
            Ok(Ok(status)) => {
                anyhow::anyhow!("{}", format_exit_status("ALSA capture ended", status))
            }
            Ok(Err(err)) => format_io_error("ALSA capture wait failed", &err),
            Err(_) => anyhow::anyhow!("ALSA capture ended ({kind:?}; process still running)"),
        }
    }
}

impl Microphone for Alsa {
    async fn frame(&mut self) -> Result<[i16; FRAME]> {
        match timeout(
            Duration::from_secs(4),
            self.pipe.read_exact(&mut self.bytes),
        )
        .await
        {
            Ok(Ok(_)) => Ok(std::array::from_fn(|i| {
                let pos = 2 * (i * self.channels + self.channel);
                i16::from_le_bytes([self.bytes[pos], self.bytes[pos + 1]])
            })),
            Ok(Err(err)) => Err(self.describe_failure(false, err.kind()).await),
            Err(_) => Err(self.describe_failure(true, io::ErrorKind::TimedOut).await),
        }
    }
}

/// RIFF with a known length: compatible with whisper-cli's nonseekable `-f -` input.
fn wav(samples: &[i16]) -> Result<Vec<u8>> {
    let bytes = u32::try_from(std::mem::size_of_val(samples))?;
    let mut out = Vec::with_capacity(bytes as usize + 44);
    out.extend(b"RIFF");
    out.extend((bytes + 36).to_le_bytes());
    out.extend(b"WAVEfmt ");
    out.extend(16_u32.to_le_bytes());
    out.extend(1_u16.to_le_bytes());
    out.extend(1_u16.to_le_bytes());
    out.extend(RATE.to_le_bytes());
    out.extend((RATE * 2).to_le_bytes());
    out.extend(2_u16.to_le_bytes());
    out.extend(16_u16.to_le_bytes());
    out.extend(b"data");
    out.extend(bytes.to_le_bytes());
    out.extend(samples.iter().flat_map(|v| v.to_le_bytes()));
    Ok(out)
}

async fn run_recognizer(mut cmd: Command, pcm: &[i16]) -> Result<String> {
    let mut child = cmd
        .spawn()
        .map_err(|err| format_io_error("starting whisper-cli", &err))?;
    let mut input = child.stdin.take().context("recognizer stdin")?;
    let stdout = child.stdout.take().context("recognizer stdout")?;
    let bytes = wav(pcm)?;
    let completed = timeout(Duration::from_secs(30), async {
        let write = async move {
            let res = input.write_all(&bytes).await;
            drop(input);
            res
        };
        let read = async move {
            let mut text = Vec::new();
            stdout
                .take(crate::input::MAX_LINE as u64 + 1)
                .read_to_end(&mut text)
                .await
                .map_err(|err| format_io_error("reading recognizer output", &err))?;
            ensure!(
                text.len() <= crate::input::MAX_LINE,
                "oversized recognizer result"
            );
            Ok(text)
        };
        tokio::try_join!(async { Ok(write.await) }, read, async {
            Ok(child.wait().await)
        },)
    })
    .await;
    let (write_res, text, status_res) = if let Ok(res) = completed {
        res?
    } else {
        if let Ok(Some(status)) = child.try_wait() {
            return Err(anyhow::anyhow!(
                "{}",
                format_exit_status("recognizer deadline", status)
            ));
        }
        return Err(anyhow::anyhow!("recognizer deadline"));
    };
    let status = status_res.map_err(|err| format_io_error("waiting for recognizer", &err))?;
    if !status.success() {
        return Err(anyhow::anyhow!(
            "{}",
            format_exit_status("recognizer failed", status)
        ));
    }
    write_res.map_err(|err| format_io_error("writing recognizer input", &err))?;
    // Joining segments only; newlines and JSON are never interpreted as control messages.
    let text = std::str::from_utf8(&text)
        .map_err(|_| anyhow::anyhow!("recognizer returned non-UTF-8 output"))?;
    Ok(text.split_whitespace().collect::<Vec<_>>().join(" "))
}

impl Recognizer for Config {
    async fn transcript(&self, pcm: &[i16]) -> Result<String> {
        let mut cmd = local_command(&self.whisper);
        cmd.arg("-m")
            .arg(&self.model)
            .args(["-l", "en", "-t", "2", "-np", "-nt", "-f", "-"])
            .stdin(Stdio::piped());
        run_recognizer(cmd, pcm).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cli_drains_long_wav_stdin_and_returns_normalized_segments() {
        let config = Config {
            whisper: concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/src/listen/tests/whisper-fixture.sh"
            )
            .into(),
            model: "test-model".into(),
            device: "default".into(),
            channel: 0,
            channels: 1,
            barge_in: true,
            wake: None,
        };
        let result = config.transcript(&vec![1200; 32_000]).await.unwrap();
        assert_eq!(result, "Hey Pip, what time is it?");
    }

    #[test]
    fn wav_has_exact_lengths_rate_and_little_endian_pcm() {
        let clip = wav(&[-1, 256, i16::MIN]).unwrap();
        assert_eq!(&clip[..12], b"RIFF\x2a\0\0\0WAVE");
        assert_eq!(&clip[24..28], &16_000_u32.to_le_bytes());
        assert_eq!(&clip[36..], b"data\x06\0\0\0\xff\xff\x00\x01\x00\x80");
    }

    #[tokio::test]
    async fn local_command_preserves_thread_caps_and_strips_credentials() {
        let output = local_command_with("sh".as_ref(), |key| match key {
            "PATH" => std::env::var_os("PATH"),
            "OMP_NUM_THREADS" => Some("2".into()),
            "OPENBLAS_NUM_THREADS" => Some("1".into()),
            "BUDDY_OPERATOR_TOKEN" => Some("secret-token".into()),
            _ => None,
        })
        .args([
            "-c",
            "printf '%s:%s:%s:%s' \"${LC_ALL:-}\" \"${OMP_NUM_THREADS:-}\" \"${OPENBLAS_NUM_THREADS:-}\" \"${BUDDY_OPERATOR_TOKEN:-}\"",
        ])
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8(output.stdout).unwrap(), "C:2:1:");
    }

    #[tokio::test]
    async fn recognizer_reports_safe_exit_code_signal_and_spawn_errors() {
        // Early non-zero exit must report the exit code instead of broken pipe or stderr text.
        let mut failing = local_command("sh".as_ref());
        failing
            .args(["-c", "printf 'private-stderr' >&2; exit 7"])
            .stdin(Stdio::piped());
        let err = run_recognizer(failing, &vec![1200; 32_000])
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("exit code 7"), "{err}");
        assert!(!err.contains("private-stderr"), "{err}");

        // Signal death reports the signal number safely.
        let mut killed = local_command("sh".as_ref());
        killed
            .args(["-c", "printf 'private-stderr' >&2; kill -9 $$"])
            .stdin(Stdio::piped());
        let err = run_recognizer(killed, &vec![1200; 32_000])
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("signal 9"), "{err}");
        assert!(!err.contains("private-stderr"), "{err}");

        // Missing binary reports safe spawn OS error.
        let missing = Config {
            whisper: "/nonexistent/robot-buddy-whisper-cli".into(),
            model: "test-model".into(),
            device: "default".into(),
            channel: 0,
            channels: 1,
            barge_in: false,
            wake: None,
        };
        let err = missing.transcript(&[0; 16]).await.unwrap_err().to_string();
        assert!(err.contains("starting whisper-cli"), "{err}");
        assert!(err.contains("NotFound"), "{err}");

        // Invalid UTF-8 and oversized output return bounded, safe errors without leaking stdout.
        let mut bad_utf8 = local_command("sh".as_ref());
        bad_utf8
            .args(["-c", "cat >/dev/null; printf '\\377\\376'"])
            .stdin(Stdio::piped());
        let err = run_recognizer(bad_utf8, &[0; 16])
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("non-UTF-8"), "{err}");

        let mut oversized = local_command("sh".as_ref());
        oversized
            .args(["-c", "cat >/dev/null; head -c 9000 /dev/zero | tr '\\0' a"])
            .stdin(Stdio::piped());
        let err = run_recognizer(oversized, &[0; 16])
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("oversized recognizer result"), "{err}");
    }

    #[tokio::test]
    async fn alsa_extracts_selected_channel_and_reports_safe_exit_signal_and_spawn_errors() {
        let mut pcm = local_command("sh".as_ref());
        // 320 interleaved stereo frames: channel 0 = 0x0001 (1), channel 1 = 0x0200 (512).
        pcm.args([
            "-c",
            "i=0; while [ $i -lt 320 ]; do printf '\\001\\000\\000\\002'; i=$((i+1)); done",
        ])
        .stdin(Stdio::null());
        let mut mic = Alsa::spawn(pcm, 1, 2).unwrap();
        assert_eq!(mic.frame().await.unwrap(), [512; FRAME]);

        let mut failing = local_command("sh".as_ref());
        failing
            .args(["-c", "printf 'private-stderr' >&2; exit 42"])
            .stdin(Stdio::null());
        let mut mic = Alsa::spawn(failing, 0, 1).unwrap();
        let err = mic.frame().await.unwrap_err().to_string();
        assert!(err.contains("exit code 42"), "{err}");
        assert!(!err.contains("private-stderr"), "{err}");

        let mut killed = local_command("sh".as_ref());
        killed
            .args(["-c", "printf 'private-stderr' >&2; kill -9 $$"])
            .stdin(Stdio::null());
        let mut mic = Alsa::spawn(killed, 0, 1).unwrap();
        let err = mic.frame().await.unwrap_err().to_string();
        assert!(err.contains("signal 9"), "{err}");
        assert!(!err.contains("private-stderr"), "{err}");

        let mut missing = local_command("/nonexistent/robot-buddy-arecord".as_ref());
        missing.stdin(Stdio::null());
        let err = Alsa::spawn(missing, 0, 1)
            .err()
            .expect("missing binary")
            .to_string();
        assert!(err.contains("starting ALSA capture"), "{err}");
        assert!(err.contains("NotFound"), "{err}");
    }
}

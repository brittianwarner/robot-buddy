//! Optional, device-local status speech. Only the fixed offline line crosses the
//! process boundary. Piper, its phonemiser and the voice are installed separately.
use crate::audio::PlayerConfig;
use anyhow::{Context, Result, bail, ensure};
use robot_buddy::audio::PcmFormat;
use std::{
    ffi::OsStr,
    io::Read as _,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Component, Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, Command},
    sync::watch,
    task::JoinHandle,
    time::Instant,
};

const OFFLINE: &[u8] = b"I'm offline. I'll try again soon.\n";
const CONFIG_LIMIT: u64 = 1024 * 1024;
const ANNOUNCE_DEADLINE: Duration = Duration::from_secs(30);
const REAP_DEADLINE: Duration = Duration::from_secs(1);

#[derive(Clone)]
pub struct OfflineVoice {
    binary: PathBuf,
    model: PathBuf,
    player: PlayerConfig,
    #[cfg(test)]
    test_rate: Option<u32>,
    #[cfg(test)]
    custom: Option<std::sync::Arc<dyn Processes + Send + Sync>>,
}

#[cfg(not(test))]
trait Processes {
    fn synth(&self) -> Command;
    fn player(&self, format: PcmFormat) -> Command;
}

#[cfg(test)]
pub(crate) trait Processes {
    fn synth(&self) -> Command;
    fn player(&self, format: PcmFormat) -> Command;
}

/// Running offline announcement. Dropping it kills the active child; calling
/// [`Self::stop`] cancels and waits until any Piper or `aplay` child is reaped.
pub struct Announcement {
    cancel: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl Announcement {
    /// Resolves when the announcement finishes on its own. Cancel-safe for `tokio::select!`.
    pub async fn finished(&mut self) {
        let _ = (&mut self.task).await;
    }

    /// Cancel the announcement and wait until any active child is killed and reaped.
    pub async fn stop(mut self) {
        let _ = self.cancel.send(true);
        let _ = (&mut self.task).await;
    }
}

impl Drop for Announcement {
    fn drop(&mut self) {
        let _ = self.cancel.send(true);
        self.task.abort();
    }
}

async fn cancelled(cancel: &mut watch::Receiver<bool>) {
    if *cancel.borrow() {
        return;
    }
    let _ = cancel.wait_for(|c| *c).await;
}

async fn reap(child: &mut Child) {
    let _ = child.start_kill();
    let _ = tokio::time::timeout(REAP_DEADLINE, child.wait()).await;
}

// The synth and ALSA only need local runtime configuration, not the operator
// token or any other device service settings.
fn local_process(program: &OsStr) -> Command {
    let mut cmd = Command::new(program);
    cmd.env_clear().env("LC_ALL", "C");
    for key in [
        "PATH",
        "HOME",
        "LD_LIBRARY_PATH",
        "ALSA_CONFIG_PATH",
        "XDG_RUNTIME_DIR",
        "OMP_NUM_THREADS",
        "OPENBLAS_NUM_THREADS",
    ] {
        if let Some(value) = std::env::var_os(key) {
            cmd.env(key, value);
        }
    }
    cmd
}

fn clean_absolute(path: &Path, label: &str) -> Result<()> {
    ensure!(path.is_absolute(), "{label} must be an absolute path");
    ensure!(
        !path.components().any(|c| matches!(c, Component::ParentDir)),
        "{label} must not contain '..'"
    );
    ensure!(path.file_name().is_some(), "{label} must name a file");
    Ok(())
}

fn validate_binary(path: &Path) -> Result<()> {
    clean_absolute(path, "Piper binary")?;
    let meta = std::fs::metadata(path).context("inspect Piper binary")?;
    ensure!(meta.is_file(), "Piper binary must be a regular file");
    ensure!(meta.mode() & 0o111 != 0, "Piper binary must be executable");
    ensure!(
        meta.mode() & 0o002 == 0,
        "Piper binary must not be world-writable"
    );
    Ok(())
}

fn validate_model(path: &Path) -> Result<()> {
    clean_absolute(path, "Piper model")?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)
        .context("open Piper model")?;
    let meta = file.metadata()?;
    ensure!(meta.is_file(), "Piper model must be a regular file");
    ensure!(meta.len() > 0, "Piper model is empty");
    ensure!(
        meta.mode() & 0o002 == 0,
        "Piper model must not be world-writable"
    );
    Ok(())
}

fn read_sidecar_rate(model: &Path) -> Result<u32> {
    let sidecar = model.with_added_extension("json");
    ensure!(sidecar != model, "invalid Piper voice config path");
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(&sidecar)
        .context("open Piper voice config")?;
    let meta = file.metadata()?;
    ensure!(meta.is_file(), "Piper voice config must be a regular file");
    ensure!(
        meta.mode() & 0o002 == 0,
        "Piper voice config must not be world-writable"
    );
    ensure!(meta.len() <= CONFIG_LIMIT, "Piper voice config too large");
    let mut json = Vec::new();
    (&mut file).take(CONFIG_LIMIT + 1).read_to_end(&mut json)?;
    ensure!(
        json.len() as u64 <= CONFIG_LIMIT,
        "Piper voice config too large"
    );
    let config: serde_json::Value = serde_json::from_slice(&json)?;
    let rate = config
        .pointer("/audio/sample_rate")
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .context("missing Piper sample rate")?;
    ensure!(
        (8000..=48000).contains(&rate),
        "unsupported Piper sample rate"
    );
    Ok(rate)
}

impl Processes for OfflineVoice {
    fn synth(&self) -> Command {
        #[cfg(test)]
        if let Some(custom) = &self.custom {
            return custom.synth();
        }
        let mut cmd = local_process(self.binary.as_os_str());
        cmd.arg("--model")
            .arg(&self.model)
            .args(["--output-raw", "--length-scale", "1.18"]);
        cmd
    }

    fn player(&self, format: PcmFormat) -> Command {
        #[cfg(test)]
        if let Some(custom) = &self.custom {
            return custom.player(format);
        }
        let mut cmd = local_process("aplay".as_ref());
        cmd.args(self.player.args(format)).stdout(Stdio::null());
        cmd
    }
}

impl OfflineVoice {
    pub fn from_lookup(
        get: &impl Fn(&str) -> Option<String>,
        player: PlayerConfig,
    ) -> Result<Option<Self>> {
        let Some(path) = get("BUDDY_PIPER_MODEL").filter(|p| !p.is_empty()) else {
            return Ok(None);
        };
        let model = PathBuf::from(path);
        let binary = PathBuf::from(
            get("BUDDY_PIPER_BIN")
                .filter(|p| !p.is_empty())
                .unwrap_or_else(|| "/usr/local/bin/piper".into()),
        );
        validate_binary(&binary)?;
        validate_model(&model)?;
        let _rate = read_sidecar_rate(&model)?;
        Ok(Some(Self {
            binary,
            model,
            player,
            #[cfg(test)]
            test_rate: None,
            #[cfg(test)]
            custom: None,
        }))
    }

    #[cfg(test)]
    pub(crate) fn for_test(rate: u32, custom: std::sync::Arc<dyn Processes + Send + Sync>) -> Self {
        Self {
            binary: PathBuf::from("/usr/local/bin/piper"),
            model: PathBuf::from("/opt/robot-buddy/models/test.onnx"),
            player: PlayerConfig::default(),
            test_rate: Some(rate),
            custom: Some(custom),
        }
    }

    /// Start one cancellable offline announcement on a background task.
    pub fn start(&self) -> Announcement {
        let voice = self.clone();
        let (cancel, mut cancel_rx) = watch::channel(false);
        let task = tokio::spawn(async move {
            if let Err(error) = voice.announce_with_cancel(&mut cancel_rx).await {
                tracing::warn!(%error, "offline voice unavailable");
            }
        });
        Announcement { cancel, task }
    }

    /// Run once per offline transition. Dropping the future kills the active child.
    #[cfg(test)]
    pub async fn announce(&self) -> Result<()> {
        let (_tx, mut rx) = watch::channel(false);
        self.announce_with_cancel(&mut rx).await
    }

    async fn announce_with_cancel(&self, cancel: &mut watch::Receiver<bool>) -> Result<()> {
        if *cancel.borrow() {
            return Ok(());
        }
        let deadline = Instant::now() + ANNOUNCE_DEADLINE;
        let rate = self.load_rate(cancel, deadline).await?;
        let Some(rate) = rate else {
            return Ok(());
        };
        render_with_cancel(self, rate, cancel, deadline).await
    }

    async fn load_rate(
        &self,
        cancel: &mut watch::Receiver<bool>,
        deadline: Instant,
    ) -> Result<Option<u32>> {
        #[cfg(test)]
        if let Some(rate) = self.test_rate {
            return Ok(Some(rate));
        }
        let model = self.model.clone();
        let read = tokio::task::spawn_blocking(move || {
            validate_model(&model)?;
            read_sidecar_rate(&model)
        });
        tokio::select! {
            biased;
            () = cancelled(cancel) => Ok(None),
            () = tokio::time::sleep_until(deadline) => bail!("offline voice deadline"),
            res = read => Ok(Some(res.context("offline voice config task failed")??)),
        }
    }
}

#[cfg(test)]
async fn render(proc: &impl Processes, rate: u32) -> Result<()> {
    let (_tx, mut rx) = watch::channel(false);
    render_with_cancel(proc, rate, &mut rx, Instant::now() + ANNOUNCE_DEADLINE).await
}

async fn render_with_cancel(
    proc: &impl Processes,
    rate: u32,
    cancel: &mut watch::Receiver<bool>,
    deadline: Instant,
) -> Result<()> {
    if *cancel.borrow() {
        return Ok(());
    }
    let mut synth = proc
        .synth()
        .kill_on_drop(true)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("start Piper")?;
    let synthesis = async {
        let mut input = synth.stdin.take().context("Piper stdin")?;
        let stdout = synth.stdout.take().context("Piper stdout")?;
        input.write_all(OFFLINE).await?;
        drop(input);
        // Wait for a successful, whole utterance before opening ALSA. A bad model
        // must not emit half a warning or give an unbounded producer the speaker.
        let limit = u64::from(rate) * 2 * 8;
        let mut pcm = Vec::new();
        stdout.take(limit + 1).read_to_end(&mut pcm).await?;
        ensure!(
            !pcm.is_empty() && pcm.len() as u64 <= limit && pcm.len().is_multiple_of(2),
            "invalid offline PCM"
        );
        ensure!(synth.wait().await?.success(), "Piper failed");
        Ok::<_, anyhow::Error>(pcm)
    };
    let mut pcm = tokio::select! {
        biased;
        () = cancelled(cancel) => {
            reap(&mut synth).await;
            return Ok(());
        }
        () = tokio::time::sleep_until(deadline) => {
            reap(&mut synth).await;
            bail!("offline voice deadline");
        }
        res = synthesis => match res {
            Ok(pcm) => pcm,
            Err(error) => {
                reap(&mut synth).await;
                return Err(error);
            }
        }
    };
    colour(&mut pcm, rate);
    if *cancel.borrow() {
        return Ok(());
    }
    if Instant::now() >= deadline {
        bail!("offline voice deadline");
    }
    // Play 1.3× faster for the higher voice; the longer synthesis compensates
    // for most of the speed change. ALSA plughw handles device-rate conversion.
    let mut player = proc
        .player(PcmFormat {
            sample_rate: rate * 13 / 10,
            channels: 1,
        })
        .kill_on_drop(true)
        .stdin(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("start offline player")?;
    let playback = async {
        let mut input = player.stdin.take().context("player stdin")?;
        input.write_all(&pcm).await?;
        drop(input);
        ensure!(player.wait().await?.success(), "offline player failed");
        Ok::<_, anyhow::Error>(())
    };
    tokio::select! {
        biased;
        () = cancelled(cancel) => {
            reap(&mut player).await;
            Ok(())
        }
        () = tokio::time::sleep_until(deadline) => {
            reap(&mut player).await;
            bail!("offline voice deadline");
        }
        res = playback => match res {
            Ok(()) => Ok(()),
            Err(error) => {
                reap(&mut player).await;
                Err(error)
            }
        }
    }
}

/// Two quiet, short reflections. Walk backwards so each tap uses the original
/// sample; scale by the sum of the weights so full-scale inputs never clip.
fn colour(pcm: &mut [u8], rate: u32) {
    let delay = (rate as usize * 45 / 10000) * 2;
    for i in (0..pcm.len()).step_by(2).rev() {
        let sample = |j| i32::from(i16::from_le_bytes([pcm[j], pcm[j + 1]]));
        let mix = 80 * sample(i)
            + 14 * i.checked_sub(delay).map_or(0, sample)
            + 7 * i.checked_sub(delay * 2).map_or(0, sample);
        let value = i16::try_from(mix / 101).unwrap_or_default();
        pcm[i..i + 2].copy_from_slice(&value.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Mock(&'static str);
    impl Processes for Mock {
        fn synth(&self) -> Command {
            let mut cmd = Command::new("sh");
            cmd.args(["-c", self.0]);
            cmd
        }
        fn player(&self, format: PcmFormat) -> Command {
            assert_eq!(
                format,
                PcmFormat {
                    sample_rate: 28665,
                    channels: 1
                }
            );
            let mut cmd = Command::new("sh");
            cmd.args(["-c", "test \"$(wc -c | tr -d ' ')\" = 4"]);
            cmd
        }
    }

    #[tokio::test]
    async fn speaks_only_housekeeping_line_and_sends_pcm_to_player() {
        render(&Mock("IFS= read -r text && test \"$text\" = \"I'm offline. I'll try again soon.\" && printf '1234'"), 22050).await.unwrap();
    }

    #[tokio::test]
    async fn failed_partial_and_oversized_renders_never_start_playback() {
        // Any attempt to open the mock player with this rate panics.
        // Each script drains the line first so it fails for its own reason, not
        // because an unread stdin turned Piper's write into EPIPE.
        for script in [
            "cat >/dev/null; printf 12; exit 1",
            "cat >/dev/null; printf 1",
            "cat >/dev/null; exit 0",
            "cat >/dev/null; head -c 128002 /dev/zero",
        ] {
            assert!(render(&Mock(script), 8000).await.is_err());
        }
    }

    #[test]
    fn voice_colouring_is_feedforward_and_bounded() {
        let mut bytes = vec![0; 218];
        bytes[..2].copy_from_slice(&10100_i16.to_le_bytes());
        colour(&mut bytes, 8000);
        let impulse: Vec<_> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&b| i16::from_le_bytes(b))
            .collect();
        assert_eq!(
            [impulse[0], impulse[36], impulse[72], impulse[108]],
            [8000, 1400, 700, 0]
        );
    }

    fn setup_piper_fixture(tag: &str) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("buddy-piper-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("piper");
        std::fs::write(&bin, b"#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let model = dir.join("voice.onnx");
        std::fs::write(&model, b"onnx-weights").unwrap();
        std::fs::set_permissions(&model, std::fs::Permissions::from_mode(0o644)).unwrap();
        let sidecar = model.with_added_extension("json");
        std::fs::write(&sidecar, br#"{"audio":{"sample_rate":22050}}"#).unwrap();
        std::fs::set_permissions(&sidecar, std::fs::Permissions::from_mode(0o644)).unwrap();
        (dir, bin, model, sidecar)
    }

    fn lookup_voice(model: Option<&Path>, bin: Option<&Path>) -> Result<Option<OfflineVoice>> {
        let model = model.map(|p| p.display().to_string());
        let bin = bin.map(|p| p.display().to_string());
        OfflineVoice::from_lookup(
            &|key| match key {
                "BUDDY_PIPER_MODEL" => model.clone(),
                "BUDDY_PIPER_BIN" => bin.clone(),
                _ => None,
            },
            PlayerConfig::default(),
        )
    }

    #[test]
    fn startup_validation_rejects_bad_paths_binary_and_model() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let (dir, bin, model, _) = setup_piper_fixture("model");

        assert!(lookup_voice(None, Some(&bin)).unwrap().is_none());
        assert!(lookup_voice(Some(&model), Some(&bin)).unwrap().is_some());

        // Relative paths and parent traversal.
        assert!(lookup_voice(Some(Path::new("voice.onnx")), Some(&bin)).is_err());
        assert!(lookup_voice(Some(&model), Some(Path::new("piper"))).is_err());
        assert!(lookup_voice(Some(&dir.join("../voice.onnx")), Some(&bin)).is_err());
        assert!(lookup_voice(Some(&model), Some(&dir.join("../piper"))).is_err());

        // Non-executable, directory, and world-writable binaries.
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(lookup_voice(Some(&model), Some(&bin)).is_err());
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(lookup_voice(Some(&model), Some(&bin)).is_err());
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(lookup_voice(Some(&model), Some(&dir)).is_err());

        // Empty and world-writable model.
        let empty = dir.join("empty.onnx");
        std::fs::write(&empty, b"").unwrap();
        std::fs::write(
            empty.with_added_extension("json"),
            br#"{"audio":{"sample_rate":22050}}"#,
        )
        .unwrap();
        assert!(lookup_voice(Some(&empty), Some(&bin)).is_err());
        std::fs::set_permissions(&model, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(lookup_voice(Some(&model), Some(&bin)).is_err());
        std::fs::set_permissions(&model, std::fs::Permissions::from_mode(0o644)).unwrap();

        // Symlink model and FIFO model (must fail immediately without blocking).
        let link_model = dir.join("link.onnx");
        symlink(&model, &link_model).unwrap();
        std::fs::write(
            link_model.with_added_extension("json"),
            br#"{"audio":{"sample_rate":22050}}"#,
        )
        .unwrap();
        assert!(lookup_voice(Some(&link_model), Some(&bin)).is_err());
        let fifo_model = dir.join("fifo.onnx");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo_model)
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(
            fifo_model.with_added_extension("json"),
            br#"{"audio":{"sample_rate":22050}}"#,
        )
        .unwrap();
        assert!(lookup_voice(Some(&fifo_model), Some(&bin)).is_err());

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn startup_and_announce_reject_unsafe_or_invalid_sidecar() {
        use std::os::unix::fs::symlink;
        let (dir, bin, model, sidecar) = setup_piper_fixture("sidecar");
        let voice = lookup_voice(Some(&model), Some(&bin)).unwrap().unwrap();

        let no_sidecar = dir.join("nosidecar.onnx");
        std::fs::write(&no_sidecar, b"weights").unwrap();
        assert!(lookup_voice(Some(&no_sidecar), Some(&bin)).is_err());

        for bad_json in [
            b"not json".as_slice(),
            b"{}",
            br#"{"audio":{"sample_rate":4000}}"#,
            br#"{"audio":{"sample_rate":96000}}"#,
        ] {
            std::fs::write(&sidecar, bad_json).unwrap();
            assert!(lookup_voice(Some(&model), Some(&bin)).is_err());
        }
        std::fs::write(
            &sidecar,
            vec![b' '; usize::try_from(CONFIG_LIMIT).unwrap() + 16],
        )
        .unwrap();
        assert!(lookup_voice(Some(&model), Some(&bin)).is_err());

        // Symlink sidecar rejected both at startup and if swapped in before announce().
        let real_json = dir.join("real.json");
        std::fs::write(&real_json, br#"{"audio":{"sample_rate":22050}}"#).unwrap();
        std::fs::remove_file(&sidecar).unwrap();
        symlink(&real_json, &sidecar).unwrap();
        assert!(lookup_voice(Some(&model), Some(&bin)).is_err());
        assert!(voice.announce().await.is_err());

        // FIFO sidecar fails immediately without blocking.
        std::fs::remove_file(&sidecar).unwrap();
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&sidecar)
                .status()
                .unwrap()
                .success()
        );
        assert!(lookup_voice(Some(&model), Some(&bin)).is_err());
        assert!(voice.announce().await.is_err());

        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Readiness handshake: a fixture child is running iff its pid file exists.
    /// Race that against the announcement ending, so a render that fails early
    /// reports itself instead of tripping a wall-clock deadline on a loaded host.
    async fn ready_pid(ann: &mut Announcement, pid_file: &Path, child: &str) -> i32 {
        tokio::select! {
            biased;
            () = ann.finished() => panic!("announcement ended before the {child} started"),
            pid = async {
                loop {
                    if let Ok(text) = std::fs::read_to_string(pid_file)
                        && let Ok(pid) = text.trim().parse::<i32>()
                    {
                        break pid;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            } => pid,
        }
    }

    #[tokio::test]
    async fn stop_cancels_and_reaps_synth_and_player_before_returning() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        struct Scripted {
            synth: String,
            player: String,
            player_called: Arc<AtomicBool>,
        }
        impl Processes for Scripted {
            fn synth(&self) -> Command {
                let mut cmd = Command::new("sh");
                cmd.args(["-c", &self.synth]);
                cmd
            }
            fn player(&self, _: PcmFormat) -> Command {
                self.player_called.store(true, Ordering::SeqCst);
                let mut cmd = Command::new("sh");
                cmd.args(["-c", &self.player]).stdout(Stdio::null());
                cmd
            }
        }
        let dir = std::env::temp_dir().join(format!("buddy-piper-stop-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        // Cancel during synth: synth is reaped and player is never started.
        let synth_pid_file = dir.join("synth.pid");
        let player_called = Arc::new(AtomicBool::new(false));
        let voice = OfflineVoice::for_test(
            22050,
            Arc::new(Scripted {
                // Publish the pid atomically (rename), then become the sleeper.
                synth: format!(
                    "echo $$ > '{0}.tmp' && mv '{0}.tmp' '{0}' && exec sleep 30",
                    synth_pid_file.display()
                ),
                player: "exit 0".into(),
                player_called: player_called.clone(),
            }),
        );
        let mut ann = voice.start();
        let synth_pid = ready_pid(&mut ann, &synth_pid_file, "synth").await;
        ann.stop().await;
        let status = std::process::Command::new("kill")
            .args(["-0", &synth_pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(!status.success(), "synth pid {synth_pid} survived stop()");
        assert!(!player_called.load(Ordering::SeqCst));

        // Cancel during player: player is killed and reaped before stop() returns.
        let player_pid_file = dir.join("player.pid");
        let voice = OfflineVoice::for_test(
            22050,
            Arc::new(Scripted {
                synth: "cat >/dev/null; printf '1234'".into(),
                player: format!(
                    "echo $$ > '{0}.tmp' && mv '{0}.tmp' '{0}' && exec sleep 30",
                    player_pid_file.display()
                ),
                player_called: Arc::new(AtomicBool::new(false)),
            }),
        );
        let mut ann = voice.start();
        let player_pid = ready_pid(&mut ann, &player_pid_file, "player").await;
        ann.stop().await;
        let status = std::process::Command::new("kill")
            .args(["-0", &player_pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(!status.success(), "player pid {player_pid} survived stop()");

        std::fs::remove_dir_all(dir).unwrap();
    }
}

//! Local camera, tracking and operator-controlled face enrollment. No network client.
mod gallery;
#[cfg(feature = "pi-vision")]
mod native;

use anyhow::{Context, Result, bail, ensure};
use gallery::{Feature, Gallery};
use std::{
    env,
    ffi::OsString,
    fs::OpenOptions,
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::{Component, Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    io::AsyncReadExt,
    process::{Child, ChildStdout, Command},
    sync::watch,
    time::timeout,
};

// Pi 5 PiSP aligns *each chroma plane* to 64 bytes. At 320 pixels this
// introduces row padding. 384 / 2 = 192 is already aligned on all three planes.
const WIDTH: u16 = 384;
const HEIGHT: u16 = 288;
const FRAME_BYTES: usize = WIDTH as usize * HEIGHT as usize * 3 / 2;
const MAX_AGE: Duration = Duration::from_secs(2);
const RECOGNIZE_INTERVAL: Duration = Duration::from_secs(1);
const CAMERA_FRAME_TIMEOUT: Duration = Duration::from_secs(8);
const CAMERA_EXIT_WAIT: Duration = Duration::from_millis(500);
const MAX_CONSECUTIVE_INFERENCE_FAILURES: usize = 10;
const INFERENCE_ERROR_LOG_INTERVAL: Duration = Duration::from_secs(10);
const INFERENCE_ACTIVITY_LOG_INTERVAL: Duration = Duration::from_secs(5);
const MIN_AWB_GAIN: f32 = 0.05;
const MAX_AWB_GAIN: f32 = 32.0;
const GIT_LFS_HEADER: &[u8] = b"version https://git-lfs.github.com/spec/";

pub struct Config {
    camera: PathBuf,
    yunet: String,
    sface: Option<String>,
    gallery: PathBuf,
    rotate: bool,
    tuning_file: Option<PathBuf>,
    awb_gains: Option<(f32, f32)>,
}

impl Config {
    pub fn from_env() -> Result<Option<Self>> {
        Self::from_lookup(|key| env::var_os(key))
    }

    fn from_lookup(lookup: impl Fn(&str) -> Option<OsString>) -> Result<Option<Self>> {
        if !switch_from(&lookup, "BUDDY_VISION")? {
            return Ok(None);
        }
        let path_or = |key, fallback: &str| {
            lookup(key).map_or_else(|| PathBuf::from(fallback), PathBuf::from)
        };
        let camera = path_or("BUDDY_VISION_CAMERA", "/usr/bin/rpicam-vid");
        let gallery = path_or("BUDDY_VISION_GALLERY", "/var/lib/robot-buddy/faces");
        let yunet = required_utf8(&lookup, "BUDDY_VISION_YUNET")?;
        let sface = optional_utf8(&lookup, "BUDDY_VISION_SFACE")?;
        let rotate = switch_from(&lookup, "BUDDY_VISION_ROTATE_180")?;
        let tuning_file = lookup("BUDDY_VISION_TUNING_FILE")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from);
        let awb_gains = optional_utf8(&lookup, "BUDDY_VISION_AWB_GAINS")?
            .as_deref()
            .map(parse_awb_gains)
            .transpose()?;

        let config = Self {
            camera,
            yunet,
            sface,
            gallery,
            rotate,
            tuning_file,
            awb_gains,
        };
        config.validate()?;
        Ok(Some(config))
    }

    fn validate(&self) -> Result<()> {
        validate_absolute_path("BUDDY_VISION_CAMERA", &self.camera)?;
        validate_absolute_path("BUDDY_VISION_GALLERY", &self.gallery)?;
        validate_regular_file("BUDDY_VISION_YUNET", Path::new(&self.yunet), true)?;
        if let Some(ref sface_path) = self.sface {
            validate_regular_file("BUDDY_VISION_SFACE", Path::new(sface_path), true)?;
        }
        if let Some(ref tuning_path) = self.tuning_file {
            validate_regular_file("BUDDY_VISION_TUNING_FILE", tuning_path, true)?;
        }
        Ok(())
    }
}

fn switch_from(lookup: &impl Fn(&str) -> Option<OsString>, key: &str) -> Result<bool> {
    let Some(value) = lookup(key) else {
        return Ok(false);
    };
    match value.to_str() {
        Some("1" | "true") => Ok(true),
        Some("0" | "false" | "") => Ok(false),
        _ => bail!("{key} must be 0 or 1"),
    }
}

fn required_utf8(lookup: &impl Fn(&str) -> Option<OsString>, key: &str) -> Result<String> {
    let value = lookup(key).with_context(|| format!("{key} required"))?;
    let text = value
        .into_string()
        .map_err(|_| anyhow::anyhow!("{key} must be valid UTF-8"))?;
    ensure!(!text.is_empty(), "{key} required");
    Ok(text)
}

fn optional_utf8(lookup: &impl Fn(&str) -> Option<OsString>, key: &str) -> Result<Option<String>> {
    let Some(value) = lookup(key) else {
        return Ok(None);
    };
    let text = value
        .into_string()
        .map_err(|_| anyhow::anyhow!("{key} must be valid UTF-8"))?;
    Ok((!text.is_empty()).then_some(text))
}

fn validate_absolute_path(key: &str, path: &Path) -> Result<()> {
    ensure!(path.is_absolute(), "{key} must be an absolute path");
    ensure!(
        !path
            .components()
            .any(|component| matches!(component, Component::ParentDir)),
        "{key} must not contain '..'"
    );
    Ok(())
}

fn is_git_lfs_pointer(head: &[u8]) -> bool {
    let without_bom = head.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(head);
    without_bom.trim_ascii_start().starts_with(GIT_LFS_HEADER)
}

fn validate_regular_file(key: &str, path: &Path, reject_lfs: bool) -> Result<()> {
    validate_absolute_path(key, path)?;
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("{key} must be a readable regular file"))?;
    let meta = file
        .metadata()
        .with_context(|| format!("{key} metadata unavailable"))?;
    ensure!(meta.is_file(), "{key} must be a readable regular file");
    ensure!(meta.len() > 0, "{key} must not be empty");
    if reject_lfs {
        let mut head = Vec::with_capacity(128);
        (&mut file)
            .take(128)
            .read_to_end(&mut head)
            .with_context(|| format!("{key} could not be read"))?;
        ensure!(
            !is_git_lfs_pointer(&head),
            "{key} is a Git LFS pointer, not model bytes"
        );
    }
    Ok(())
}

fn parse_awb_gains(raw: &str) -> Result<(f32, f32)> {
    let (red_raw, blue_raw) = raw
        .split_once(',')
        .context("BUDDY_VISION_AWB_GAINS must be formatted as r,b")?;
    ensure!(
        !blue_raw.contains(','),
        "BUDDY_VISION_AWB_GAINS must be formatted as r,b"
    );
    let red: f32 = red_raw
        .trim()
        .parse()
        .context("BUDDY_VISION_AWB_GAINS red gain must be a finite number")?;
    let blue: f32 = blue_raw
        .trim()
        .parse()
        .context("BUDDY_VISION_AWB_GAINS blue gain must be a finite number")?;
    ensure!(
        red.is_finite()
            && blue.is_finite()
            && (MIN_AWB_GAIN..=MAX_AWB_GAIN).contains(&red)
            && (MIN_AWB_GAIN..=MAX_AWB_GAIN).contains(&blue),
        "BUDDY_VISION_AWB_GAINS values must be finite numbers between {MIN_AWB_GAIN} and {MAX_AWB_GAIN}"
    );
    Ok((red, blue))
}

/// Explicit face-recognition outcome for a single [`Observation`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Recognition {
    /// Recognition did not run on this frame (between intervals or recognition disabled).
    Unchecked,
    /// Recognition ran with no match, or tracking/camera state cleared.
    NoMatch,
    /// Recognition ran and matched an enrolled person in the local gallery.
    Known(String),
}

/// Capture coordinates: right/down positive, in [-1,1].
/// `recognition` distinguishes unchecked frames from active non-matches and matches.
#[derive(Clone, Debug, PartialEq)]
pub struct Observation {
    pub target: Option<(f32, f32)>,
    pub recognition: Recognition,
}

impl Observation {
    fn new(target: Option<(f32, f32)>, recognition: Recognition) -> Self {
        Self {
            target,
            recognition,
        }
    }

    fn cleared() -> Self {
        Self::new(None, Recognition::NoMatch)
    }
}

#[derive(Clone)]
struct Frame {
    captured: Instant,
    pixels: Arc<[u8]>,
}

#[derive(Clone)]
struct SequencedFrame {
    seq: u64,
    frame: Frame,
}

struct CachedJpeg {
    seq: u64,
    bytes: Vec<u8>,
}

struct Detected {
    count: usize,
    // Pixel coordinates of largest valid box; all landmarks stay in the native engine.
    largest: Option<[f32; 4]>,
    feature: Option<Feature>,
}

pub(super) fn valid_box(rect: [f32; 4]) -> bool {
    let [x, y, w, h] = rect;
    rect.iter().all(|v| v.is_finite())
        && w > 0.0
        && h > 0.0
        && (w * h).is_finite()
        && (2.0 * x + w).is_finite()
        && (2.0 * y + h).is_finite()
}

/// Counts valid boxes and selects the largest valid box `(index, rect)`, skipping bad boxes.
#[cfg(any(feature = "pi-vision", test))]
pub(super) fn select_largest_box<I>(boxes: I) -> (usize, Option<(usize, [f32; 4])>)
where
    I: IntoIterator<Item = [f32; 4]>,
{
    let mut count = 0usize;
    let mut best: Option<(usize, [f32; 4], f32)> = None;
    for (index, rect) in boxes.into_iter().enumerate() {
        if !valid_box(rect) {
            continue;
        }
        count += 1;
        let area = rect[2] * rect[3];
        if best.is_none_or(|(_, _, best_area)| area > best_area) {
            best = Some((index, rect, area));
        }
    }
    (count, best.map(|(index, rect, _)| (index, rect)))
}

trait Engine: Send {
    fn detect(&mut self, frame: &Frame, extract: bool) -> Result<Detected>;
}

trait JpegEncoder: Send + Sync {
    fn encode(&self, frame: &Frame) -> Result<Vec<u8>>;
}

trait Camera {
    async fn frame(&mut self) -> Result<Frame>;
}

struct PiCamera {
    process: Child,
    output: ChildStdout,
}

impl PiCamera {
    fn open(cfg: &Config) -> Result<Self> {
        let mut cmd = Command::new(&cfg.camera);
        cmd.args([
            "--nopreview",
            "--timeout",
            "0",
            "--codec",
            "yuv420",
            "--width",
            &WIDTH.to_string(),
            "--height",
            &HEIGHT.to_string(),
            "--framerate",
            "15",
        ]);
        if cfg.rotate {
            cmd.args(["--rotation", "180"]);
        }
        if let Some(tuning) = &cfg.tuning_file {
            cmd.arg("--tuning-file").arg(tuning);
        }
        if let Some((red, blue)) = cfg.awb_gains {
            cmd.args(["--awbgains", &format!("{red},{blue}")]);
        }
        // libcamera doesn't need device credentials or a copy of the panel token.
        cmd.env_clear().env("PATH", "/usr/bin:/bin");
        let mut child = cmd
            .args(["--output", "-"])
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("start Pi camera")?;
        let output = child.stdout.take().context("camera stdout unavailable")?;
        Ok(Self {
            process: child,
            output,
        })
    }

    async fn describe_failure(&mut self, stalled: bool, kind: std::io::ErrorKind) -> anyhow::Error {
        if stalled {
            if let Ok(Some(status)) = self.process.try_wait() {
                return anyhow::anyhow!("{}", format_exit_status("camera stalled", status));
            }
            return anyhow::anyhow!("camera stalled");
        }
        match timeout(CAMERA_EXIT_WAIT, self.process.wait()).await {
            Ok(Ok(status)) => {
                anyhow::anyhow!("{}", format_exit_status("camera stream ended", status))
            }
            Ok(Err(_)) => anyhow::anyhow!("camera stream read failed ({kind:?})"),
            Err(_) => anyhow::anyhow!("camera stream ended ({kind:?}; process still running)"),
        }
    }
}

fn format_exit_status(prefix: &str, status: ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    if let Some(code) = status.code() {
        format!("{prefix} (exit code {code})")
    } else if let Some(signal) = status.signal() {
        format!("{prefix} (signal {signal})")
    } else {
        format!("{prefix} (exited)")
    }
}

impl Camera for PiCamera {
    async fn frame(&mut self) -> Result<Frame> {
        let mut pixels = vec![0u8; FRAME_BYTES];
        match timeout(CAMERA_FRAME_TIMEOUT, self.output.read_exact(&mut pixels)).await {
            Ok(Ok(_)) => Ok(Frame {
                captured: Instant::now(),
                pixels: pixels.into(),
            }),
            Ok(Err(err)) => Err(self.describe_failure(false, err.kind()).await),
            Err(_) => Err(self
                .describe_failure(true, std::io::ErrorKind::TimedOut)
                .await),
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> Result<MutexGuard<'_, T>> {
    m.lock()
        .map_err(|_| anyhow::anyhow!("vision state unavailable"))
}

struct MatchedPerson {
    at: Instant,
    name: String,
}

#[derive(Default)]
struct RecognitionSlot {
    gallery_generation: u64,
    current: Option<MatchedPerson>,
}

struct Live {
    engine: Mutex<Box<dyn Engine>>,
    jpeg_encoder: Arc<dyn JpegEncoder>,
    jpeg_cache: Mutex<Option<CachedJpeg>>,
    gallery: Option<Mutex<Gallery>>,
    recognized: Mutex<RecognitionSlot>,
    next_seq: AtomicU64,
    latest: Mutex<Option<SequencedFrame>>,
}

impl Live {
    fn store_latest(&self, frame: Frame) -> Result<()> {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed) + 1;
        *lock(&self.latest)? = Some(SequencedFrame { seq, frame });
        Ok(())
    }

    fn clear_camera_state(&self) -> Result<bool> {
        let was_live = lock(&self.latest)?.take().is_some();
        *lock(&self.jpeg_cache)? = None;
        lock(&self.recognized)?.current = None;
        Ok(was_live)
    }

    fn clear_recognized(&self) -> Result<()> {
        lock(&self.recognized)?.current = None;
        Ok(())
    }

    fn invalidate_recognition(&self) -> Result<()> {
        let mut slot = lock(&self.recognized)?;
        slot.current = None;
        slot.gallery_generation = slot.gallery_generation.wrapping_add(1);
        Ok(())
    }

    fn gallery_generation(&self) -> Result<u64> {
        Ok(lock(&self.recognized)?.gallery_generation)
    }

    fn current_recognized(&self, max_age: Duration) -> Result<Option<String>> {
        let slot = lock(&self.recognized)?;
        Ok(slot
            .current
            .as_ref()
            .filter(|matched| matched.at.elapsed() < max_age)
            .map(|matched| matched.name.clone()))
    }

    fn latest_frame(&self, max_age: Duration) -> Result<Option<SequencedFrame>> {
        Ok(lock(&self.latest)?
            .as_ref()
            .filter(|f| f.frame.captured.elapsed() <= max_age)
            .cloned())
    }

    fn gallery(&self) -> Result<MutexGuard<'_, Gallery>> {
        lock(self.gallery.as_ref().context("recognition is off")?)
    }

    fn observe(
        &self,
        frame: &Frame,
        recognize: bool,
        max_age: Duration,
    ) -> Result<(Observation, u64)> {
        if frame.captured.elapsed() > max_age {
            let mut slot = lock(&self.recognized)?;
            slot.current = None;
            return Ok((Observation::cleared(), slot.gallery_generation));
        }

        let extract = recognize && self.gallery.is_some();
        let result = lock(&self.engine)?.detect(frame, extract)?;
        let largest = result.largest.filter(|&rect| valid_box(rect));
        let target = largest.map(|[x, y, w, h]| {
            (
                ((2.0 * x + w) / f32::from(WIDTH) - 1.0).clamp(-1.0, 1.0),
                ((2.0 * y + h) / f32::from(HEIGHT) - 1.0).clamp(-1.0, 1.0),
            )
        });

        if frame.captured.elapsed() > max_age {
            let mut slot = lock(&self.recognized)?;
            slot.current = None;
            return Ok((Observation::cleared(), slot.gallery_generation));
        }

        let Some(target_coords) = target else {
            let mut slot = lock(&self.recognized)?;
            slot.current = None;
            return Ok((Observation::cleared(), slot.gallery_generation));
        };

        if extract {
            // Lock order is always gallery -> recognized (same as enroll/forget).
            let gallery = self.gallery()?;
            let matched = result.feature.as_ref().and_then(|f| gallery.identify(f));
            let mut slot = lock(&self.recognized)?;
            let generation = slot.gallery_generation;
            if frame.captured.elapsed() > max_age {
                slot.current = None;
                return Ok((Observation::cleared(), generation));
            }
            let recognition = if let Some(name) = matched {
                slot.current = Some(MatchedPerson {
                    at: frame.captured,
                    name: name.clone(),
                });
                Recognition::Known(name)
            } else {
                slot.current = None;
                Recognition::NoMatch
            };
            Ok((
                Observation::new(Some(target_coords), recognition),
                generation,
            ))
        } else {
            let mut slot = lock(&self.recognized)?;
            if self.gallery.is_none() {
                slot.current = None;
            }
            let generation = slot.gallery_generation;
            Ok((
                Observation::new(Some(target_coords), Recognition::Unchecked),
                generation,
            ))
        }
    }
}

/// Local authenticated panel handle. Methods doing inference or filesystem I/O are blocking.
#[derive(Clone, Default)]
pub struct Panel(Arc<Mutex<Option<Arc<Live>>>>);
impl Panel {
    fn live(&self) -> Result<Arc<Live>> {
        lock(&self.0)?.clone().context("vision is offline")
    }

    /// JPEG is produced only on request, decoupled from the inference engine mutex,
    /// and cached per captured frame. A disconnected or stale camera yields `None`.
    pub fn jpeg(&self) -> Result<Option<Vec<u8>>> {
        let Some(live) = lock(&self.0)?.clone() else {
            return Ok(None);
        };
        let Some(sequenced) = live.latest_frame(MAX_AGE)? else {
            return Ok(None);
        };
        let mut cache = lock(&live.jpeg_cache)?;
        if sequenced.frame.captured.elapsed() > MAX_AGE {
            *cache = None;
            return Ok(None);
        }
        if let Some(cached) = cache.as_ref().filter(|c| c.seq == sequenced.seq) {
            return Ok(Some(cached.bytes.clone()));
        }
        let encoded = live.jpeg_encoder.encode(&sequenced.frame)?;
        if sequenced.frame.captured.elapsed() > MAX_AGE {
            *cache = None;
            return Ok(None);
        }
        *cache = Some(CachedJpeg {
            seq: sequenced.seq,
            bytes: encoded.clone(),
        });
        Ok(Some(encoded))
    }

    /// Current fresh local recognition match, or `None` when offline, cleared, or expired.
    pub fn recognized(&self) -> Result<Option<String>> {
        let Some(live) = lock(&self.0)?.clone() else {
            return Ok(None);
        };
        live.current_recognized(MAX_AGE)
    }

    pub fn people(&self) -> Result<Vec<String>> {
        Ok(self.live()?.gallery()?.names())
    }

    pub fn forget(&self, name: &str) -> Result<()> {
        let name = gallery::key(name)?;
        let live = self.live()?;
        let mut gallery = live.gallery()?;
        ensure!(gallery.contains(&name)?, "unknown gallery entry");
        // Clear any stale recognized label before modifying the on-disk gallery.
        live.invalidate_recognition()?;
        gallery.forget(&name)
    }

    pub fn enroll(&self, name: &str) -> Result<()> {
        let name = gallery::key(name)?;
        let live = self.live()?;
        ensure!(live.gallery.is_some(), "recognition is off");
        let sequenced = live
            .latest_frame(MAX_AGE)?
            .context("no fresh camera frame")?;
        let found = lock(&live.engine)?.detect(&sequenced.frame, true)?;
        ensure!(
            found.count == 1
                && found
                    .largest
                    .is_some_and(|rect| valid_box(rect) && rect[2].min(rect[3]) >= 40.0),
            "show exactly one face at least 40 pixels across"
        );
        ensure!(
            sequenced.frame.captured.elapsed() <= MAX_AGE,
            "enrollment frame expired"
        );
        let feature = found.feature.context("no usable face feature")?;
        let mut gallery = live.gallery()?;
        gallery.check_add(&name)?;
        // Clear any stale recognized label before writing the updated enrollment.
        live.invalidate_recognition()?;
        gallery.add(&name, feature)
    }
}

struct Attached<'a>(&'a Panel);
impl Drop for Attached<'_> {
    fn drop(&mut self) {
        if let Ok(mut slot) = self.0.0.lock()
            && let Some(live) = slot.take()
        {
            let _ = live.clear_camera_state();
        }
    }
}

/// Runs until cancellation or an error. Dropping the future closes the camera and detaches the panel.
pub async fn run(
    cfg: Config,
    panel: Panel,
    mut callback: impl FnMut(Observation) + Send,
) -> Result<()> {
    let (cfg, live) = tokio::task::spawn_blocking(move || -> Result<_> {
        let (engine, jpeg_encoder) = open_engine(&cfg)?;
        let gallery = cfg
            .sface
            .as_ref()
            .map(|_| Gallery::open(&cfg.gallery).map(Mutex::new))
            .transpose()?;
        Ok((
            cfg,
            Arc::new(Live {
                engine: Mutex::new(engine),
                jpeg_encoder,
                jpeg_cache: Mutex::default(),
                gallery,
                recognized: Mutex::default(),
                next_seq: AtomicU64::new(0),
                latest: Mutex::default(),
            }),
        ))
    })
    .await??;
    {
        let mut slot = lock(&panel.0)?;
        ensure!(slot.is_none(), "vision already running");
        *slot = Some(live.clone());
    }
    let _attached = Attached(&panel);
    let mut backoff = 1;
    loop {
        let result = match PiCamera::open(&cfg) {
            Ok(camera) => track(camera, &live, &mut callback).await,
            Err(err) => Err(Stopped::Camera(err)),
        };
        let was_live = live.clear_camera_state()?;
        callback(Observation::cleared());
        match result {
            Ok(()) => return Ok(()),
            Err(Stopped::Inference(err)) => return Err(err),
            Err(Stopped::Camera(err)) => {
                if was_live {
                    backoff = 1;
                }
                tracing::warn!(retry_secs = backoff, error = %err, "camera unavailable");
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                backoff = (2 * backoff).min(30);
            }
        }
    }
}

enum Stopped {
    Camera(anyhow::Error),
    Inference(anyhow::Error),
}

#[derive(Clone, Copy)]
struct InferencePolicy {
    recognize_interval: Duration,
    max_age: Duration,
    max_consecutive_failures: usize,
    error_log_interval: Duration,
    activity_log_interval: Duration,
}

impl Default for InferencePolicy {
    fn default() -> Self {
        Self {
            recognize_interval: RECOGNIZE_INTERVAL,
            max_age: MAX_AGE,
            max_consecutive_failures: MAX_CONSECUTIVE_INFERENCE_FAILURES,
            error_log_interval: INFERENCE_ERROR_LOG_INTERVAL,
            activity_log_interval: INFERENCE_ACTIVITY_LOG_INTERVAL,
        }
    }
}

struct ErrorRateLimiter {
    interval: Duration,
    last_logged: Option<Instant>,
    suppressed: usize,
}

impl ErrorRateLimiter {
    fn new(interval: Duration) -> Self {
        Self {
            interval,
            last_logged: None,
            suppressed: 0,
        }
    }

    fn record_error(&mut self, now: Instant) -> Option<usize> {
        if self
            .last_logged
            .is_none_or(|prev| now.saturating_duration_since(prev) >= self.interval)
        {
            self.last_logged = Some(now);
            Some(std::mem::take(&mut self.suppressed))
        } else {
            self.suppressed = self.suppressed.saturating_add(1);
            None
        }
    }

    fn take_suppressed(&mut self) -> usize {
        std::mem::take(&mut self.suppressed)
    }
}

struct InferenceTracker {
    policy: InferencePolicy,
    last_recognition: Option<(Instant, u64)>,
    consecutive_failures: usize,
    errors: ErrorRateLimiter,
    valid_inferences: u64,
    last_activity_log: Option<Instant>,
}

impl InferenceTracker {
    fn new(policy: InferencePolicy) -> Self {
        Self {
            policy,
            last_recognition: None,
            consecutive_failures: 0,
            errors: ErrorRateLimiter::new(policy.error_log_interval),
            valid_inferences: 0,
            last_activity_log: None,
        }
    }

    fn should_recognize(&self, gallery_gen: u64, now: Instant) -> bool {
        self.last_recognition.is_none_or(|(at, prev_gen)| {
            prev_gen != gallery_gen
                || now.saturating_duration_since(at) >= self.policy.recognize_interval
        })
    }

    fn on_success(
        &mut self,
        live: &Live,
        mut observed: Observation,
        obs_gen: u64,
        captured: Instant,
        now: Instant,
    ) -> Result<Observation> {
        if now.saturating_duration_since(captured) > self.policy.max_age {
            live.clear_recognized()?;
            self.last_recognition = None;
            return Ok(Observation::cleared());
        }

        self.consecutive_failures = 0;
        let suppressed_errors = self.errors.take_suppressed();
        if suppressed_errors > 0 {
            tracing::info!(suppressed_errors, "vision inference recovered");
        }

        self.valid_inferences = self.valid_inferences.saturating_add(1);
        if self.last_activity_log.is_none_or(|prev| {
            now.saturating_duration_since(prev) >= self.policy.activity_log_interval
        }) {
            tracing::info!(
                vision_frames = self.valid_inferences,
                "camera inference active"
            );
            self.last_activity_log = Some(now);
        }

        let current_gen = live.gallery_generation()?;
        if current_gen != obs_gen && matches!(observed.recognition, Recognition::Known(_)) {
            observed = Observation::new(observed.target, Recognition::NoMatch);
            self.last_recognition = None;
            return Ok(observed);
        }

        match &observed.recognition {
            Recognition::Known(_) => {
                self.last_recognition = Some((now, obs_gen));
            }
            Recognition::NoMatch => {
                if observed.target.is_some() {
                    self.last_recognition = Some((now, obs_gen));
                } else {
                    self.last_recognition = None;
                }
            }
            Recognition::Unchecked => {}
        }
        Ok(observed)
    }

    fn on_failure(&mut self, live: &Live, err: &anyhow::Error, now: Instant) -> bool {
        let _ = live.clear_recognized();
        self.last_recognition = None;
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        if let Some(suppressed_errors) = self.errors.record_error(now) {
            tracing::warn!(
                consecutive_failures = self.consecutive_failures,
                suppressed_errors,
                error = %err,
                "vision frame inference failed"
            );
        }
        self.consecutive_failures >= self.policy.max_consecutive_failures
    }
}

async fn track(
    camera: impl Camera,
    live: &Arc<Live>,
    callback: impl FnMut(Observation) + Send,
) -> Result<(), Stopped> {
    track_with_policy(camera, live, InferencePolicy::default(), callback).await
}

async fn track_with_policy(
    mut camera: impl Camera,
    live: &Arc<Live>,
    policy: InferencePolicy,
    mut callback: impl FnMut(Observation) + Send,
) -> Result<(), Stopped> {
    // Drain the camera while OpenCV works; a slow detector drops intermediate frames rather
    // than applying stale motion or building up a queue of faces.
    let (tx, mut rx) = watch::channel(None);
    let capture = async move {
        loop {
            let frame = camera.frame().await?;
            ensure!(
                frame.pixels.len() == FRAME_BYTES,
                "invalid camera frame length"
            );
            live.store_latest(frame.clone())?;
            tx.send_replace(Some(frame));
        }
    };
    let inference = async {
        let mut tracker = InferenceTracker::new(policy);
        loop {
            rx.changed().await?;
            let frame: Frame = rx.borrow_and_update().clone().context("missing frame")?;
            let now = Instant::now();
            let gallery_gen = live.gallery_generation()?;
            let recognize = tracker.should_recognize(gallery_gen, now);
            let service = Arc::clone(live);
            let captured = frame.captured;
            let max_age = policy.max_age;
            let join =
                tokio::task::spawn_blocking(move || service.observe(&frame, recognize, max_age))
                    .await;
            let outcome = match join {
                Ok(res) => res,
                Err(err) if err.is_cancelled() => return Ok(()),
                Err(err) => Err(anyhow::anyhow!("vision worker failed: {err}")),
            };
            let finished = Instant::now();
            match outcome {
                Ok((observed, obs_gen)) => {
                    let emitted =
                        tracker.on_success(live, observed, obs_gen, captured, finished)?;
                    callback(emitted);
                }
                Err(err) => {
                    let exceeded = tracker.on_failure(live, &err, finished);
                    callback(Observation::cleared());
                    if exceeded {
                        return Err(err.context(format!(
                            "vision inference failed {} consecutive frames",
                            tracker.consecutive_failures
                        )));
                    }
                }
            }
        }
    };
    tokio::select! {
        result = capture => Result::<()>::map_err(result, Stopped::Camera),
        result = inference => Result::<()>::map_err(result, Stopped::Inference),
    }
}

#[cfg(feature = "pi-vision")]
fn open_engine(cfg: &Config) -> Result<(Box<dyn Engine>, Arc<dyn JpegEncoder>)> {
    Ok((
        Box::new(native::OpenCv::open(cfg)?),
        Arc::new(native::OpenCvJpeg),
    ))
}
#[cfg(not(feature = "pi-vision"))]
fn open_engine(_: &Config) -> Result<(Box<dyn Engine>, Arc<dyn JpegEncoder>)> {
    bail!("vision requires a pi-vision build")
}

#[cfg(test)]
mod tests;

//! Local camera, tracking and operator-controlled face enrollment. No network client.
mod gallery;
#[cfg(feature = "pi-vision")]
mod native;

use anyhow::{Context, Result, bail, ensure};
use gallery::{Feature, Gallery};
use std::{
    env,
    path::PathBuf,
    process::Stdio,
    sync::{Arc, Mutex, MutexGuard},
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

pub struct Config {
    camera: PathBuf,
    yunet: String,
    sface: Option<String>,
    gallery: PathBuf,
    rotate: bool,
}

impl Config {
    pub fn from_env() -> Result<Option<Self>> {
        if !switch("BUDDY_VISION")? {
            return Ok(None);
        }
        let path =
            |key, fallback| env::var_os(key).map_or_else(|| PathBuf::from(fallback), PathBuf::from);
        let config = Self {
            camera: path("BUDDY_VISION_CAMERA", "/usr/bin/rpicam-vid"),
            yunet: env::var("BUDDY_VISION_YUNET").context("BUDDY_VISION_YUNET required")?,
            sface: env::var("BUDDY_VISION_SFACE")
                .ok()
                .filter(|s| !s.is_empty()),
            gallery: path("BUDDY_VISION_GALLERY", "/var/lib/robot-buddy/faces"),
            rotate: switch("BUDDY_VISION_ROTATE_180")?,
        };
        ensure!(
            config.camera.is_absolute()
                && config.gallery.is_absolute()
                && std::path::Path::new(&config.yunet).is_absolute()
                && config
                    .sface
                    .as_ref()
                    .is_none_or(|p| std::path::Path::new(p).is_absolute()),
            "vision paths must be absolute"
        );
        Ok(Some(config))
    }
}

fn switch(key: &str) -> Result<bool> {
    match env::var(key).as_deref() {
        Ok("1" | "true") => Ok(true),
        Ok("0" | "false" | "") | Err(env::VarError::NotPresent) => Ok(false),
        _ => bail!("{key} must be 0 or 1"),
    }
}

/// Capture coordinates: right/down positive, in [-1,1]. Person is set only on a fresh match.
pub struct Observation {
    pub target: Option<(f32, f32)>,
    pub person: Option<String>,
}

#[derive(Clone)]
struct Frame {
    captured: Instant,
    pixels: Arc<[u8]>,
}

struct Detected {
    count: usize,
    // Pixel coordinates of largest box; all landmarks stay in the native engine.
    largest: Option<[f32; 4]>,
    feature: Option<Feature>,
}

trait Engine: Send {
    fn detect(&mut self, frame: &Frame, extract: bool) -> Result<Detected>;
    fn jpeg(&self, frame: &Frame) -> Result<Vec<u8>>;
}

trait Camera {
    async fn frame(&mut self) -> Result<Frame>;
}

struct PiCamera {
    _process: Child,
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
            _process: child,
            output,
        })
    }
}
impl Camera for PiCamera {
    async fn frame(&mut self) -> Result<Frame> {
        let mut pixels = vec![0u8; FRAME_BYTES];
        timeout(Duration::from_secs(8), self.output.read_exact(&mut pixels))
            .await
            .context("camera stalled")??;
        Ok(Frame {
            captured: Instant::now(),
            pixels: pixels.into(),
        })
    }
}

fn lock<T>(m: &Mutex<T>) -> Result<MutexGuard<'_, T>> {
    m.lock()
        .map_err(|_| anyhow::anyhow!("vision state unavailable"))
}

struct Live {
    engine: Mutex<Box<dyn Engine>>,
    gallery: Option<Mutex<Gallery>>,
    latest: Mutex<Option<Frame>>,
}

impl Live {
    fn frame(&self) -> Result<Option<Frame>> {
        Ok(lock(&self.latest)?
            .as_ref()
            .filter(|f| f.captured.elapsed() <= MAX_AGE)
            .cloned())
    }
    fn gallery(&self) -> Result<MutexGuard<'_, Gallery>> {
        lock(self.gallery.as_ref().context("recognition is off")?)
    }

    fn observe(&self, frame: &Frame, recognize: bool) -> Result<Observation> {
        let result = lock(&self.engine)?.detect(frame, recognize && self.gallery.is_some())?;
        let target = result.largest.map(|[x, y, w, h]| {
            (
                ((2.0 * x + w) / f32::from(WIDTH) - 1.0).clamp(-1.0, 1.0),
                ((2.0 * y + h) / f32::from(HEIGHT) - 1.0).clamp(-1.0, 1.0),
            )
        });
        let person = match result.feature {
            Some(f) => self.gallery()?.identify(&f),
            None => None,
        };
        Ok(Observation { target, person })
    }
}

/// Local authenticated panel handle. Methods doing inference or filesystem I/O are blocking.
#[derive(Clone, Default)]
pub struct Panel(Arc<Mutex<Option<Arc<Live>>>>);
impl Panel {
    fn live(&self) -> Result<Arc<Live>> {
        lock(&self.0)?.clone().context("vision is offline")
    }

    /// JPEG is produced only on request. A disconnected or stale camera yields `None`.
    pub fn jpeg(&self) -> Result<Option<Vec<u8>>> {
        let Some(live) = lock(&self.0)?.clone() else {
            return Ok(None);
        };
        let Some(frame) = live.frame()? else {
            return Ok(None);
        };
        let engine = lock(&live.engine)?;
        if frame.captured.elapsed() > MAX_AGE {
            return Ok(None);
        }
        Ok(Some(engine.jpeg(&frame)?))
    }
    pub fn people(&self) -> Result<Vec<String>> {
        Ok(self.live()?.gallery()?.names())
    }
    pub fn forget(&self, name: &str) -> Result<()> {
        self.live()?.gallery()?.forget(name)
    }
    pub fn enroll(&self, name: &str) -> Result<()> {
        let name = gallery::key(name)?;
        let live = self.live()?;
        ensure!(live.gallery.is_some(), "recognition is off");
        let frame = live.frame()?.context("no fresh camera frame")?;
        let found = lock(&live.engine)?.detect(&frame, true)?;
        ensure!(
            found.count == 1 && found.largest.is_some_and(|[_, _, w, h]| w.min(h) >= 40.0),
            "show exactly one face at least 40 pixels across"
        );
        ensure!(
            frame.captured.elapsed() <= MAX_AGE,
            "enrollment frame expired"
        );
        live.gallery()?
            .add(&name, found.feature.context("no usable face feature")?)
    }
}

struct Attached<'a>(&'a Panel);
impl Drop for Attached<'_> {
    fn drop(&mut self) {
        if let Ok(mut slot) = self.0.0.lock() {
            *slot = None;
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
        let engine = open_engine(&cfg)?;
        let gallery = cfg
            .sface
            .as_ref()
            .map(|_| Gallery::open(&cfg.gallery).map(Mutex::new))
            .transpose()?;
        Ok((
            cfg,
            Arc::new(Live {
                engine: Mutex::new(engine),
                gallery,
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
        let was_live = lock(&live.latest)?.take().is_some();
        callback(Observation {
            target: None,
            person: None,
        });
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

async fn track(
    mut camera: impl Camera,
    live: &Arc<Live>,
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
            *lock(&live.latest)? = Some(frame.clone());
            tx.send_replace(Some(frame));
        }
    };
    let inference = async {
        let mut last = None;
        loop {
            rx.changed().await?;
            let frame: Frame = rx.borrow_and_update().clone().context("missing frame")?;
            let recognize = last.is_none_or(|t: Instant| t.elapsed() >= Duration::from_secs(1));
            if recognize {
                last = Some(Instant::now());
            }
            let service = Arc::clone(live);
            let captured = frame.captured;
            let observed =
                tokio::task::spawn_blocking(move || service.observe(&frame, recognize)).await??;
            if captured.elapsed() <= MAX_AGE {
                callback(observed);
            } else {
                callback(Observation {
                    target: None,
                    person: None,
                });
            }
        }
    };
    tokio::select! {
        result = capture => Result::<()>::map_err(result, Stopped::Camera),
        result = inference => Result::<()>::map_err(result, Stopped::Inference),
    }
}

#[cfg(feature = "pi-vision")]
fn open_engine(cfg: &Config) -> Result<Box<dyn Engine>> {
    Ok(Box::new(native::OpenCv::open(cfg)?))
}
#[cfg(not(feature = "pi-vision"))]
fn open_engine(_: &Config) -> Result<Box<dyn Engine>> {
    bail!("vision requires a pi-vision build")
}

#[cfg(test)]
mod tests;

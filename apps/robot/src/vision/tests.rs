use super::*;
use std::{
    collections::HashMap,
    fs,
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    sync::atomic::{AtomicUsize, Ordering},
};
use tokio::sync::mpsc;
use tracing_subscriber::prelude::*;

pub(super) struct Scratch(pub(super) PathBuf);
impl Scratch {
    pub(super) fn new() -> Self {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let path = env::temp_dir().join(format!(
            "robot-buddy-vision-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn vector(index: usize, magnitude: f32) -> Feature {
    let mut values = [0.0; 128];
    values[index] = magnitude;
    Feature::new(&values).unwrap()
}

struct FakeJpeg {
    calls: Arc<AtomicUsize>,
}

impl FakeJpeg {
    fn new() -> (Arc<Self>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (
            Arc::new(Self {
                calls: Arc::clone(&calls),
            }),
            calls,
        )
    }
}

impl JpegEncoder for FakeJpeg {
    fn encode(&self, frame: &Frame) -> Result<Vec<u8>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(vec![frame.pixels[0]])
    }
}

// Byte 0 controls the synthetic face count; byte 1 selects the feature bucket; no real face data.
struct FakeEngine;
impl Engine for FakeEngine {
    fn detect(&mut self, frame: &Frame, extract: bool) -> Result<Detected> {
        let count = usize::from(frame.pixels[0]);
        let bucket = usize::from(frame.pixels[1]);
        Ok(Detected {
            count,
            largest: (count > 0).then_some([0., 96., 96., 96.]),
            feature: (extract && count > 0).then(|| vector(bucket, 2.0)),
        })
    }
}

fn live_with(
    path: Option<&Scratch>,
    engine: Box<dyn Engine>,
    jpeg: Arc<dyn JpegEncoder>,
) -> Arc<Live> {
    Arc::new(Live {
        engine: Mutex::new(engine),
        jpeg_encoder: jpeg,
        jpeg_cache: Mutex::default(),
        gallery: path.map(|p| Mutex::new(Gallery::open(&p.0).unwrap())),
        recognized: Mutex::default(),
        next_seq: AtomicU64::new(0),
        latest: Mutex::default(),
    })
}

fn live(path: &Scratch) -> Arc<Live> {
    let (jpeg, _) = FakeJpeg::new();
    live_with(Some(path), Box::new(FakeEngine), jpeg)
}

fn frame_with(faces: u8, bucket: u8) -> Frame {
    let mut pixels = vec![faces; FRAME_BYTES];
    pixels[1] = bucket;
    Frame {
        pixels: pixels.into(),
        captured: Instant::now(),
    }
}

fn frame(faces: u8) -> Frame {
    frame_with(faces, 4)
}

fn lookup_map(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
    let map: HashMap<String, OsString> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), OsString::from(v)))
        .collect();
    move |key| map.get(key).cloned()
}

#[test]
fn gallery_roundtrips_samples_and_forgets_without_storing_photos() {
    let path = Scratch::new();
    let mut g = Gallery::open(&path.0).unwrap();
    assert!(Gallery::open(&path.0).is_err(), "single writer");
    g.add("Ada", vector(0, 8.0)).unwrap();
    g.add("ada", vector(1, 0.7)).unwrap();
    assert!(g.add("../p", vector(0, 1.0)).is_err());
    drop(g);
    let meta = fs::metadata(path.0.join("ada.sf1")).unwrap();
    assert_eq!((meta.len(), meta.mode() & 0o777), (1024, 0o600));
    assert_eq!(fs::metadata(&path.0).unwrap().mode() & 0o777, 0o700);
    let mut g = Gallery::open(&path.0).unwrap();
    assert_eq!(g.identify(&vector(1, 2.0)).as_deref(), Some("ada"));
    assert_eq!(g.identify(&vector(2, 3.0)), None);
    g.forget("ADA").unwrap();
    drop(g);
    assert_eq!(
        Gallery::open(&path.0).unwrap().names(),
        Vec::<String>::new()
    );
    assert_eq!(fs::read_dir(&path.0).unwrap().count(), 0);
}

#[test]
fn gallery_rejects_shared_files_invalid_vectors_and_symlinks() {
    let path = Scratch::new();
    let mut g = Gallery::open(&path.0).unwrap();
    g.add("ada", vector(0, 1.0)).unwrap();
    drop(g);
    let file = path.0.join("ada.sf1");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(Gallery::open(&path.0).is_err());
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&file, [0; 512]).unwrap();
    assert!(Gallery::open(&path.0).is_err());
    fs::remove_file(&file).unwrap();
    std::os::unix::fs::symlink("/dev/null", &file).unwrap();
    assert!(Gallery::open(&path.0).is_err());
    assert!(Feature::new(&[f32::NAN; 128]).is_err());
}

#[test]
fn recognition_uses_cosine_and_rejects_a_close_but_below_threshold_match() {
    let path = Scratch::new();
    let mut gallery = Gallery::open(&path.0).unwrap();
    gallery.add("sam", vector(0, 4.0)).unwrap();
    for (cosine, expected) in [(0.419_f32, None), (0.421, Some("sam"))] {
        let mut values = [0.0; 128];
        values[0] = cosine;
        values[1] = (1.0 - cosine * cosine).sqrt();
        assert_eq!(
            gallery.identify(&Feature::new(&values).unwrap()).as_deref(),
            expected
        );
    }
}

#[test]
fn config_validates_model_files_and_rejects_git_lfs_pointers() {
    let scratch = Scratch::new();
    let yunet = scratch.0.join("yunet.onnx");
    let sface = scratch.0.join("sface.onnx");
    let lfs = scratch.0.join("pointer.onnx");
    let bom_lfs = scratch.0.join("bom_pointer.onnx");
    let empty = scratch.0.join("empty.onnx");
    let subdir = scratch.0.join("dir");
    fs::write(&yunet, [0x08, 0x07, 0x12, 0x04]).unwrap();
    fs::write(&sface, [0x08, 0x07, 0x12, 0x05]).unwrap();
    fs::write(
        &lfs,
        b"version https://git-lfs.github.com/spec/v1\noid sha256:abc\nsize 123\n",
    )
    .unwrap();
    fs::write(
        &bom_lfs,
        b"\xEF\xBB\xBFversion https://git-lfs.github.com/spec/v1\noid sha256:abc\nsize 123\n",
    )
    .unwrap();
    fs::write(&empty, b"").unwrap();
    fs::create_dir(&subdir).unwrap();

    assert!(Config::from_lookup(lookup_map(&[])).unwrap().is_none());
    assert!(
        Config::from_lookup(lookup_map(&[("BUDDY_VISION", "0")]))
            .unwrap()
            .is_none()
    );

    let yunet_str = yunet.to_str().unwrap();
    let sface_str = sface.to_str().unwrap();
    let cfg = Config::from_lookup(lookup_map(&[
        ("BUDDY_VISION", "1"),
        ("BUDDY_VISION_YUNET", yunet_str),
        ("BUDDY_VISION_SFACE", sface_str),
    ]))
    .unwrap()
    .unwrap();
    assert_eq!(cfg.yunet, yunet_str);
    assert_eq!(cfg.sface.as_deref(), Some(sface_str));

    for bad_yunet in [
        "relative/yunet.onnx",
        "/opt/../etc/yunet.onnx",
        scratch.0.join("missing.onnx").to_str().unwrap(),
        subdir.to_str().unwrap(),
        empty.to_str().unwrap(),
        lfs.to_str().unwrap(),
        bom_lfs.to_str().unwrap(),
    ] {
        assert!(
            Config::from_lookup(lookup_map(&[
                ("BUDDY_VISION", "1"),
                ("BUDDY_VISION_YUNET", bad_yunet),
            ]))
            .is_err(),
            "expected error for yunet={bad_yunet}"
        );
    }

    let lfs_err = Config::from_lookup(lookup_map(&[
        ("BUDDY_VISION", "1"),
        ("BUDDY_VISION_YUNET", yunet_str),
        ("BUDDY_VISION_SFACE", lfs.to_str().unwrap()),
    ]))
    .err()
    .expect("LFS pointer sface must be rejected");
    assert!(lfs_err.to_string().contains("Git LFS pointer"));
}

#[test]
fn config_validates_noir_tuning_file_and_awb_gains() {
    let scratch = Scratch::new();
    let yunet = scratch.0.join("yunet.onnx");
    let tuning = scratch.0.join("imx219_noir.json");
    let empty_tuning = scratch.0.join("empty.json");
    let lfs_tuning = scratch.0.join("lfs.json");
    fs::write(&yunet, [0x08, 0x07]).unwrap();
    fs::write(&tuning, br#"{"version": 2.0}"#).unwrap();
    fs::write(&empty_tuning, b"").unwrap();
    fs::write(
        &lfs_tuning,
        b"version https://git-lfs.github.com/spec/v1\noid sha256:def\nsize 10\n",
    )
    .unwrap();

    let yunet_str = yunet.to_str().unwrap();
    let tuning_str = tuning.to_str().unwrap();
    let cfg = Config::from_lookup(lookup_map(&[
        ("BUDDY_VISION", "1"),
        ("BUDDY_VISION_YUNET", yunet_str),
        ("BUDDY_VISION_TUNING_FILE", tuning_str),
        ("BUDDY_VISION_AWB_GAINS", "1.5, 1.25"),
    ]))
    .unwrap()
    .unwrap();
    assert_eq!(cfg.tuning_file.as_deref(), Some(tuning.as_path()));
    assert_eq!(cfg.awb_gains, Some((1.5, 1.25)));

    for bad_tuning in [
        "noir.json",
        "/usr/share/../noir.json",
        scratch.0.join("absent.json").to_str().unwrap(),
        scratch.0.to_str().unwrap(),
        empty_tuning.to_str().unwrap(),
        lfs_tuning.to_str().unwrap(),
    ] {
        assert!(
            Config::from_lookup(lookup_map(&[
                ("BUDDY_VISION", "1"),
                ("BUDDY_VISION_YUNET", yunet_str),
                ("BUDDY_VISION_TUNING_FILE", bad_tuning),
            ]))
            .is_err(),
            "expected error for tuning={bad_tuning}"
        );
    }

    for bad_awb in [
        "0,1",
        "-1,1",
        "0.01,1.0",
        "1.0,33.0",
        "NaN,1.0",
        "1.0,inf",
        "1.5",
        "1.5,1.2,1.0",
        "red,blue",
        ",1.2",
    ] {
        assert!(
            Config::from_lookup(lookup_map(&[
                ("BUDDY_VISION", "1"),
                ("BUDDY_VISION_YUNET", yunet_str),
                ("BUDDY_VISION_AWB_GAINS", bad_awb),
            ]))
            .is_err(),
            "expected error for awb={bad_awb}"
        );
    }
}

#[tokio::test]
async fn camera_accepts_a_complete_i420_frame_and_rejects_a_truncated_next_frame() {
    let path = Scratch::new();
    let camera = path.0.join("driver");
    fs::write(&camera, format!("#!/bin/sh\nset -eu\n[ \"$*\" = '--nopreview --timeout 0 --codec yuv420 --width 384 --height 288 --framerate 15 --rotation 180 --output -' ]\ndd if=/dev/zero bs={FRAME_BYTES} count=1 2>/dev/null\nprintf 'tail'\n")).unwrap();
    fs::set_permissions(&camera, fs::Permissions::from_mode(0o700)).unwrap();
    let cfg = Config {
        camera,
        gallery: path.0.clone(),
        rotate: true,
        yunet: String::new(),
        sface: None,
        tuning_file: None,
        awb_gains: None,
    };
    let mut capture = PiCamera::open(&cfg).unwrap();
    assert_eq!(capture.frame().await.unwrap().pixels.len(), 165_888);
    let err = capture.frame().await.err().expect("truncated frame fails");
    assert!(
        err.to_string().contains("exit code 0"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn camera_passes_noir_options_and_reports_bounded_exit_status_without_private_data() {
    let path = Scratch::new();
    let tuning = path.0.join("noir.json");
    fs::write(&tuning, b"{}").unwrap();

    // Verify tuning-file and awbgains arguments, non-zero exit code, and stderr suppression.
    let exit_driver = path.0.join("driver-exit");
    let expected_args = format!(
        "--nopreview --timeout 0 --codec yuv420 --width 384 --height 288 --framerate 15 --tuning-file {} --awbgains 1.5,1.25 --output -",
        tuning.display()
    );
    fs::write(
        &exit_driver,
        format!(
            "#!/bin/sh\nset -eu\n[ \"$*\" = '{expected_args}' ]\necho 'PRIVATE_STDERR_SECRET_VALUE' >&2\nexit 23\n"
        ),
    )
    .unwrap();
    fs::set_permissions(&exit_driver, fs::Permissions::from_mode(0o700)).unwrap();

    let cfg = Config {
        camera: exit_driver,
        gallery: path.0.clone(),
        rotate: false,
        yunet: String::new(),
        sface: None,
        tuning_file: Some(tuning),
        awb_gains: Some((1.5, 1.25)),
    };
    let mut capture = PiCamera::open(&cfg).unwrap();
    let err = capture.frame().await.err().expect("camera exit fails");
    let message = err.to_string();
    assert!(
        message.contains("exit code 23"),
        "expected exit code 23, got: {message}"
    );
    assert!(
        !message.contains("PRIVATE_STDERR_SECRET_VALUE"),
        "private stderr must not leak: {message}"
    );

    // Verify signal termination reporting.
    let sig_driver = path.0.join("driver-sig");
    fs::write(&sig_driver, "#!/bin/sh\nkill -TERM $$\n").unwrap();
    fs::set_permissions(&sig_driver, fs::Permissions::from_mode(0o700)).unwrap();
    let sig_cfg = Config {
        camera: sig_driver,
        gallery: path.0.clone(),
        rotate: false,
        yunet: String::new(),
        sface: None,
        tuning_file: None,
        awb_gains: None,
    };
    let mut capture = PiCamera::open(&sig_cfg).unwrap();
    let err = capture.frame().await.err().expect("signaled camera fails");
    assert!(
        err.to_string().contains("signal 15"),
        "expected SIGTERM signal 15, got: {err}"
    );
}

#[test]
fn bad_boxes_are_skipped_without_failing_detection() {
    struct BadBoxEngine;
    impl Engine for BadBoxEngine {
        fn detect(&mut self, _frame: &Frame, _extract: bool) -> Result<Detected> {
            Ok(Detected {
                count: 1,
                largest: Some([f32::NAN, 10.0, 50.0, 50.0]),
                feature: Some(vector(4, 1.0)),
            })
        }
    }

    let boxes = [
        [f32::NAN, 10.0, 50.0, 50.0],
        [0.0, 0.0, -10.0, 50.0],
        [0.0, 0.0, 0.0, 50.0],
        [f32::INFINITY, 0.0, 50.0, 50.0],
        [10.0, 20.0, 40.0, 40.0],
        [50.0, 60.0, 80.0, 80.0],
        [0.0, 0.0, f32::MAX, f32::MAX],
    ];
    let (count, selected) = select_largest_box(boxes);
    assert_eq!(count, 2);
    assert_eq!(selected, Some((5, [50.0, 60.0, 80.0, 80.0])));

    let (bad_count, bad_selected) = select_largest_box([
        [f32::NAN, 0.0, 10.0, 10.0],
        [0.0, 0.0, -1.0, 10.0],
        [0.0, 0.0, 10.0, 0.0],
    ]);
    assert_eq!(bad_count, 0);
    assert_eq!(bad_selected, None);

    // Also verify Live::observe clears tracking and recognition if an engine returns a bad box.
    let path = Scratch::new();
    let (jpeg, _) = FakeJpeg::new();
    let live = live_with(Some(&path), Box::new(BadBoxEngine), jpeg);
    live.gallery().unwrap().add("ada", vector(4, 1.0)).unwrap();
    let (obs, _) = live.observe(&frame(1), true, MAX_AGE).unwrap();
    assert_eq!(obs.target, None);
    assert_eq!(obs.recognition, Recognition::NoMatch);
    assert_eq!(live.current_recognized(MAX_AGE).unwrap(), None);
}

#[test]
fn jpeg_is_decoupled_from_engine_mutex_and_avoids_reencoding_same_frame() {
    let path = Scratch::new();
    let (jpeg, calls) = FakeJpeg::new();
    let live = live_with(Some(&path), Box::new(FakeEngine), jpeg);
    let panel = Panel::default();
    *panel.0.lock().unwrap() = Some(Arc::clone(&live));
    let _attached = Attached(&panel);

    live.store_latest(frame(7)).unwrap();

    // Hold the inference engine mutex across JPEG requests to prove decoupling.
    let _busy_engine = live.engine.lock().unwrap();
    assert_eq!(panel.jpeg().unwrap(), Some(vec![7]));
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // Repeating on the same captured frame serves the cached JPEG without re-encoding.
    assert_eq!(panel.jpeg().unwrap(), Some(vec![7]));
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // A new frame invalidates the cache and encodes once.
    live.store_latest(frame(9)).unwrap();
    assert_eq!(panel.jpeg().unwrap(), Some(vec![9]));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(panel.jpeg().unwrap(), Some(vec![9]));
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    // An expired frame yields None without encoding.
    let mut expired = frame(3);
    expired.captured -= Duration::from_secs(3);
    live.store_latest(expired).unwrap();
    assert_eq!(panel.jpeg().unwrap(), None);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[test]
fn panel_enrolls_only_one_fresh_face_clears_stale_labels_and_loses_access_on_shutdown() {
    let path = Scratch::new();
    let live = live(&path);
    let panel = Panel::default();
    *panel.0.lock().unwrap() = Some(Arc::clone(&live));
    let attached = Attached(&panel);

    let mut expired = frame(1);
    expired.captured -= Duration::from_secs(3);
    for invalid in [frame(0), frame(2), expired] {
        live.store_latest(invalid).unwrap();
        assert!(panel.enroll("Ada").is_err());
    }
    assert!(panel.jpeg().unwrap().is_none());
    assert_eq!(panel.recognized().unwrap(), None);

    live.store_latest(frame_with(1, 4)).unwrap();
    panel.enroll("Ada").unwrap();
    assert_eq!(panel.people().unwrap(), ["ada"]);
    assert_eq!(panel.jpeg().unwrap(), Some(vec![1]));

    let (obs, _) = live.observe(&frame_with(1, 4), true, MAX_AGE).unwrap();
    assert_eq!(obs.recognition, Recognition::Known("ada".into()));
    assert_eq!(panel.recognized().unwrap().as_deref(), Some("ada"));

    // Re-enrolling clears the stale recognized label immediately.
    live.store_latest(frame_with(1, 5)).unwrap();
    panel.enroll("Bea").unwrap();
    assert_eq!(panel.recognized().unwrap(), None);

    let (obs, _) = live.observe(&frame_with(1, 5), true, MAX_AGE).unwrap();
    assert_eq!(obs.recognition, Recognition::Known("bea".into()));
    assert_eq!(panel.recognized().unwrap().as_deref(), Some("bea"));

    // Forgetting clears the stale recognized label immediately.
    panel.forget("bea").unwrap();
    assert_eq!(panel.recognized().unwrap(), None);

    let (obs, _) = live.observe(&frame_with(1, 5), true, MAX_AGE).unwrap();
    assert_eq!(obs.recognition, Recognition::NoMatch);
    assert_eq!(panel.recognized().unwrap(), None);

    // Expired observation clears an existing recognized label immediately.
    let (obs, _) = live.observe(&frame_with(1, 4), true, MAX_AGE).unwrap();
    assert_eq!(obs.recognition, Recognition::Known("ada".into()));
    assert_eq!(panel.recognized().unwrap().as_deref(), Some("ada"));
    let mut stale_frame = frame_with(1, 4);
    stale_frame.captured -= Duration::from_secs(3);
    let (obs, _) = live.observe(&stale_frame, true, MAX_AGE).unwrap();
    assert_eq!(obs.target, None);
    assert_eq!(obs.recognition, Recognition::NoMatch);
    assert_eq!(panel.recognized().unwrap(), None);

    // An expired stored match is never served by Panel::recognized().
    {
        let mut slot = live.recognized.lock().unwrap();
        slot.current = Some(MatchedPerson {
            at: Instant::now().checked_sub(Duration::from_secs(3)).unwrap(),
            name: "ada".into(),
        });
    }
    assert_eq!(panel.recognized().unwrap(), None);

    // A stale observation whose gallery generation predates a forget/enroll is downgraded to NoMatch.
    let mut tracker = InferenceTracker::new(InferencePolicy::default());
    let old_gen = live.gallery_generation().unwrap();
    live.invalidate_recognition().unwrap();
    let now = Instant::now();
    let downgraded = tracker
        .on_success(
            &live,
            Observation::new(Some((0.0, 0.0)), Recognition::Known("ada".into())),
            old_gen,
            now,
            now,
        )
        .unwrap();
    assert_eq!(downgraded.recognition, Recognition::NoMatch);
    assert_eq!(panel.recognized().unwrap(), None);

    drop(attached);
    assert!(panel.jpeg().unwrap().is_none());
    assert_eq!(panel.recognized().unwrap(), None);
    assert!(panel.enroll("another").is_err());
}

struct FakeCamera(mpsc::Receiver<Frame>);
impl Camera for FakeCamera {
    async fn frame(&mut self) -> Result<Frame> {
        self.0.recv().await.context("camera unplugged")
    }
}

#[tokio::test]
async fn capture_drives_tracking_lost_face_and_disconnect() {
    let path = Scratch::new();
    let live = live(&path);
    let (tx, rx) = mpsc::channel(1);
    let (seen, mut updates) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        track(FakeCamera(rx), &live, move |o| {
            let _ = seen.send(o);
        })
        .await
    });
    tx.send(frame(1)).await.unwrap();
    let first = updates.recv().await.unwrap();
    assert_eq!(first.target, Some((-0.75, 0.0)));
    assert_eq!(first.recognition, Recognition::NoMatch);
    tx.send(frame(0)).await.unwrap();
    let second = updates.recv().await.unwrap();
    assert_eq!(second.target, None);
    assert_eq!(second.recognition, Recognition::NoMatch);
    drop(tx);
    assert!(matches!(task.await.unwrap(), Err(Stopped::Camera(_))));
}

#[tokio::test]
async fn recognition_intervals_distinguish_unchecked_no_match_and_known_and_react_to_forget() {
    struct CountingEngine {
        extractions: Arc<AtomicUsize>,
    }
    impl Engine for CountingEngine {
        fn detect(&mut self, frame: &Frame, extract: bool) -> Result<Detected> {
            let count = usize::from(frame.pixels[0]);
            let bucket = usize::from(frame.pixels[1]);
            if extract && count > 0 {
                self.extractions.fetch_add(1, Ordering::SeqCst);
            }
            Ok(Detected {
                count,
                largest: (count > 0).then_some([0., 96., 96., 96.]),
                feature: (extract && count > 0).then(|| vector(bucket, 2.0)),
            })
        }
    }

    let path = Scratch::new();
    let extractions = Arc::new(AtomicUsize::new(0));
    let (jpeg, _) = FakeJpeg::new();
    let live = live_with(
        Some(&path),
        Box::new(CountingEngine {
            extractions: Arc::clone(&extractions),
        }),
        jpeg,
    );
    live.gallery().unwrap().add("ada", vector(4, 2.0)).unwrap();
    let panel = Panel::default();
    *panel.0.lock().unwrap() = Some(Arc::clone(&live));
    let _attached = Attached(&panel);

    let (tx, rx) = mpsc::channel(1);
    let (seen, mut updates) = mpsc::unbounded_channel();
    let policy = InferencePolicy {
        recognize_interval: Duration::from_millis(40),
        max_age: MAX_AGE,
        max_consecutive_failures: 3,
        error_log_interval: Duration::from_secs(10),
        activity_log_interval: Duration::from_secs(5),
    };
    let live_task = Arc::clone(&live);
    let task = tokio::spawn(async move {
        track_with_policy(FakeCamera(rx), &live_task, policy, move |o| {
            let _ = seen.send(o);
        })
        .await
    });

    // 1. First face triggers recognition immediately -> Known("ada").
    tx.send(frame_with(1, 4)).await.unwrap();
    let obs = updates.recv().await.unwrap();
    assert_eq!(obs.recognition, Recognition::Known("ada".into()));
    assert_eq!(panel.recognized().unwrap().as_deref(), Some("ada"));
    assert_eq!(extractions.load(Ordering::SeqCst), 1);

    // 2. Immediate next frame within interval skips SFace -> Unchecked, retains panel match.
    tx.send(frame_with(1, 4)).await.unwrap();
    let obs = updates.recv().await.unwrap();
    assert_eq!(obs.recognition, Recognition::Unchecked);
    assert_eq!(obs.target, Some((-0.75, 0.0)));
    assert_eq!(panel.recognized().unwrap().as_deref(), Some("ada"));
    assert_eq!(extractions.load(Ordering::SeqCst), 1);

    // 3. After interval elapses, an unknown face actively clears stale match -> NoMatch.
    tokio::time::sleep(Duration::from_millis(55)).await;
    tx.send(frame_with(1, 9)).await.unwrap();
    let obs = updates.recv().await.unwrap();
    assert_eq!(obs.recognition, Recognition::NoMatch);
    assert_eq!(obs.target, Some((-0.75, 0.0)));
    assert_eq!(panel.recognized().unwrap(), None);
    assert_eq!(extractions.load(Ordering::SeqCst), 2);

    // 4. Subsequent frame of unknown face within interval does not spam SFace -> Unchecked.
    tx.send(frame_with(1, 9)).await.unwrap();
    let obs = updates.recv().await.unwrap();
    assert_eq!(obs.recognition, Recognition::Unchecked);
    assert_eq!(panel.recognized().unwrap(), None);
    assert_eq!(extractions.load(Ordering::SeqCst), 2);

    // 5. Face leaves -> NoMatch and resets interval so next arrival checks immediately.
    tx.send(frame_with(0, 0)).await.unwrap();
    let obs = updates.recv().await.unwrap();
    assert_eq!(obs.target, None);
    assert_eq!(obs.recognition, Recognition::NoMatch);

    // 6. Ada returns immediately -> recognized without waiting out the interval.
    tx.send(frame_with(1, 4)).await.unwrap();
    let obs = updates.recv().await.unwrap();
    assert_eq!(obs.recognition, Recognition::Known("ada".into()));
    assert_eq!(panel.recognized().unwrap().as_deref(), Some("ada"));
    assert_eq!(extractions.load(Ordering::SeqCst), 3);

    // 7. Forget clears immediately and forces re-check on the next frame even inside interval.
    panel.forget("ada").unwrap();
    assert_eq!(panel.recognized().unwrap(), None);
    tx.send(frame_with(1, 4)).await.unwrap();
    let obs = updates.recv().await.unwrap();
    assert_eq!(obs.recognition, Recognition::NoMatch);
    assert_eq!(panel.recognized().unwrap(), None);
    assert_eq!(extractions.load(Ordering::SeqCst), 4);

    drop(tx);
    assert!(matches!(task.await.unwrap(), Err(Stopped::Camera(_))));
}

#[tokio::test]
async fn per_frame_inference_errors_recover_then_stop_at_bounded_limit() {
    // Byte 0 == 255 triggers a simulated inference error; otherwise delegates to FakeEngine.
    struct FlakyEngine;
    impl Engine for FlakyEngine {
        fn detect(&mut self, frame: &Frame, extract: bool) -> Result<Detected> {
            if frame.pixels[0] == 255 {
                bail!("transient detector fault");
            }
            FakeEngine.detect(frame, extract)
        }
    }

    let path = Scratch::new();
    let (jpeg, _) = FakeJpeg::new();
    let live = live_with(Some(&path), Box::new(FlakyEngine), jpeg);
    live.gallery().unwrap().add("ada", vector(4, 2.0)).unwrap();
    let panel = Panel::default();
    *panel.0.lock().unwrap() = Some(Arc::clone(&live));
    let _attached = Attached(&panel);

    let (tx, rx) = mpsc::channel(1);
    let (seen, mut updates) = mpsc::unbounded_channel();
    let policy = InferencePolicy {
        recognize_interval: Duration::from_secs(60),
        max_age: MAX_AGE,
        max_consecutive_failures: 3,
        error_log_interval: Duration::from_secs(60),
        activity_log_interval: Duration::from_secs(60),
    };
    let live_task = Arc::clone(&live);
    let task = tokio::spawn(async move {
        track_with_policy(FakeCamera(rx), &live_task, policy, move |o| {
            let _ = seen.send(o);
        })
        .await
    });

    // Healthy recognized frame.
    tx.send(frame_with(1, 4)).await.unwrap();
    let obs = updates.recv().await.unwrap();
    assert_eq!(obs.recognition, Recognition::Known("ada".into()));
    assert_eq!(panel.recognized().unwrap().as_deref(), Some("ada"));

    // Two transient failures (< limit of 3): clear target & recognition, stay alive.
    for _ in 0..2 {
        tx.send(frame_with(255, 4)).await.unwrap();
        let obs = updates.recv().await.unwrap();
        assert_eq!(obs.target, None);
        assert_eq!(obs.recognition, Recognition::NoMatch);
        assert_eq!(panel.recognized().unwrap(), None);
    }

    // Recovery frame resets consecutive failures and re-recognizes immediately.
    tx.send(frame_with(1, 4)).await.unwrap();
    let obs = updates.recv().await.unwrap();
    assert_eq!(obs.recognition, Recognition::Known("ada".into()));
    assert_eq!(panel.recognized().unwrap().as_deref(), Some("ada"));

    // Three consecutive failures reach the bounded limit and terminate inference.
    for _ in 0..3 {
        tx.send(frame_with(255, 4)).await.unwrap();
        let obs = updates.recv().await.unwrap();
        assert_eq!(obs.target, None);
        assert_eq!(obs.recognition, Recognition::NoMatch);
        assert_eq!(panel.recognized().unwrap(), None);
    }

    let stopped = task.await.unwrap();
    assert!(matches!(stopped, Err(Stopped::Inference(_))));
}

#[test]
fn error_rate_limiter_suppresses_bursts_and_reports_suppressed_count() {
    let start = Instant::now();
    let mut limiter = ErrorRateLimiter::new(Duration::from_secs(10));
    assert_eq!(limiter.record_error(start), Some(0));
    assert_eq!(limiter.record_error(start + Duration::from_secs(1)), None);
    assert_eq!(limiter.record_error(start + Duration::from_secs(5)), None);
    assert_eq!(
        limiter.record_error(start + Duration::from_secs(10)),
        Some(2)
    );
    assert_eq!(limiter.record_error(start + Duration::from_secs(12)), None);
    assert_eq!(limiter.take_suppressed(), 1);
    assert_eq!(limiter.take_suppressed(), 0);
}

#[test]
fn inference_activity_log_emits_once_per_interval_only_on_valid_inference_without_private_data() {
    #[derive(Default)]
    struct Capture(Arc<Mutex<Vec<String>>>);
    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            struct Visitor(String);
            impl tracing::field::Visit for Visitor {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    use std::fmt::Write;
                    let _ = write!(self.0, "{}={value:?} ", field.name());
                }
            }
            let mut visitor = Visitor(String::new());
            event.record(&mut visitor);
            self.0.lock().unwrap().push(visitor.0);
        }
    }

    let path = Scratch::new();
    let live = live(&path);
    let logs = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(Capture(Arc::clone(&logs)));

    tracing::subscriber::with_default(subscriber, || {
        let mut tracker = InferenceTracker::new(InferencePolicy::default());
        let t0 = Instant::now();

        // Failure never emits "camera inference active".
        let _ = tracker.on_failure(&live, &anyhow::anyhow!("boom"), t0);
        assert!(
            logs.lock()
                .unwrap()
                .iter()
                .all(|line| !line.contains("camera inference active"))
        );

        // Expired frame never emits "camera inference active".
        let expired = t0.checked_sub(Duration::from_secs(5)).unwrap();
        let _ = tracker
            .on_success(
                &live,
                Observation::new(Some((0.25, -0.5)), Recognition::Known("ada".into())),
                0,
                expired,
                t0,
            )
            .unwrap();
        assert!(
            logs.lock()
                .unwrap()
                .iter()
                .all(|line| !line.contains("camera inference active"))
        );

        // First valid inference emits vision_frames=1 without coordinates or names.
        let _ = tracker
            .on_success(
                &live,
                Observation::new(Some((0.25, -0.5)), Recognition::Known("ada".into())),
                0,
                t0,
                t0,
            )
            .unwrap();
        // Second valid inference 2s later is suppressed by the 5s interval.
        let t1 = t0 + Duration::from_secs(2);
        let _ = tracker
            .on_success(
                &live,
                Observation::new(Some((0.25, -0.5)), Recognition::Unchecked),
                0,
                t1,
                t1,
            )
            .unwrap();
        // Third valid inference at 5s emits vision_frames=3.
        let t2 = t0 + Duration::from_secs(5);
        let _ = tracker
            .on_success(
                &live,
                Observation::new(Some((0.25, -0.5)), Recognition::Known("ada".into())),
                0,
                t2,
                t2,
            )
            .unwrap();
    });

    let active_lines: Vec<String> = logs
        .lock()
        .unwrap()
        .iter()
        .filter(|line| line.contains("camera inference active"))
        .cloned()
        .collect();
    assert_eq!(active_lines.len(), 2, "got logs: {active_lines:?}");
    assert!(active_lines[0].contains("vision_frames=1"));
    assert!(active_lines[1].contains("vision_frames=3"));
    for line in &active_lines {
        assert!(!line.contains("ada"), "must not log names: {line}");
        assert!(!line.contains("0.25"), "must not log coords: {line}");
        assert!(!line.contains("-0.5"), "must not log coords: {line}");
    }
}

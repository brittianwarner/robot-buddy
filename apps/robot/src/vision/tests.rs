use super::*;
use std::{
    fs,
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    sync::atomic::{AtomicUsize, Ordering},
};
use tokio::sync::mpsc;

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
    };
    let mut capture = PiCamera::open(&cfg).unwrap();
    assert_eq!(capture.frame().await.unwrap().pixels.len(), 165_888);
    assert!(capture.frame().await.is_err());
}

// The first Y sample controls the synthetic face count; no real face data in tests.
struct FakeEngine;
impl Engine for FakeEngine {
    fn detect(&mut self, frame: &Frame, extract: bool) -> Result<Detected> {
        let count = usize::from(frame.pixels[0]);
        Ok(Detected {
            count,
            largest: (count > 0).then_some([0., 96., 96., 96.]),
            feature: (extract && count > 0).then(|| vector(4, 2.0)),
        })
    }
    fn jpeg(&self, frame: &Frame) -> Result<Vec<u8>> {
        Ok(vec![frame.pixels[0]])
    }
}
fn live(path: &Scratch) -> Arc<Live> {
    Arc::new(Live {
        engine: Mutex::new(Box::new(FakeEngine)),
        gallery: Some(Mutex::new(Gallery::open(&path.0).unwrap())),
        latest: Mutex::default(),
    })
}
fn frame(faces: u8) -> Frame {
    Frame {
        pixels: vec![faces; FRAME_BYTES].into(),
        captured: Instant::now(),
    }
}

#[test]
fn panel_enrolls_only_one_fresh_face_and_loses_access_on_shutdown() {
    let path = Scratch::new();
    let live = live(&path);
    let panel = Panel::default();
    *panel.0.lock().unwrap() = Some(live.clone());
    let attached = Attached(&panel);
    let mut expired = frame(1);
    expired.captured -= Duration::from_secs(3);
    for invalid in [frame(0), frame(2), expired] {
        *live.latest.lock().unwrap() = Some(invalid);
        assert!(panel.enroll("Ada").is_err());
    }
    assert!(panel.jpeg().unwrap().is_none());
    *live.latest.lock().unwrap() = Some(frame(1));
    panel.enroll("Ada").unwrap();
    assert_eq!(panel.people().unwrap(), ["ada"]);
    assert_eq!(panel.jpeg().unwrap(), Some(vec![1]));
    assert_eq!(
        live.observe(&frame(1), true).unwrap().person.as_deref(),
        Some("ada")
    );
    panel.forget("ada").unwrap();
    assert!(live.observe(&frame(1), true).unwrap().person.is_none());
    drop(attached);
    assert!(panel.jpeg().unwrap().is_none());
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
            let _ = seen.send(o.target);
        })
        .await
    });
    tx.send(frame(1)).await.unwrap();
    assert_eq!(updates.recv().await, Some(Some((-0.75, 0.0))));
    tx.send(frame(0)).await.unwrap();
    assert_eq!(updates.recv().await, Some(None));
    drop(tx);
    assert!(task.await.unwrap().is_err());
}

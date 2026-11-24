//! Optional openWakeWord inference. One bounded mailbox to the single-threaded ONNX
//! sessions; dropping the sender exits the worker without blocking device shutdown.
use super::vad::FRAME;
use anyhow::{Result, ensure};
use std::path::Path;

pub(super) struct Models {
    #[cfg(feature = "onnx-wake")]
    pub dir: std::path::PathBuf,
    #[cfg(feature = "onnx-wake")]
    threshold: f32,
    #[cfg(feature = "onnx-wake")]
    busy_threshold: f32,
}

impl Models {
    pub fn new(dir: &Path, threshold: f32, busy_threshold: f32) -> Result<Self> {
        ensure!(
            cfg!(feature = "onnx-wake"),
            "BUDDY_LISTEN_WAKE_DIR requires the onnx-wake build feature"
        );
        ensure!(
            (0.0..=1.0).contains(&busy_threshold)
                && (0.0..=busy_threshold).contains(&threshold)
                && threshold > 0.0,
            "invalid wake thresholds"
        );
        for name in [
            "melspectrogram.onnx",
            "embedding_model.onnx",
            "hey_pip.onnx",
        ] {
            ensure!(dir.join(name).is_file(), "missing wake model: {name}");
        }
        Ok(Self {
            #[cfg(feature = "onnx-wake")]
            dir: dir.to_owned(),
            #[cfg(feature = "onnx-wake")]
            threshold,
            #[cfg(feature = "onnx-wake")]
            busy_threshold,
        })
    }
}

pub(super) trait WakeDetector {
    fn reset(&mut self) {}
    async fn fired(&mut self, frame: &[i16; FRAME], busy: bool) -> Result<bool>;
}

pub(super) struct Transcript;
impl WakeDetector for Transcript {
    fn fired(
        &mut self,
        _: &[i16; FRAME],
        _: bool,
    ) -> impl std::future::Future<Output = Result<bool>> {
        std::future::ready(Ok(false))
    }
}

#[cfg(feature = "onnx-wake")]
mod onnx {
    use super::{FRAME, Models, Result, WakeDetector, ensure};
    use anyhow::Context;
    use ort::{session::Session, value::Tensor};
    use std::{collections::VecDeque, path::Path, time::Duration};
    use tokio::sync::{mpsc, oneshot};

    const HOP: usize = 1280;
    const OVERLAP: usize = 480; // 3 prior 10 ms hops for the mel analysis window
    const BINS: usize = 32;
    const MEL_ROWS: usize = 76;
    const WIDTH: usize = 96;

    struct Pipeline {
        mel: Session,
        encoder: Session,
        classifier: Session,
        overlap: Vec<f32>,
        bands: VecDeque<f32>,
        embeddings: VecDeque<f32>,
        steps: usize,
    }
    impl Pipeline {
        fn open(dir: &Path) -> Result<Self> {
            let load = |name| {
                Session::builder()?
                    .with_intra_threads(1)?
                    .with_inter_threads(1)?
                    .commit_from_file(dir.join(name))
            };
            let classifier = load("hey_pip.onnx")?;
            let shape = classifier
                .inputs
                .first()
                .and_then(|v| v.input_type.tensor_dimensions())
                .context("wake classifier needs tensor input")?;
            ensure!(
                shape.len() == 3 && (1..=64).contains(&shape[1]) && shape[2] == 96,
                "unsupported wake classifier shape"
            );
            let steps = usize::try_from(shape[1])?;
            Ok(Self {
                mel: load("melspectrogram.onnx")?,
                encoder: load("embedding_model.onnx")?,
                classifier,
                overlap: Vec::new(),
                bands: VecDeque::new(),
                embeddings: VecDeque::new(),
                steps,
            })
        }
        fn infer(net: &Session, shape: &[usize], data: Vec<f32>) -> Result<Vec<f32>> {
            let tensor = Tensor::from_array((shape, data.into_boxed_slice()))?;
            let outputs = net.run(ort::inputs![tensor]?)?;
            ensure!(outputs.len() == 1, "unexpected wake model outputs");
            Ok(outputs[0].try_extract_raw_tensor::<f32>()?.1.to_vec())
        }
        fn score(&mut self, raw: &[i16], reset: bool) -> Result<f32> {
            if reset || self.bands.is_empty() {
                self.overlap.clear();
                self.bands = VecDeque::from(vec![1.; MEL_ROWS * BINS]);
                self.embeddings.clear();
            }
            let mut wave = self.overlap.clone();
            wave.extend(raw.iter().copied().map(f32::from));
            self.overlap = wave[wave.len() - OVERLAP..].to_vec();
            let bands = Self::infer(&self.mel, &[1, wave.len()], wave)?;
            ensure!(matches!(bands.len(), 160 | 256), "unexpected mel output");
            self.bands.drain(..bands.len());
            self.bands.extend(bands.into_iter().map(|v| v / 10. + 2.));
            let embedding = Self::infer(
                &self.encoder,
                &[1, MEL_ROWS, BINS, 1],
                self.bands.iter().copied().collect(),
            )?;
            ensure!(embedding.len() == WIDTH, "unexpected embedding width");
            if self.embeddings.len() == self.steps * WIDTH {
                self.embeddings.drain(..WIDTH);
            }
            self.embeddings.extend(embedding);
            // Refuse artificial startup scores until every classifier row was heard live.
            if self.embeddings.len() < self.steps * WIDTH {
                return Ok(0.);
            }
            let scores = Self::infer(
                &self.classifier,
                &[1, self.steps, WIDTH],
                self.embeddings.iter().copied().collect(),
            )?;
            ensure!(
                scores.len() == 1 && (0.0..=1.0).contains(&scores[0]),
                "invalid wake score"
            );
            Ok(scores[0])
        }
    }

    struct Request {
        reset: bool,
        samples: Vec<i16>,
        result: oneshot::Sender<Result<f32>>,
    }
    pub(in super::super) struct Worker {
        tx: mpsc::Sender<Request>,
    }
    impl Worker {
        pub(in super::super) async fn start(dir: &Path) -> Result<Self> {
            let (tx, mut recv) = mpsc::channel::<Request>(1);
            let (ready, loaded) = oneshot::channel();
            let dir = dir.to_owned();
            std::thread::Builder::new()
                .name("local-wake".into())
                .spawn(move || {
                    let mut pipeline = match Pipeline::open(&dir) {
                        Ok(net) => net,
                        Err(e) => {
                            let _ = ready.send(Err(e));
                            return;
                        }
                    };
                    if ready.send(Ok(())).is_err() {
                        return;
                    }
                    while let Some(req) = recv.blocking_recv() {
                        let _ = req.result.send(pipeline.score(&req.samples, req.reset));
                    }
                })?;
            tokio::time::timeout(Duration::from_secs(30), loaded)
                .await
                .context("wake model load deadline")?
                .context("wake runtime unavailable")??;
            Ok(Self { tx })
        }
    }

    struct Gate {
        threshold: f32,
        busy_threshold: f32,
        samples: Vec<i16>,
        reset: bool,
        armed: bool,
    }
    impl Gate {
        fn new(threshold: f32, busy_threshold: f32) -> Self {
            Self {
                threshold,
                busy_threshold,
                samples: Vec::with_capacity(HOP),
                reset: true,
                armed: true,
            }
        }
        fn reset(&mut self) {
            self.armed = true;
            self.samples.clear();
            self.reset = true;
        }
        fn push(&mut self, frame: &[i16; FRAME]) -> Option<(bool, Vec<i16>)> {
            self.samples.extend(frame);
            if self.samples.len() < HOP {
                return None;
            }
            let samples = std::mem::replace(&mut self.samples, Vec::with_capacity(HOP));
            Some((std::mem::take(&mut self.reset), samples))
        }
        fn evaluate(&mut self, score: f32, busy: bool) -> bool {
            let threshold = if busy {
                self.busy_threshold
            } else {
                self.threshold
            };
            let fire = self.armed && score >= threshold;
            if fire {
                self.armed = false;
            } else if score < threshold * 0.6 {
                self.armed = true;
            }
            fire
        }
    }

    pub(in super::super) struct Stream<'a> {
        worker: &'a Worker,
        gate: Gate,
    }
    impl<'a> Stream<'a> {
        pub(in super::super) fn new(worker: &'a Worker, models: &'a Models) -> Self {
            Self {
                worker,
                gate: Gate::new(models.threshold, models.busy_threshold),
            }
        }
    }
    impl WakeDetector for Stream<'_> {
        fn reset(&mut self) {
            self.gate.reset();
        }
        async fn fired(&mut self, frame: &[i16; FRAME], busy: bool) -> Result<bool> {
            let Some((reset, samples)) = self.gate.push(frame) else {
                return Ok(false);
            };
            let (result, rx) = oneshot::channel();
            self.worker
                .tx
                .try_send(Request {
                    reset,
                    samples,
                    result,
                })
                .map_err(|_| anyhow::anyhow!("wake worker unavailable"))?;
            let score = tokio::time::timeout(Duration::from_secs(2), rx).await???;
            Ok(self.gate.evaluate(score, busy))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn gate_batches_hops_enforces_thresholds_and_hysteresis() {
            let mut gate = Gate::new(0.5, 0.8);
            for i in 0..3 {
                assert!(gate.push(&[i; FRAME]).is_none());
            }
            let (reset, hop) = gate.push(&[3; FRAME]).unwrap();
            assert!(reset);
            assert_eq!(hop.len(), HOP);
            assert_eq!(hop[0], 0);
            assert_eq!(hop[3 * FRAME], 3);

            for _ in 0..3 {
                assert!(gate.push(&[4; FRAME]).is_none());
            }
            let (reset, hop) = gate.push(&[5; FRAME]).unwrap();
            assert!(!reset);
            assert_eq!(hop.len(), HOP);

            // Busy requires the higher threshold; idle fires at the base threshold.
            assert!(!gate.evaluate(0.6, true));
            assert!(gate.evaluate(0.8, true));
            // Staying warm across busy -> idle suppresses tail double-fires until score drops below 0.6 * threshold.
            assert!(!gate.evaluate(0.7, false));
            assert!(!gate.evaluate(0.3, false));
            assert!(!gate.evaluate(0.29, false)); // re-arms
            assert!(gate.evaluate(0.5, false));
            assert!(!gate.evaluate(0.9, false));

            // Boundary reset clears partial samples, marks pipeline reset, and re-arms.
            assert!(gate.push(&[7; FRAME]).is_none());
            gate.reset();
            for _ in 0..3 {
                assert!(gate.push(&[8; FRAME]).is_none());
            }
            let (reset, hop) = gate.push(&[9; FRAME]).unwrap();
            assert!(reset);
            assert_eq!(hop[0], 8);
            assert!(gate.evaluate(0.5, false));
        }

        /// Operator-supplied JSONL, one reference score per little-endian 1280-sample hop.
        /// Run with the three models and a locally recorded raw single-beam clip; never commit either.
        #[test]
        #[ignore = "needs private wake models, reference scores, and a held-out recording"]
        fn compare_reference_scores() -> Result<()> {
            let mut engine = Pipeline::open(Path::new(&std::env::var("BUDDY_LISTEN_WAKE_DIR")?))?;
            let raw = std::fs::read(std::env::var("BUDDY_WAKE_TEST_PCM")?)?;
            let reference = std::fs::read_to_string(std::env::var("BUDDY_WAKE_TEST_SCORES")?)?;
            let scores: Vec<f32> = reference
                .lines()
                .map(str::parse)
                .collect::<std::result::Result<_, _>>()?;
            ensure!(
                raw.len() == scores.len() * HOP * 2 && scores.len() > 50,
                "unaligned or short reference clip"
            );
            let mut positives = 0;
            for (index, (bytes, expected)) in raw
                .as_chunks::<{ HOP * 2 }>()
                .0
                .iter()
                .zip(scores)
                .enumerate()
            {
                let samples: Vec<_> = bytes
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|v| i16::from_le_bytes(*v))
                    .collect();
                let actual = engine.score(&samples, index == 0)?;
                // Reference random startup embeddings have drained by 4 seconds.
                if index >= 50 {
                    ensure!(
                        actual.is_finite()
                            && (actual - expected).abs() < 0.02
                            && (actual >= 0.5) == (expected >= 0.5),
                        "wake score mismatch at hop {index}"
                    );
                    positives += usize::from(expected >= 0.5);
                }
            }
            ensure!(
                positives > 0 && positives < raw.len() / (HOP * 2) - 50,
                "reference needs both classes after warmup"
            );
            Ok(())
        }
    }
}

#[cfg(feature = "onnx-wake")]
pub(super) use onnx::{Stream, Worker};

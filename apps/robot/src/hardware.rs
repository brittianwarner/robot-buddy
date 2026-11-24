//! Process-owned hardware lifecycle. Only semantic cues and transcripts cross the
//! session boundary; camera frames and the recognition gallery stay on Pip.
use crate::{
    config::Settings,
    face::{Face, FaceGesture},
    input, listen,
    motion::{Motion, MotionInput},
    piper::OfflineVoice,
    screen::operator::{Control, Hardware},
    vision,
};
use anyhow::{Context, Result, bail};
use robot_buddy::protocol::{ExpressionCue, Gesture};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::{
    sync::{mpsc, watch},
    task::JoinSet,
};

#[derive(Clone, Default)]
pub struct Peripherals {
    pub motion: Option<MotionInput>,
    pub listen: Option<watch::Sender<listen::State>>,
    pub piper: Option<OfflineVoice>,
}

pub struct Devices {
    pub panel: Arc<Panel>,
    pub controller: Peripherals,
    // Aborting the camera and microphone tasks closes their child processes;
    // dropping Motion stops the servo worker and disables torque.
    _tasks: JoinSet<()>,
    _neck: Option<Motion>,
}

impl Devices {
    pub fn start(
        settings: &Settings,
        face: &Face,
        microphone: mpsc::Sender<input::Control>,
    ) -> Result<Self> {
        let mut tasks = JoinSet::new();
        let mut controller = Peripherals::default();
        let camera = vision::Panel::default();
        let (recognized, last_match) = watch::channel(None);
        let neck = if settings.simulate {
            None
        } else {
            let env_path = |key| {
                std::env::var_os(key)
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from)
            };
            match (
                env_path("BUDDY_SERVO_PORT"),
                env_path("BUDDY_SERVO_CALIBRATION"),
            ) {
                (Some(port), Some(calibration)) => {
                    if let Some(face_port) = &settings.face_serial
                        && std::fs::canonicalize(face_port)
                            .is_ok_and(|f| std::fs::canonicalize(&port).ok().as_ref() == Some(&f))
                    {
                        bail!("face and neck require different serial ports");
                    }
                    match Motion::start(&port, &calibration) {
                        Ok(neck) => Some(neck),
                        Err(error) => {
                            tracing::error!(%error,"neck disarmed; correct hardware or calibration and restart to enable motion");
                            None
                        }
                    }
                }
                (None, None) => None,
                _ => bail!("set both BUDDY_SERVO_PORT and BUDDY_SERVO_CALIBRATION"),
            }
        };
        controller.motion = neck.as_ref().map(Motion::input);
        if !settings.simulate {
            if settings.speech {
                controller.piper =
                    OfflineVoice::from_lookup(&|k| std::env::var(k).ok(), settings.player.clone())?;
            }
            if let Some(config) = listen::Config::from_env()? {
                let (tx, rx) = watch::channel(listen::State::default());
                controller.listen = Some(tx);
                tasks.spawn(async move {
                    listen::run(config, microphone, rx).await;
                });
            }
            if let Some(config) = vision::Config::from_env()? {
                let (face, tracking, camera) =
                    (face.clone(), controller.motion.clone(), camera.clone());
                tasks.spawn(async move {
                    let result =
                        vision::run(config, camera, move |observation: vision::Observation| {
                            if let Some(name) = observation.person {
                                recognized.send_replace(Some((std::time::Instant::now(), name)));
                            } else if observation.target.is_none() {
                                recognized.send_replace(None);
                            }
                            // An outward-facing camera and the eye firmware use
                            // opposite horizontal frames (robot vs. viewer).
                            if let Some((x, y)) = observation.target {
                                face.gaze(-x, y);
                                if let Some(motion) = &tracking {
                                    motion.track(x, y);
                                }
                            } else {
                                face.clear_gaze();
                            }
                        })
                        .await;
                    if let Err(error) = result {
                        tracing::warn!(%error,"camera stopped");
                    }
                });
            }
        }
        let panel = Arc::new(Panel {
            face: face.clone(),
            motion: controller.motion.clone(),
            camera,
            last_match,
        });
        Ok(Self {
            panel,
            controller,
            _neck: neck,
            _tasks: tasks,
        })
    }
}

pub fn apply(face: &Face, motion: Option<&MotionInput>, cue: ExpressionCue) {
    face.express(cue.emotion, cue.intensity);
    if let Some(neck) = motion {
        neck.express(cue.emotion.as_str(), cue.intensity);
    }
    if let Some(g) = cue.gesture {
        gesture(face, motion, g, cue.intensity);
    }
}

fn gesture(face: &Face, motion: Option<&MotionInput>, g: Gesture, i: f32) {
    match g {
        Gesture::Blink => face.gesture(FaceGesture::Blink),
        Gesture::EarWiggle => face.gesture(FaceGesture::EarWiggle),
        other => {
            if let Some(neck) = motion {
                neck.gesture(other.as_str(), i);
            }
        }
    }
}

pub struct Panel {
    face: Face,
    motion: Option<MotionInput>,
    camera: vision::Panel,
    last_match: watch::Receiver<Option<(std::time::Instant, String)>>,
}
impl Hardware for Panel {
    fn control(&self, cmd: Control) -> Result<()> {
        let neck = || self.motion.as_ref().context("neck disabled");
        match cmd {
            Control::Pose { yaw, pitch, roll } => {
                neck()?.manual([yaw, pitch, roll], Duration::from_secs(5));
            }
            Control::Release => neck()?.release(),
            Control::Gesture(name) => {
                let g = serde_json::from_value(name.into())?;
                if !matches!(g, Gesture::Blink | Gesture::EarWiggle) {
                    neck()?;
                }
                gesture(&self.face, self.motion.as_ref(), g, 1.);
            }
            Control::Emotion(name) => apply(
                &self.face,
                self.motion.as_ref(),
                ExpressionCue {
                    emotion: serde_json::from_value(name.into())?,
                    intensity: 1.,
                    gesture: None,
                },
            ),
            Control::Enroll(name) => self.camera.enroll(&name)?,
            Control::Forget(name) => self.camera.forget(&name)?,
        }
        Ok(())
    }
    fn jpeg(&self) -> Result<Option<Vec<u8>>> {
        self.camera.jpeg()
    }
    fn people(&self) -> Result<Vec<String>> {
        self.camera.people()
    }
    fn recognized(&self) -> Result<Option<String>> {
        Ok(self
            .last_match
            .borrow()
            .as_ref()
            .filter(|(at, _)| at.elapsed() < Duration::from_secs(2))
            .map(|(_, name)| name.clone()))
    }
    fn wifi_eyes(&self, pair: Option<(&str, &str)>) {
        let (left, right) = pair.unwrap_or_default();
        // Leave a quiet zone within the round 240 px LCD and the physical bezel.
        const WIDTH: u16 = 132;
        if self.face.qr(left, right, WIDTH).is_err() {
            let _ = self.face.qr("", "", WIDTH);
            tracing::warn!("Wi-Fi codes exceed face board capacity");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use robot_buddy::protocol::Emotion;

    #[test]
    fn face_controls_work_without_a_neck_and_match_labels_expire() {
        let (face, _board) = Face::new();
        let (matches, last_match) = watch::channel(None);
        let panel = Panel {
            face,
            motion: None,
            camera: vision::Panel::default(),
            last_match,
        };
        panel.control(Control::Emotion("surprised".into())).unwrap();
        assert_eq!(panel.face.override_emotion(), Some(Emotion::Surprised));
        assert!(panel.control(Control::Gesture("nod".into())).is_err());
        assert!(panel.control(Control::Gesture("ear_wiggle".into())).is_ok());
        assert!(panel.jpeg().unwrap().is_none());
        let now = std::time::Instant::now();
        matches.send_replace(Some((now, "Ada".into())));
        assert_eq!(panel.recognized().unwrap().as_deref(), Some("Ada"));
        matches.send_replace(Some((
            now.checked_sub(Duration::from_secs(3)).unwrap(),
            "Ada".into(),
        )));
        assert!(panel.recognized().unwrap().is_none());
    }
}

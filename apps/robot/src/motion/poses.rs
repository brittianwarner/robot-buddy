use super::calibration::Joints;
use std::{f32::consts::PI, time::Instant};

type Pose = [f32; 3];
type Key = (f32, Pose);
const ZERO: Pose = [0.; 3];

fn interpolate(keys: &[Key], t: f32) -> Pose {
    for pair in keys.windows(2) {
        let [(start, from), (end, to)] = [pair[0], pair[1]];
        if t < end {
            let fraction = (1. - (PI * ((t - start) / (end - start)).clamp(0., 1.)).cos()) * 0.5;
            return std::array::from_fn(|i| from[i] + (to[i] - from[i]) * fraction);
        }
    }
    ZERO
}

#[derive(Copy, Clone)]
pub(super) enum Gesture {
    Nod,
    Shake,
    Tilt,
    Look,
    Dance,
}
impl Gesture {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "nod" => Self::Nod,
            "shake" => Self::Shake,
            "tilt" => Self::Tilt,
            "look_around" => Self::Look,
            "dance" => Self::Dance,
            _ => return None,
        })
    }
    fn sample(self, t: f32) -> Pose {
        let keys: &[Key] = match self {
            Self::Nod => &[
                (0., ZERO),
                (0.25, [0., -12., 0.]),
                (0.5, [0., 4., 0.]),
                (0.75, [0., -8., 0.]),
                (1., ZERO),
            ],
            Self::Shake => &[
                (0., ZERO),
                (0.25, [14., 0., 0.]),
                (0.55, [-14., 0., 0.]),
                (0.8, [9., 0., 0.]),
                (1.05, ZERO),
            ],
            Self::Tilt => &[
                (0., ZERO),
                (0.35, [0., 0., 18.]),
                (1.1, [0., 0., 18.]),
                (1.5, ZERO),
            ],
            Self::Look => &[
                (0., ZERO),
                (0.6, [35., 5., 0.]),
                (1.2, [35., 5., 0.]),
                (2., [-35., 5., 0.]),
                (2.6, [-35., 5., 0.]),
                (3.2, ZERO),
            ],
            Self::Dance => return dance(t),
        };
        interpolate(keys, t)
    }
}
fn dance(t: f32) -> Pose {
    // Stretch, nod, groove, bops and a bow. Phase envelopes meet at neutral.
    let (start, end, pitch, roll, freq) = if t < 2. {
        (0., 2., 18., 15., 0.5)
    } else if t < 4. {
        (2., 4., 12., 0., 2.5)
    } else if t < 7. {
        (4., 7., 5., 20., 0.5)
    } else if t < 11. {
        (7., 11., 10., 18., 0.5)
    } else if t < 12.5 {
        (11., 12.5, 8., 15., 2.)
    } else {
        return interpolate(
            &[
                (12.5, ZERO),
                (13., [0., -18., 0.]),
                (13.4, [0., -18., 0.]),
                (14., ZERO),
            ],
            t,
        );
    };
    let phase = t - start;
    let strength = (PI * phase / (end - start)).sin().powi(2);
    [
        0.,
        strength * pitch * (phase * 4. * PI * freq).sin(),
        strength * roll * (phase * 2. * PI * freq).sin(),
    ]
}

pub(super) fn expression(s: &str, i: f32) -> (Pose, bool) {
    let (pitch, roll) = match s {
        "happy" => (6., -4.),
        "listening" | "curious" => (0., 13.),
        "thinking" => (0., 7.),
        "surprised" => (12., 0.),
        "sad" => (-12., -6.),
        "angry" => (-3., 0.),
        "sleepy" => (-15., -9.),
        "love" => (0., 6.),
        "asleep" => (-19.5, -6.),
        _ => (0., 0.),
    };
    ([0., pitch * i, roll * i], s == "asleep")
}

#[derive(Clone, Default)]
pub(super) struct Inputs {
    pub pose: Pose,
    pub sleeping: bool,
    pub gesture: Option<(Gesture, Instant, f32)>,
    pub manual: Option<(Pose, Instant)>,
    pub face: Option<(Instant, f32, f32)>,
}
pub(super) struct Planner {
    joints: Joints,
    pos: Pose,
    vel: Pose,
    track: Pose,
    seen: Option<Instant>,
    epoch: Instant,
}
impl Planner {
    pub fn new(joints: Joints, steps: [u16; 3], now: Instant) -> Self {
        Self {
            pos: std::array::from_fn(|i| joints[i].degrees(steps[i])),
            joints,
            vel: ZERO,
            track: ZERO,
            seen: None,
            epoch: now,
        }
    }
    pub fn step(&mut self, now: Instant, inputs: &Inputs) -> [u16; 3] {
        let manual = inputs.manual.filter(|(_, until)| *until > now);
        if inputs.sleeping {
            self.track = ZERO;
        }
        if let Some((t, x, y)) = inputs.face
            && self.seen != Some(t)
        {
            self.seen = Some(t);
            if !inputs.sleeping
                && manual.is_none()
                && now.saturating_duration_since(t).as_secs_f32() < 0.5
            {
                for (i, error) in [-60. * x, -45. * y].into_iter().enumerate() {
                    if error.abs() > 6. {
                        self.track[i] =
                            self.joints[i].clamp(self.pos[i] - inputs.pose[i] + 0.35 * error);
                    }
                }
            }
        }
        if self
            .seen
            .is_none_or(|t| now.saturating_duration_since(t).as_secs() > 10)
        {
            self.track = self.track.map(|v| v * 0.985);
        }
        let gesture = inputs.gesture.map_or(ZERO, |(g, t, i)| {
            g.sample(now.saturating_duration_since(t).as_secs_f32())
                .map(|v| v * i)
        });
        let phase = now.saturating_duration_since(self.epoch).as_secs_f32();
        let sway = if inputs.sleeping { 0.15 } else { 1. };
        let idle = [
            0.,
            (phase * 0.9).sin() * sway,
            (phase * 0.7).sin() * 1.2 * sway,
        ];
        std::array::from_fn(|i| {
            let j = self.joints[i];
            let goal = j.clamp(manual.map_or(
                inputs.pose[i] + self.track[i] + gesture[i] + idle[i],
                |(p, _)| p[i],
            ));
            let accel = 49. * (goal - self.pos[i]) - 14. * self.vel[i];
            self.vel[i] = (self.vel[i] + accel * 0.02).clamp(-j.max_speed_deg_s, j.max_speed_deg_s);
            self.pos[i] = j.clamp(self.pos[i] + self.vel[i] * 0.02);
            j.steps(self.pos[i])
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn gestures_ease_from_rest_and_return_to_neutral() {
        let start = Gesture::Nod.sample(0.01)[1].abs();
        let middle = (Gesture::Nod.sample(0.13)[1] - Gesture::Nod.sample(0.12)[1]).abs();
        assert!(middle > start * 5.);
        for g in [
            Gesture::Nod,
            Gesture::Shake,
            Gesture::Tilt,
            Gesture::Look,
            Gesture::Dance,
        ] {
            assert!(g.sample(0.).iter().all(|v| v.abs() < f32::EPSILON));
            assert!(g.sample(15.).iter().all(|v| v.abs() < f32::EPSILON));
            assert!(
                (0u16..700).any(|i| g.sample(f32::from(i) * 0.02).iter().any(|v| v.abs() > 5.))
            );
        }
    }
}

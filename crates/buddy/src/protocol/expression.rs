//! Live, best-effort physical cues. The durable event keeps its sequence but no cue.
use super::OutputEvent;
use serde::{Deserialize, Deserializer, Serialize};

/// The `OutputEvent.kind` reserved for one turn's face and motion cue.
pub const FACE_EXPRESS: &str = "face.express";

/// The face firmware's expression vocabulary, independent of the robot's lifecycle state.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Emotion {
    Neutral,
    Happy,
    Curious,
    Thinking,
    Surprised,
    Sad,
    Angry,
    Sleepy,
    Love,
    Asleep,
}

impl Emotion {
    /// Stable wire name shared with the device's face and pose tables.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Neutral => "neutral",
            Self::Happy => "happy",
            Self::Curious => "curious",
            Self::Thinking => "thinking",
            Self::Surprised => "surprised",
            Self::Sad => "sad",
            Self::Angry => "angry",
            Self::Sleepy => "sleepy",
            Self::Love => "love",
            Self::Asleep => "asleep",
        }
    }
}

/// One optional device-local animation; the model never supplies joint positions or timings.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Gesture {
    Blink,
    EarWiggle,
    Nod,
    Shake,
    Tilt,
    LookAround,
    Dance,
}

impl Gesture {
    /// Stable wire name accepted by the corresponding local animation controller.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Blink => "blink",
            Self::EarWiggle => "ear_wiggle",
            Self::Nod => "nod",
            Self::Shake => "shake",
            Self::Tilt => "tilt",
            Self::LookAround => "look_around",
            Self::Dance => "dance",
        }
    }
}

/// Typed payload of a live [`FACE_EXPRESS`]. Replay contains `{}` and never decodes as one.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq)]
pub struct ExpressionCue {
    pub emotion: Emotion,
    #[serde(deserialize_with = "intensity")]
    pub intensity: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gesture: Option<Gesture>,
}

fn intensity<'de, D: Deserializer<'de>>(deserializer: D) -> Result<f32, D::Error> {
    let value = f64::deserialize(deserializer)?;
    if (0.0..=1.0).contains(&value) {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "range checked before narrowing"
        )]
        Ok(value as f32)
    } else {
        Err(serde::de::Error::custom(
            "intensity must be between 0 and 1",
        ))
    }
}

impl OutputEvent {
    /// Call only for an in-order live push on the locally active turn. Unknown actions,
    /// invalid intensities and non-actionable replay markers all produce no movement.
    #[must_use]
    pub fn expression(&self) -> Option<ExpressionCue> {
        (self.kind == FACE_EXPRESS)
            .then(|| ExpressionCue::deserialize(&self.payload).ok())
            .flatten()
            .filter(|cue| cue.emotion != Emotion::Asleep)
    }
}

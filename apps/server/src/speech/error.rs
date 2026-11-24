//! Why a speech stream could not start, so the caller learns a status instead of a
//! blanket 400. Attached to `anyhow` errors as their source and found again with
//! [`SpeechError::of`].
use crate::web::Kind;
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpeechError {
    /// The actor refused the call: the turn is unknown, stopped or not speakable, or the
    /// session lost access. The route tells the two apart with an access check.
    Refused,
    /// The turn has nothing to speak.
    NoSpeech,
    /// A newer request for the same bot took the stream.
    Replaced,
    /// The engine could not be reached.
    EngineUnavailable,
    /// The engine answered with something other than an actor's refusal.
    Engine,
    /// An actor call or the stream deadline ran out.
    Timeout,
}

impl fmt::Display for SpeechError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Refused => "speech refused by the actor",
            Self::NoSpeech => "no speech for this turn",
            Self::Replaced => "speech stream replaced",
            Self::EngineUnavailable => "speech engine unreachable",
            Self::Engine => "speech engine failed",
            Self::Timeout => "speech deadline exceeded",
        })
    }
}

impl std::error::Error for SpeechError {}

impl SpeechError {
    /// The kind attached anywhere in `error`'s context chain.
    pub fn of(error: &anyhow::Error) -> Option<Self> {
        error.downcast_ref::<Self>().copied()
    }

    /// Classifies a Rivet client error. `rivetkit-client` 3.1 reports an actor's error
    /// as `action failed ({group}/{code}): ...`, and a gateway reply without an error
    /// body as `action failed: {status}`. An actor's own `bail!` reaches the client
    /// masked as `internal_error`. Anything else never reached an actor.
    pub fn from_client(error: &anyhow::Error) -> Self {
        let Some(message) = error
            .chain()
            .map(ToString::to_string)
            .find(|m| m.starts_with("action failed"))
        else {
            return Self::EngineUnavailable;
        };
        let code = message
            .strip_prefix("action failed (")
            .and_then(|rest| rest.split_once(')'))
            .map(|(code, _)| code);
        match code {
            Some(code) if code.ends_with("/internal_error") || code.starts_with("user/") => {
                Self::Refused
            }
            _ => Self::Engine,
        }
    }

    pub fn kind(self) -> Kind {
        match self {
            Self::Refused | Self::Replaced => Kind::Conflict,
            Self::NoSpeech => Kind::NotFound,
            Self::EngineUnavailable => Kind::Unavailable,
            Self::Engine => Kind::BadGateway,
            Self::Timeout => Kind::GatewayTimeout,
        }
    }
}

/// Tags a Rivet client error with its [`SpeechError`] kind.
pub fn engine(error: anyhow::Error) -> anyhow::Error {
    let kind = SpeechError::from_client(&error);
    error.context(kind)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    /// Pins the `rivetkit-client` 3.1.1 message shapes this classification reads.
    #[test]
    fn client_errors_are_classified_by_their_rivetkit_shape() {
        let cases = [
            (
                "action failed (core/internal_error): An internal error occurred, metadata=None",
                SpeechError::Refused,
            ),
            (
                "action failed (rivetkit/internal_error): x, metadata=None",
                SpeechError::Refused,
            ),
            (
                "action failed (user/busy): busy, metadata=None",
                SpeechError::Refused,
            ),
            (
                "action failed (actor/not_found): x, metadata=None",
                SpeechError::Engine,
            ),
            ("action failed: 502 Bad Gateway", SpeechError::Engine),
            (
                "error sending request: connection refused",
                SpeechError::EngineUnavailable,
            ),
        ];
        for (message, expected) in cases {
            assert_eq!(
                SpeechError::from_client(&anyhow!(message)),
                expected,
                "{message}"
            );
        }
        // Context added above the client error does not hide it.
        let wrapped = anyhow!("action failed (core/internal_error): x").context("chunk read");
        assert_eq!(SpeechError::from_client(&wrapped), SpeechError::Refused);
        let tagged = engine(anyhow!("connection refused")).context("outer");
        assert_eq!(
            SpeechError::of(&tagged),
            Some(SpeechError::EngineUnavailable)
        );
        assert_eq!(SpeechError::of(&anyhow!("plain")), None);
    }

    #[test]
    fn every_kind_has_a_distinct_failure_status() {
        use axum::http::StatusCode;
        for (error, status) in [
            (SpeechError::Refused, StatusCode::CONFLICT),
            (SpeechError::Replaced, StatusCode::CONFLICT),
            (SpeechError::NoSpeech, StatusCode::NOT_FOUND),
            (
                SpeechError::EngineUnavailable,
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (SpeechError::Engine, StatusCode::BAD_GATEWAY),
            (SpeechError::Timeout, StatusCode::GATEWAY_TIMEOUT),
        ] {
            assert_eq!(error.kind().status(), status, "{error}");
        }
    }
}

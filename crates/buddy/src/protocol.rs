//! Versioned application messages transported by the official Rivet client.
//!
//! Compatibility rule: every type here, and every settings type it carries,
//! ignores fields it does not know, because robots, the lab and older or newer
//! servers decode what a newer peer wrote (including rows read back from the
//! actor database). Settings *writes* are the exception: [`strict`] rejects unknown
//! fields on the values a user, the lab or a model tool submits, so a typo is
//! an error instead of a silently dropped setting.
use serde::{Deserialize, Deserializer, Serialize, de::DeserializeOwned};
use serde_json::Value;

/// Deserialize `T` and fail on any field `T` would have ignored, naming its
/// path. Use with `#[serde(deserialize_with = "strict")]` on write inputs.
pub fn strict<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    let mut unknown = None;
    let value = serde_ignored::deserialize(deserializer, |path| {
        unknown.get_or_insert_with(|| path.to_string());
    })?;
    match unknown {
        Some(path) => Err(serde::de::Error::custom(format!("unknown field `{path}`"))),
        None => Ok(value),
    }
}

/// [`strict`] for a JSON value, as model tool arguments arrive.
pub fn strict_value<T: DeserializeOwned>(value: &Value) -> serde_json::Result<T> {
    strict(value)
}

/// The one application action: an authenticated [`Call`]. Its output is JSON.
pub const CALL_ACTION: &str = "call";
/// The event that carries an [`OutputEvent`] to subscribed connections.
pub const OUTPUT_EVENT: &str = "output";
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Connection {
    pub session: String,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Call {
    pub session: String,
    pub command: Command,
}
#[cfg(feature = "server")]
impl rivetkit::Action for Call {
    type Output = Value;
    const NAME: &'static str = CALL_ACTION;
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    Enroll {
        device_id: String,
        public_key: String,
    },
    Snapshot,
    GetContextConfig,
    SetContextConfig {
        expected_revision: i64,
        #[serde(deserialize_with = "strict")]
        config: crate::context::ContextConfig,
        reason: String,
    },
    ListPersonas,
    GetPersona {
        name: String,
        version: i64,
    },
    SavePersona {
        #[serde(deserialize_with = "strict")]
        persona: crate::persona::PersonaDraft,
    },
    ActivatePersona {
        name: String,
        version: i64,
        expected_prompt_revision: i64,
        expected_voice_revision: i64,
    },
    GetVoiceConfig,
    SetVoiceConfig {
        expected_revision: i64,
        #[serde(deserialize_with = "strict")]
        config: crate::voice::VoiceConfig,
        reason: String,
    },
    GetSystemPrompt,
    SystemPromptHistory,
    SetSystemPrompt {
        expected_revision: i64,
        prompt: String,
        reason: String,
    },
    SpeechChunk {
        turn_id: String,
        index: u32,
    },
    SpokenAnswer {
        turn_id: String,
    },
    CheckAccess,
    ConsumeChallenge {
        nonce: String,
        expires: i64,
    },
    History {
        before: String,
    },
    SearchHistory {
        query: String,
    },
    SearchMemory {
        query: String,
    },
    Submit {
        request_id: String,
        text: String,
    },
    Cancel {
        turn_id: String,
    },
    /// Cancels the turn a `Submit` with this request ID made, or, before that submit
    /// arrives, makes it resolve cancelled without running. For a submit whose reply was
    /// lost. Replies like `Cancel`.
    CancelRequest {
        request_id: String,
    },
    Replay {
        after: i64,
    },
    Remember {
        key: String,
        value: String,
    },
    Forget {
        key: String,
    },
    BeginClaim,
    ProposeClaim {
        token: String,
        owner: String,
    },
    ConfirmClaim {
        code: String,
    },
    Revoke,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutputEvent {
    pub sequence: i64,
    pub turn_id: String,
    pub kind: String,
    pub payload: Value,
}
#[cfg(feature = "server")]
impl rivetkit::Event for OutputEvent {
    const NAME: &'static str = OUTPUT_EVENT;
}
/// The action error group and code of a submit refused because another turn is
/// still open. The error is public, so it reaches the client with its message and
/// a [`Busy`] as metadata. Servers before it refused with an internal error.
pub const BUSY_ERROR: (&str, &str) = ("user", "busy");
/// Metadata of a [`BUSY_ERROR`]: the open turn, so a device cancels exactly it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Busy {
    pub turn_id: String,
}
/// The open turn named by a [`BUSY_ERROR`] as `rivetkit-client` renders a refused call
/// over an actor connection: `RPC Error(user/busy): <message>, <metadata JSON>`. The client
/// exposes no structured error, so this is the one place that reads its text; the real
/// engine round trip is pinned by `tests/actors.rs`. The metadata is the trailing JSON
/// object, however the message reads (a brace in the message does not hide it). Anything
/// else, including a busy refusal without readable metadata, is not a busy refusal.
#[must_use]
pub fn busy_turn(error: &str) -> Option<String> {
    let (group, code) = BUSY_ERROR;
    let rest = error.strip_prefix(&format!("RPC Error({group}/{code}): "))?;
    rest.match_indices('{')
        .find_map(|(at, _)| serde_json::from_str::<Busy>(&rest[at..]).ok())
        .map(|busy| busy.turn_id)
}

// Action replies. The action output is JSON (`Call::Output`); these types are what the
// conversation actor writes and what clients may decode it into. Each ignores fields it
// does not know, like every other type here.

/// Reply of `Submit`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmitReply {
    pub turn_id: String,
    /// The request ID was already submitted: this is that turn, not a new one.
    pub duplicate: bool,
    /// A `CancelRequest` arrived first: the turn is cancelled and never runs. Omitted
    /// when false, so the reply older clients read is unchanged.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cancelled: bool,
}

/// Reply of `Cancel` and `CancelRequest`: `{"ok": true}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OkReply {
    pub ok: bool,
}

/// Reply of `SpeechChunk`, tagged by `state`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SpeechChunkReply {
    /// The chunk and the voice pinned for its turn.
    Ready {
        index: u32,
        text: String,
        voice: Value,
    },
    /// Not committed yet: the turn is queued or running.
    Pending,
    /// The turn completed and has no chunk at this index.
    Done,
}

/// Reply of `SpokenAnswer`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpokenAnswerReply {
    pub turn_id: String,
    pub text: String,
    pub voice: Value,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct EnrollmentRequest {
    pub certificate: crate::auth::Certificate,
    pub challenge: String,
    pub proof: String,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Bundle {
    pub bot_id: String,
    pub actor_id: String,
    pub conversation_actor_id: String,
    pub memory_actor_id: String,
    pub endpoint: String,
    pub namespace: String,
    pub gateway_token: String,
    pub session: String,
    pub expires: i64,
}

/// Text is never accepted here: the server loads the completed canonical answer.
#[derive(Clone, Serialize, Deserialize)]
pub struct SpeechRequest {
    pub turn_id: String,
    #[serde(default)]
    pub stream: bool,
}

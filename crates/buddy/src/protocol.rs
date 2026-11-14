//! Versioned application messages transported by the official Rivet client.
use rivetkit::{Action, Event};
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Connection {
    pub session: String,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Call {
    pub session: String,
    pub command: Command,
}
impl Action for Call {
    type Output = Value;
    const NAME: &'static str = "call";
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
        config: crate::context::ContextConfig,
        reason: String,
    },
    ListPersonas,
    GetPersona {
        name: String,
        version: i64,
    },
    SavePersona {
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
impl Event for OutputEvent {
    const NAME: &'static str = "output";
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

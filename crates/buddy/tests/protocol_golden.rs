//! Golden wire fixtures for every protocol message.
//!
//! Each fixture in `fixtures/protocol/` is the exact JSON a peer sends or receives. A test
//! builds the Rust value, serializes it and compares with the fixture, then parses the
//! fixture and serializes it again, so both directions are pinned. A changed wire format
//! fails here before it reaches a robot in the field.
//!
//! Regenerate after a deliberate wire change with `BUDDY_UPDATE_GOLDEN=1 cargo test -p
//! robot-buddy --test protocol_golden`, then review the fixture diff.
use robot_buddy::{
    auth::{Certificate, Challenge, Session},
    context::ContextConfig,
    persona::PersonaDraft,
    protocol::{Bundle, Call, Command, Connection, EnrollmentRequest, OutputEvent, SpeechRequest},
    voice::{Latency, SpeechRule, Synthesis, Voice, VoiceConfig, WordStyle},
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::path::PathBuf;

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/protocol")
        .join(format!("{name}.json"))
}

fn updating() -> bool {
    std::env::var("BUDDY_UPDATE_GOLDEN").is_ok_and(|v| v == "1")
}

/// Serializes `value`, compares with the fixture, and round-trips the fixture through `T`.
fn golden<T: Serialize + DeserializeOwned>(name: &str, value: &T) {
    let path = fixture_path(name);
    let actual = serde_json::to_value(value).expect("serialize");
    if updating() {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("fixture dir");
        let text = serde_json::to_string_pretty(&actual).expect("pretty");
        std::fs::write(&path, format!("{text}\n")).expect("write fixture");
    }
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e}; run with BUDDY_UPDATE_GOLDEN=1", path.display()));
    let expected: Value = serde_json::from_str(&raw).expect("fixture is JSON");
    assert_eq!(
        actual, expected,
        "{name}: serialized value differs from the fixture"
    );
    let parsed: T = serde_json::from_value(expected.clone())
        .unwrap_or_else(|e| panic!("{name}: fixture does not parse: {e}"));
    let again = serde_json::to_value(&parsed).expect("serialize parsed");
    assert_eq!(again, expected, "{name}: fixture does not round-trip");
}

/// The wire `type` of a command. Exhaustive on purpose: a new variant fails to compile
/// here until it has a fixture in [`all_commands`].
fn command_type(command: &Command) -> &'static str {
    match command {
        Command::Enroll { .. } => "enroll",
        Command::Snapshot => "snapshot",
        Command::GetContextConfig => "get_context_config",
        Command::SetContextConfig { .. } => "set_context_config",
        Command::ListPersonas => "list_personas",
        Command::GetPersona { .. } => "get_persona",
        Command::SavePersona { .. } => "save_persona",
        Command::ActivatePersona { .. } => "activate_persona",
        Command::GetVoiceConfig => "get_voice_config",
        Command::SetVoiceConfig { .. } => "set_voice_config",
        Command::GetSystemPrompt => "get_system_prompt",
        Command::SystemPromptHistory => "system_prompt_history",
        Command::SetSystemPrompt { .. } => "set_system_prompt",
        Command::SpeechChunk { .. } => "speech_chunk",
        Command::SpokenAnswer { .. } => "spoken_answer",
        Command::CheckAccess => "check_access",
        Command::ConsumeChallenge { .. } => "consume_challenge",
        Command::History { .. } => "history",
        Command::SearchHistory { .. } => "search_history",
        Command::SearchMemory { .. } => "search_memory",
        Command::Submit { .. } => "submit",
        Command::Cancel { .. } => "cancel",
        Command::Replay { .. } => "replay",
        Command::Remember { .. } => "remember",
        Command::Forget { .. } => "forget",
        Command::BeginClaim => "begin_claim",
        Command::ProposeClaim { .. } => "propose_claim",
        Command::ConfirmClaim { .. } => "confirm_claim",
        Command::Revoke => "revoke",
    }
}

fn voice_config() -> VoiceConfig {
    VoiceConfig {
        voice: Voice::Will,
        delivery: "Warm, calm and close.".into(),
        synthesis: Synthesis {
            language: Some("en".into()),
            latency: Latency::Balanced,
            ..Synthesis::default()
        },
        speech_rules: vec![
            SpeechRule {
                text: "secret".into(),
                style: WordStyle::Whisper,
                phonemes: None,
            },
            SpeechRule {
                text: "Pip".into(),
                style: WordStyle::Emphasis,
                phonemes: Some("P IH1 P".into()),
            },
        ],
    }
}

fn context_config() -> ContextConfig {
    ContextConfig {
        timezone: "America/New_York".into(),
        location: Some("New York".into()),
    }
}

const TURN: &str = "0195f0a2-7c1e-7d2b-9a40-3f1c2b8e9d10";

fn all_commands() -> Vec<Command> {
    vec![
        Command::Enroll {
            device_id: "device-1".into(),
            public_key: "MCowBQYDK2VwAyEAexamplepublickey".into(),
        },
        Command::Snapshot,
        Command::GetContextConfig,
        Command::SetContextConfig {
            expected_revision: 1,
            config: context_config(),
            reason: "User moved".into(),
        },
        Command::ListPersonas,
        Command::GetPersona {
            name: "Captain".into(),
            version: 0,
        },
        Command::SavePersona {
            persona: PersonaDraft {
                name: "Captain".into(),
                expected_version: 0,
                prompt: "You are Captain Buddy.".into(),
                voice: voice_config(),
                reason: "New character".into(),
            },
        },
        Command::ActivatePersona {
            name: "Captain".into(),
            version: 2,
            expected_prompt_revision: 3,
            expected_voice_revision: 4,
        },
        Command::GetVoiceConfig,
        Command::SetVoiceConfig {
            expected_revision: 1,
            config: voice_config(),
            reason: "Softer voice".into(),
        },
        Command::GetSystemPrompt,
        Command::SystemPromptHistory,
        Command::SetSystemPrompt {
            expected_revision: 1,
            prompt: "You are a helpful desk robot.".into(),
            reason: "Shorter answers".into(),
        },
        Command::SpeechChunk {
            turn_id: TURN.into(),
            index: 2,
        },
        Command::SpokenAnswer {
            turn_id: TURN.into(),
        },
        Command::CheckAccess,
        Command::ConsumeChallenge {
            nonce: "nonce-1".into(),
            expires: 1_790_000_000,
        },
        Command::History {
            before: String::new(),
        },
        Command::SearchHistory {
            query: "weather".into(),
        },
        Command::SearchMemory {
            query: "favorite color".into(),
        },
        Command::Submit {
            request_id: "req-1".into(),
            text: "What's the weather?".into(),
        },
        Command::Cancel {
            turn_id: TURN.into(),
        },
        Command::Replay { after: 41 },
        Command::Remember {
            key: "favorite_color".into(),
            value: "blue".into(),
        },
        Command::Forget {
            key: "favorite_color".into(),
        },
        Command::BeginClaim,
        Command::ProposeClaim {
            token: "claim-token".into(),
            owner: "issuer|user-1".into(),
        },
        Command::ConfirmClaim {
            code: "123456".into(),
        },
        Command::Revoke,
    ]
}

#[test]
fn every_command_variant_has_a_golden_fixture() {
    let commands = all_commands();
    let mut seen = std::collections::BTreeSet::new();
    for command in &commands {
        let kind = command_type(command);
        assert!(seen.insert(kind), "{kind} listed twice");
        let wire = serde_json::to_value(command).expect("serialize");
        assert_eq!(wire["type"], kind, "wire tag for {kind}");
        golden(&format!("command.{kind}"), command);
    }
    assert_eq!(seen.len(), 29, "one fixture per Command variant");
}

#[test]
fn action_envelope_and_connection_params_are_pinned() {
    golden(
        "call",
        &Call {
            session: "header.payload.signature".into(),
            command: Command::Submit {
                request_id: "req-1".into(),
                text: "Hello".into(),
            },
        },
    );
    golden(
        "connection",
        &Connection {
            session: "header.payload.signature".into(),
        },
    );
}

#[test]
fn output_events_are_pinned() {
    for (name, kind, payload) in [
        ("turn_started", "turn.started", json!({"text":"Hello"})),
        ("text_delta", "text.delta", json!({"text":"Hi there. "})),
        (
            "speech_ready",
            "speech.ready",
            json!({"index":0,"text":"Hi there."}),
        ),
        (
            "tool_started",
            "tool.started",
            json!({"name":"memory_remember"}),
        ),
        (
            "turn_completed",
            "turn.completed",
            json!({"text":"Hi there.","usage":{"input_tokens":3,"output_tokens":2}}),
        ),
        (
            "turn_interrupted",
            "turn.interrupted",
            json!({"reason":"server_restart"}),
        ),
    ] {
        golden(
            &format!("output_event.{name}"),
            &OutputEvent {
                sequence: 7,
                turn_id: TURN.into(),
                kind: kind.into(),
                payload,
            },
        );
    }
}

#[test]
fn speech_request_is_pinned_and_stream_defaults_off() {
    golden(
        "speech_request",
        &SpeechRequest {
            turn_id: TURN.into(),
            stream: true,
        },
    );
    let legacy: SpeechRequest = serde_json::from_value(json!({"turn_id":TURN})).expect("parse");
    assert!(
        !legacy.stream,
        "a request without `stream` is a buffered request"
    );
}

#[test]
fn enrollment_and_bundle_are_pinned() {
    let certificate = Certificate {
        device_id: "device-1".into(),
        public_key: "ZXhhbXBsZS1wdWJsaWMta2V5LTMyLWJ5dGVzLWxvbmc=".into(),
        issuer_signature: "c2lnbmF0dXJl".into(),
    };
    golden("certificate", &certificate);
    golden(
        "enrollment_request",
        &EnrollmentRequest {
            certificate,
            challenge: "header.payload.signature".into(),
            proof: "cHJvb2Y=".into(),
        },
    );
    golden(
        "challenge",
        &Challenge {
            device_id: "device-1".into(),
            public_key: "ZXhhbXBsZS1wdWJsaWMta2V5LTMyLWJ5dGVzLWxvbmc=".into(),
            nonce: "nonce-1".into(),
            expires: 1_790_000_000,
        },
    );
    golden(
        "session",
        &Session {
            bot_id: "bot-1".into(),
            role: "device".into(),
            expires: 1_790_000_000,
        },
    );
    golden(
        "bundle",
        &Bundle {
            bot_id: "bot-1".into(),
            actor_id: "actor-bot".into(),
            conversation_actor_id: "actor-conversation".into(),
            memory_actor_id: "actor-memory".into(),
            endpoint: "https://engine.example.test".into(),
            namespace: "default".into(),
            gateway_token: "gateway-token".into(),
            session: "header.payload.signature".into(),
            expires: 1_790_000_000,
        },
    );
}

#[test]
fn settings_payloads_are_pinned() {
    golden("voice_config", &voice_config());
    golden("context_config", &context_config());
    golden("synthesis.default", &Synthesis::default());
}

/// Adds `"added_later": ...` to a fixture object, as a newer peer would.
fn with_additive_field(name: &str) -> Value {
    let raw = std::fs::read_to_string(fixture_path(name)).expect("fixture");
    let mut value: Value = serde_json::from_str(&raw).expect("json");
    value
        .as_object_mut()
        .expect("object fixture")
        .insert("added_later".into(), json!({"nested":[1, 2, 3]}));
    value
}

fn accepts<T: DeserializeOwned>(name: &str) -> bool {
    serde_json::from_value::<T>(with_additive_field(name)).is_ok()
}

#[test]
fn messages_a_client_reads_ignore_additive_fields() {
    // docs/architecture/robot-client.md: "Clients ignore additive unknown fields."
    assert!(accepts::<OutputEvent>("output_event.speech_ready"));
    assert!(accepts::<Bundle>("bundle"));
    assert!(accepts::<SpeechRequest>("speech_request"));
    assert!(accepts::<Call>("call"));
    assert!(accepts::<Connection>("connection"));
    assert!(accepts::<EnrollmentRequest>("enrollment_request"));
    assert!(accepts::<Challenge>("challenge"));
    assert!(accepts::<Session>("session"));
    for command in all_commands() {
        let name = format!("command.{}", command_type(&command));
        assert!(
            accepts::<Command>(&name),
            "{name} rejects an additive field"
        );
    }
}

#[test]
fn unknown_event_kinds_parse_and_unknown_commands_are_rejected() {
    let event: OutputEvent = serde_json::from_value(json!({
        "sequence": 9, "turn_id": TURN, "kind": "turn.something_new", "payload": {"x": 1}
    }))
    .expect("unknown event kinds must not break clients");
    assert_eq!(event.kind, "turn.something_new");
    assert!(serde_json::from_value::<Command>(json!({"type":"self_destruct"})).is_err());
    assert!(serde_json::from_value::<Command>(json!({"type":"replay"})).is_err());
}

fn command_fixture(name: &str) -> Value {
    serde_json::from_str(&std::fs::read_to_string(fixture_path(name)).expect("fixture"))
        .expect("json")
}

/// Compatibility rule (docs/architecture/robot-client.md): anything decoded from a peer or
/// from storage ignores additive fields. Settings types are read back from the actor database,
/// by the speech service from the pinned voice, and by clients from `get_*` results, so a
/// newer peer's field must never break them.
#[test]
fn settings_read_back_ignore_additive_fields() {
    assert!(accepts::<VoiceConfig>("voice_config"));
    assert!(accepts::<ContextConfig>("context_config"));
    assert!(accepts::<Synthesis>("synthesis.default"));
    let mut nested = with_additive_field("voice_config");
    nested["synthesis"]["added_later"] = json!(1);
    nested["speech_rules"][0]["added_later"] = json!("x");
    assert!(serde_json::from_value::<VoiceConfig>(nested).is_ok());
    let mut draft = command_fixture("command.save_persona")["persona"].clone();
    draft["added_later"] = json!(true);
    draft["voice"]["added_later"] = json!(true);
    assert!(serde_json::from_value::<PersonaDraft>(draft).is_ok());
    let rule = json!({"text":"secret","style":"whisper","added_later":true});
    assert!(serde_json::from_value::<SpeechRule>(rule).is_ok());
}

/// Settings writes are the exception: a field the server does not know, at any depth of the
/// submitted config, is rejected and named rather than silently dropped. The command
/// envelope itself still ignores additive fields (see above).
#[test]
fn settings_writes_reject_unknown_fields() {
    let rejects = |fixture: &str, field: &str, pointer: &str| {
        let mut command = command_fixture(fixture);
        command
            .pointer_mut(pointer)
            .and_then(Value::as_object_mut)
            .expect("object at pointer")
            .insert(field.into(), json!(true));
        let error = serde_json::from_value::<Command>(command)
            .err()
            .unwrap_or_else(|| panic!("{fixture}{pointer}/{field} was accepted"));
        assert!(
            error.to_string().contains(field),
            "{fixture}: error should name the field: {error}"
        );
    };
    rejects("command.set_voice_config", "added_later", "/config");
    rejects(
        "command.set_voice_config",
        "added_later",
        "/config/synthesis",
    );
    rejects(
        "command.set_voice_config",
        "added_later",
        "/config/speech_rules/0",
    );
    rejects("command.set_context_config", "added_later", "/config");
    rejects("command.save_persona", "added_later", "/persona");
    rejects("command.save_persona", "added_later", "/persona/voice");
    // Model tool arguments decode through the same check.
    let mut config = serde_json::to_value(voice_config()).expect("ser");
    assert!(robot_buddy::protocol::strict_value::<VoiceConfig>(&config).is_ok());
    config["speech_rules"][1]["typo"] = json!("x");
    let error = robot_buddy::protocol::strict_value::<VoiceConfig>(&config)
        .err()
        .expect("tool argument with an unknown field");
    assert!(error.to_string().contains("speech_rules.1.typo"), "{error}");
}

#[test]
fn stored_voice_names_from_older_actors_still_parse() {
    for (stored, current) in [
        ("adrian", Voice::Will),
        ("delia", Voice::Sarah),
        ("sheila", Voice::Jessica),
        (" Will ", Voice::Will),
    ] {
        let parsed: Voice = serde_json::from_value(json!(stored)).expect("legacy voice");
        assert_eq!(parsed, current, "{stored}");
    }
    assert_eq!(
        serde_json::to_value(Voice::Sarah).expect("ser"),
        json!("sarah")
    );
    assert!(serde_json::from_value::<Voice>(json!("nobody")).is_err());
}

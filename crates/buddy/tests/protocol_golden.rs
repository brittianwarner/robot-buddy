//! Golden wire fixtures for every protocol message.
//!
//! Each fixture in `fixtures/protocol/` is the exact JSON a peer sends or receives. A test
//! builds the Rust value, serializes it and compares with the fixture, then parses the
//! fixture and serializes it again, so both directions are pinned. A changed wire format
//! fails here before it reaches a robot in the field.
//!
//! Output events are the exception: their fixtures are captured from the real actor by the
//! integration suite (`BUDDY_UPDATE_GOLDEN=1 bun run test:integration`), and this file only
//! checks they parse, round-trip and cover every kind.
//!
//! Regenerate the rest after a deliberate wire change with `BUDDY_UPDATE_GOLDEN=1 cargo test
//! -p robot-buddy --test protocol_golden`, then review the fixture diff.
use robot_buddy::{
    auth::{Certificate, Challenge, Session},
    context::ContextConfig,
    persona::PersonaDraft,
    protocol::{
        Bundle, Busy, Call, Command, Connection, EnrollmentRequest, OkReply, OutputEvent,
        SpeechChunkReply, SpeechRequest, SpokenAnswerReply, SubmitReply, busy_turn,
    },
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
        Command::CancelRequest { .. } => "cancel_request",
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
        Command::CancelRequest {
            request_id: "req-1".into(),
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
    assert_eq!(seen.len(), 30, "one fixture per Command variant");
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

/// Every output event kind the actor emits. The fixtures are captured from real turns by
/// `actors.rs::output_events_match_their_golden_fixtures`, the interrupted-turn restart
/// test and the live-only expression test. They show what a robot actually receives.
const OUTPUT_EVENTS: [&str; 10] = [
    "face_express",
    "turn_started",
    "text_delta",
    "speech_ready",
    "tool_started",
    "tool_completed",
    "turn_completed",
    "turn_cancelled",
    "turn_failed",
    "turn_interrupted",
];

fn read_fixture(name: &str) -> Value {
    let path = fixture_path(name);
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "{}: {e}; capture it with BUDDY_UPDATE_GOLDEN=1 bun run test:integration",
            path.display()
        )
    });
    serde_json::from_str(&raw).expect("fixture is JSON")
}

#[test]
fn output_events_are_pinned() {
    let dir = fixture_path("x").parent().expect("dir").to_path_buf();
    let mut found: Vec<String> = std::fs::read_dir(&dir)
        .expect("fixture dir")
        .filter_map(|e| {
            let name = e.ok()?.file_name().into_string().ok()?;
            Some(
                name.strip_prefix("output_event.")?
                    .strip_suffix(".json")?
                    .to_owned(),
            )
        })
        .collect();
    found.sort();
    let mut expected: Vec<String> = OUTPUT_EVENTS.iter().map(|s| (*s).to_owned()).collect();
    expected.sort();
    assert_eq!(found, expected, "output event fixtures");
    for name in OUTPUT_EVENTS {
        let fixture = read_fixture(&format!("output_event.{name}"));
        let event: OutputEvent = serde_json::from_value(fixture.clone())
            .unwrap_or_else(|e| panic!("{name}: fixture does not parse: {e}"));
        assert_eq!(
            serde_json::to_value(&event).expect("serialize"),
            fixture,
            "{name}: fixture does not round-trip"
        );
        assert_eq!(event.kind, name.replacen('_', ".", 1), "{name}: kind");
        assert_eq!(
            (event.sequence, event.turn_id.as_str()),
            (7, TURN),
            "{name}: normalized"
        );
        assert!(event.payload.is_object(), "{name}: payload is an object");
    }
}

#[test]
fn speech_ready_carries_only_the_sentence_index() {
    // Speech is fetched by turn ID and index; the event never carries the text to speak.
    let event = read_fixture("output_event.speech_ready");
    let keys: Vec<&String> = event["payload"]
        .as_object()
        .expect("payload")
        .keys()
        .collect();
    assert_eq!(keys, ["index"], "{event}");
    assert!(event["payload"]["index"].is_u64(), "{event}");
}

#[test]
fn expression_reader_ignores_future_fields_but_never_moves_on_unknown_or_invalid_cues() {
    use robot_buddy::protocol::{Emotion, ExpressionCue, Gesture};

    let mut event: OutputEvent =
        serde_json::from_value(read_fixture("output_event.face_express")).unwrap();
    let expected = ExpressionCue {
        emotion: Emotion::Happy,
        intensity: 0.75,
        gesture: Some(Gesture::Nod),
    };
    assert_eq!(event.expression(), Some(expected));
    event.payload["added_later"] = json!(42);
    assert_eq!(event.expression(), Some(expected));
    // Model writes, unlike client reads, may not smuggle free-form/private fields.
    assert!(robot_buddy::protocol::strict_value::<ExpressionCue>(&event.payload).is_err());
    for invalid in [
        json!({}),
        json!({"emotion":"happy","intensity":1.000_000_001}),
        json!({"emotion":"happy","intensity":-1}),
        json!({"emotion":"other","intensity":0}),
        json!({"emotion":"happy","intensity":1,"gesture":"custom"}),
    ] {
        event.payload = invalid;
        assert_eq!(event.expression(), None);
    }
    event.payload = json!({"emotion":"happy","intensity":0});
    assert!(event.expression().is_some_and(|cue| cue.gesture.is_none()));
    event.kind = "text.delta".into();
    assert_eq!(event.expression(), None);
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
    for name in OUTPUT_EVENTS {
        let name = format!("output_event.{name}");
        assert!(
            accepts::<OutputEvent>(&name),
            "{name} rejects an additive field"
        );
    }
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

fn speech_voice() -> Value {
    json!({"config": voice_config(), "revision": 2, "updated": 1_790_000_000,
        "voice_id": "voice-id", "name": "Will"})
}

/// Action replies: what the conversation actor writes as the action output, in the shapes
/// `SubmitReply`, `OkReply`, `SpeechChunkReply` and `SpokenAnswerReply` decode.
#[test]
fn action_replies_are_pinned() {
    let submit = |duplicate, cancelled| SubmitReply {
        turn_id: TURN.into(),
        duplicate,
        cancelled,
    };
    golden("reply.submit", &submit(false, false));
    golden("reply.submit.duplicate", &submit(true, false));
    golden("reply.submit.cancelled", &submit(false, true));
    golden("reply.ok", &OkReply { ok: true });
    golden(
        "reply.speech_chunk.ready",
        &SpeechChunkReply::Ready {
            index: 0,
            text: "Hello there.".into(),
            voice: speech_voice(),
        },
    );
    golden("reply.speech_chunk.pending", &SpeechChunkReply::Pending);
    golden("reply.speech_chunk.done", &SpeechChunkReply::Done);
    golden(
        "reply.spoken_answer",
        &SpokenAnswerReply {
            turn_id: TURN.into(),
            text: "Hello there.".into(),
            voice: speech_voice(),
        },
    );
    golden(
        "busy",
        &Busy {
            turn_id: TURN.into(),
        },
    );
}

#[test]
fn replies_older_clients_read_are_unchanged() {
    // A submit that was not cancelled first has exactly the reply older servers sent.
    let reply = command_fixture("reply.submit");
    assert_eq!(reply, json!({"turn_id": TURN, "duplicate": false}));
    assert_eq!(command_fixture("reply.ok"), json!({"ok": true}));
    let chunk = command_fixture("reply.speech_chunk.ready");
    assert_eq!(chunk["state"], "ready");
    assert_eq!(
        command_fixture("reply.speech_chunk.pending"),
        json!({"state": "pending"})
    );
    assert_eq!(
        command_fixture("reply.speech_chunk.done"),
        json!({"state": "done"})
    );
    // An older reply without `cancelled` still decodes.
    let old: SubmitReply = serde_json::from_value(json!({"turn_id": TURN, "duplicate": true}))
        .expect("older submit reply");
    assert!(old.duplicate && !old.cancelled);
    for name in [
        "reply.submit",
        "reply.submit.cancelled",
        "reply.speech_chunk.ready",
        "reply.spoken_answer",
        "busy",
    ] {
        assert!(
            serde_json::from_value::<Value>(with_additive_field(name)).is_ok(),
            "{name}"
        );
    }
    assert!(accepts::<SubmitReply>("reply.submit"));
    assert!(accepts::<OkReply>("reply.ok"));
    assert!(accepts::<SpeechChunkReply>("reply.speech_chunk.ready"));
    assert!(accepts::<SpeechChunkReply>("reply.speech_chunk.pending"));
    assert!(accepts::<SpokenAnswerReply>("reply.spoken_answer"));
    assert!(accepts::<Busy>("busy"));
}

/// The busy refusal as `rivetkit-client` renders it over an actor connection. The real
/// engine round trip is `actors.rs::running_turn_pins_voice_rejects_busy_and_cancels_cleanly`.
#[test]
fn busy_refusals_name_the_open_turn() {
    let metadata = serde_json::to_string(&Busy {
        turn_id: TURN.into(),
    })
    .expect("ser");
    let refused = format!("RPC Error(user/busy): Another turn is still open., {metadata}");
    assert_eq!(busy_turn(&refused).as_deref(), Some(TURN));
    // Additive metadata fields from a newer server still name the turn.
    let newer = format!(
        "RPC Error(user/busy): Another turn is still open., {{\"turn_id\":\"{TURN}\",\"x\":1}}"
    );
    assert_eq!(busy_turn(&newer).as_deref(), Some(TURN));
    // The `{:#}` (pretty) rendering, and a brace in the message text, still name it.
    let pretty = format!(
        "RPC Error(user/busy): Another turn is still open., {:#}",
        json!({ "turn_id": TURN })
    );
    assert_eq!(busy_turn(&pretty).as_deref(), Some(TURN));
    let braced = format!("RPC Error(user/busy): Busy {{still open}}., {metadata}");
    assert_eq!(busy_turn(&braced).as_deref(), Some(TURN));
    for other in [
        "RPC Error(user/busy): Another turn is still open., null",
        "RPC Error(actor/internal_error): busy, {\"turn_id\":\"t\"}",
        "RPC Error(user/busy): Connection closed: gone",
        "timed out",
    ] {
        assert_eq!(busy_turn(other), None, "{other}");
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

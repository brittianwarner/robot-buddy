//! Actor integration tests against an isolated real Rivet engine.
//!
//! All tests share one engine, one registry and one mock model per test binary (see
//! `support`). Each test enrolls its own bots and keys the mock by its own user text, so
//! tests run in parallel; the restart test takes the harness exclusively.
//! Run with `bun run test:integration`.
mod support;

use anyhow::{Context, Result, ensure};
use robot_buddy::{
    auth::{self, Session},
    context::ContextConfig,
    persona::PersonaDraft,
    protocol::Command,
    voice::{Voice, VoiceConfig},
};
use serde_json::{Value, json};
use std::{
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};
use support::{
    DropGuard, Events, Harness, Seen, TestBot, WAIT, harness, init_tracing, kinds, sse,
    streamed_text, terminal,
};

/// Gap between mock tokens of about four characters.
const TOKEN_GAP: Duration = Duration::from_millis(20);

fn voice(voice: Voice) -> VoiceConfig {
    VoiceConfig {
        voice,
        delivery: "Calm and warm.".into(),
        synthesis: robot_buddy::voice::Synthesis::default(),
        speech_rules: Vec::new(),
    }
}

fn draft(name: &str, expected_version: i64, prompt: &str) -> PersonaDraft {
    PersonaDraft {
        name: name.into(),
        expected_version,
        prompt: prompt.into(),
        voice: voice(Voice::Sarah),
        reason: format!("Save {name}"),
    }
}

async fn start() -> (&'static Harness, tokio::sync::RwLockReadGuard<'static, ()>) {
    init_tracing();
    let h = harness().await;
    (h, h.shared().await)
}

async fn start_alone() -> (&'static Harness, tokio::sync::RwLockWriteGuard<'static, ()>) {
    init_tracing();
    let h = harness().await;
    (h, h.exclusive().await)
}

fn status(record: &Value) -> &str {
    record["status"].as_str().unwrap_or_default()
}

fn end_kind(events: &[Seen]) -> Option<&str> {
    terminal(events).map(|s| s.event.kind.as_str())
}

// ---------------------------------------------------------------------------------------
// Registration, enrollment and access
// ---------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the isolated engine; run bun run test:integration"]
async fn enrollment_is_idempotent_and_session_bound() -> Result<()> {
    let (h, _shared) = start().await;
    let b = TestBot::enroll(h).await?;
    let enroll = |public_key: &str| Command::Enroll {
        device_id: format!("device-{}", b.id),
        public_key: public_key.into(),
    };
    for _ in 0..10 {
        let again = b.bot_as(&b.admin, enroll("test-key")).await?;
        assert_eq!(again["bot_id"], b.id.as_str());
    }
    assert!(
        b.bot_as(&b.admin, enroll("another-key")).await.is_err(),
        "identity must not be replaced"
    );
    assert!(
        b.bot_as(&b.device, enroll("test-key")).await.is_err(),
        "device must not enroll"
    );
    let wrong_bot = h.token("wrong-bot", "device");
    assert!(b.bot_as(&wrong_bot, Command::Snapshot).await.is_err());
    assert!(b.conv_as(&wrong_bot, Command::Snapshot).await.is_err());
    let session = |role: &str, expires| Session {
        bot_id: b.id.clone(),
        role: role.into(),
        expires,
    };
    let forged = auth::sign(
        b"forged-signing-key-that-is-long-enough-0",
        "session",
        &session("admin", auth::now() + 600),
    )?;
    assert!(b.bot_as(&forged, Command::Snapshot).await.is_err());
    let expired = auth::sign(
        std::env::var("BUDDY_SIGNING_KEY")?.as_bytes(),
        "session",
        &session("device", auth::now() - 1),
    )?;
    assert!(b.conv_as(&expired, Command::Snapshot).await.is_err());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the isolated engine; run bun run test:integration"]
async fn challenges_are_single_use_and_claims_confirm_once() -> Result<()> {
    let (h, _shared) = start().await;
    let b = TestBot::enroll(h).await?;
    let consume = |nonce: &str, expires| Command::ConsumeChallenge {
        nonce: nonce.into(),
        expires,
    };
    b.bot_as(&b.admin, consume("used-once", auth::now() + 100))
        .await?;
    assert!(
        b.bot_as(&b.admin, consume("used-once", auth::now() + 100))
            .await
            .is_err()
    );
    assert!(
        b.bot_as(&b.admin, consume("too-far", auth::now() + 600))
            .await
            .is_err(),
        "challenge expiry is capped at 120 s"
    );

    let claim = b.bot_as(&b.device, Command::BeginClaim).await?;
    let propose = || Command::ProposeClaim {
        token: claim["token"].as_str().unwrap_or_default().into(),
        owner: "test-issuer|user".into(),
    };
    assert!(
        b.bot_as(&b.device, propose()).await.is_err(),
        "only admin proposes"
    );
    let proposed = b.bot_as(&b.admin, propose()).await?;
    let confirm = |code: &str| Command::ConfirmClaim { code: code.into() };
    assert!(b.bot_as(&b.device, confirm("000000")).await.is_err());
    let code = proposed["code"].as_str().context("code")?;
    b.bot_as(&b.device, confirm(code)).await?;
    assert!(b.bot_as(&b.device, confirm(code)).await.is_err());
    assert!(
        b.bot_as(&b.device, Command::BeginClaim).await.is_err(),
        "already claimed"
    );
    let snapshot = b.bot_as(&b.device, Command::Snapshot).await?;
    assert_eq!(snapshot["owner"], "test-issuer|user");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the isolated engine; run bun run test:integration"]
async fn revoke_blocks_bot_conversation_and_memory() -> Result<()> {
    let (h, _shared) = start().await;
    let b = TestBot::enroll(h).await?;
    b.mem(Command::Remember {
        key: "style".into(),
        value: "short".into(),
    })
    .await?;
    b.conv(Command::Snapshot).await?;
    assert!(
        b.bot_as(&b.device, Command::Revoke).await.is_err(),
        "only admin revokes"
    );
    b.bot_as(&b.admin, Command::Revoke).await?;
    assert!(b.bot_as(&b.device, Command::Snapshot).await.is_err());
    assert!(b.conv(Command::Snapshot).await.is_err());
    assert!(b.conv(Command::Replay { after: 0 }).await.is_err());
    assert!(
        b.submit("after-revoke", "Hello after revoke")
            .await
            .is_err()
    );
    assert!(b.mem(Command::Snapshot).await.is_err());
    assert!(b.mem_as(&b.agent, Command::Snapshot).await.is_err());
    assert!(
        b.mem(Command::Forget {
            key: "style".into()
        })
        .await
        .is_err(),
        "revoked memory must not change"
    );
    assert!(Events::open(&b, &b.device).await.is_err());
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Streaming, speech and isolation
// ---------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the isolated engine; run bun run test:integration"]
async fn chat_turn_is_one_provider_call_and_hides_private_text() -> Result<()> {
    const TAG: &str = "Hello buddy, chat check";
    let (h, _shared) = start().await;
    h.mock.script(TAG, |r| {
        let content = if r.direct {
            "Hello robot"
        } else {
            "PRIVATE_PLANNING_MUST_STAY_SILENT"
        };
        let delta = json!({"role":"assistant","content":content,
            "reasoning_content":"PRIVATE_REASONING_MUST_STAY_SILENT"});
        sse::body(format!("{}{}", sse::chunk(&delta, None), sse::stop()))
    });
    let b = TestBot::enroll(h).await?;
    let mut events = b.events().await?;
    let submit = |text: &str| Command::Submit {
        request_id: "same".into(),
        text: text.into(),
    };
    let first = b.conv(submit(TAG)).await?;
    let duplicate = b.conv(submit(TAG)).await?;
    assert_eq!(first["turn_id"], duplicate["turn_id"]);
    assert_eq!(duplicate["duplicate"], true);
    assert!(
        b.conv(submit("Different")).await.is_err(),
        "request id reused"
    );
    let turn = first["turn_id"].as_str().context("turn")?.to_owned();
    let done = events.wait_terminal(&turn, WAIT).await?;
    assert_eq!(end_kind(&done), Some("turn.completed"));
    assert_eq!(streamed_text(&done), "Hello robot");
    let all: Vec<_> = events.log().into_iter().map(|s| s.event).collect();
    let all = serde_json::to_string(&all)?;
    assert!(!all.contains("PRIVATE_"), "private text reached events");
    assert!(!all.contains("<spoken>"));
    assert_eq!(h.mock.requests(TAG).len(), 1, "chat must use one call");

    let spoken = |turn_id: &str| Command::SpokenAnswer {
        turn_id: turn_id.into(),
    };
    assert_eq!(b.conv(spoken(&turn)).await?["text"], "Hello robot");
    assert!(
        b.conv_as(&b.agent, spoken(&turn)).await.is_err(),
        "speech is device-only"
    );
    b.conv(Command::Cancel {
        turn_id: turn.clone(),
    })
    .await?;
    events.pull().await?;
    assert!(
        !kinds(&events.of_turn(&turn)).contains(&"turn.cancelled"),
        "cancelling a completed turn is a no-op"
    );
    assert_eq!(status(&b.turn_record(&turn).await?), "completed");
    let hits = b
        .conv(Command::SearchHistory {
            query: "chat check".into(),
        })
        .await?;
    assert_eq!(hits.as_array().context("hits")?.len(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the isolated engine; run bun run test:integration"]
async fn tool_turn_writes_memory_then_speaks_without_tools() -> Result<()> {
    const WARM: &str = "Warm up the tool history";
    const TAG: &str = "Remember my color for the tool test";
    let (h, _shared) = start().await;
    h.mock.script(WARM, |_| sse::text("Warm reply"));
    h.mock.script(TAG, |r| {
        if r.spoken {
            return sse::text("Saved your color.");
        }
        if r.tool_results < 2 {
            let value = if r.tool_results == 0 { "red" } else { "blue" };
            let args = json!({"key":"color","value":value});
            return sse::tool("call_color", "memory_remember", &args);
        }
        sse::text("PRIVATE_PLANNING_MUST_STAY_SILENT")
    });
    let b = TestBot::enroll(h).await?;
    b.turn("warm", WARM).await?;
    let (turn, done) = b.turn("tool", TAG).await?;
    assert_eq!(end_kind(&done), Some("turn.completed"));
    let tools = kinds(&done)
        .iter()
        .filter(|k| k.starts_with("tool."))
        .count();
    assert_eq!(
        tools,
        4,
        "two tools started and completed: {:?}",
        kinds(&done)
    );
    assert_eq!(streamed_text(&done), "Saved your color.");

    let requests = h.mock.requests(TAG);
    assert_eq!(requests.len(), 4, "direct, two action calls, then spoken");
    let history = requests[0]["messages"].as_array().context("messages")?;
    let has = |role: &str, content: &str| {
        history
            .iter()
            .any(|m| m["role"] == role && m["content"] == content)
    };
    assert!(has("user", WARM), "Rig request lost earlier user history");
    assert!(
        has("assistant", "Warm reply"),
        "Rig request lost earlier assistant history"
    );
    let spoken = requests.last().context("spoken request")?;
    assert!(spoken.to_string().contains("SPOKEN RESPONSE PHASE:"));
    assert!(
        spoken["tools"].as_array().is_none_or(Vec::is_empty),
        "spoken phase must not expose tools"
    );
    assert!(
        !spoken.to_string().contains("PRIVATE_PLANNING"),
        "planner prose leaked into the spoken request"
    );
    let memory = b
        .mem(Command::SearchMemory {
            query: "color".into(),
        })
        .await?;
    let memory = memory.as_array().context("memory")?;
    assert!(
        memory
            .iter()
            .any(|m| m["key"] == "color" && m["value"] == "blue")
    );
    assert_eq!(status(&b.turn_record(&turn).await?), "completed");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the isolated engine; run bun run test:integration"]
async fn direct_mode_tool_call_after_spoken_text_fails_the_turn() -> Result<()> {
    const TAG: &str = "Talk first, then look it up";
    let (h, _shared) = start().await;
    h.mock.script(TAG, |r| {
        if !r.direct {
            return sse::text("should not be called");
        }
        let args = json!({"key":"after_text","value":"leaked"});
        sse::body(format!(
            "{}{}",
            sse::content("Let me check that for you. "),
            sse::tool_call("late_call", "memory_remember", &args)
        ))
    });
    let b = TestBot::enroll(h).await?;
    let (turn, done) = b.turn("late-tool", TAG).await?;
    assert_eq!(end_kind(&done), Some("turn.failed"), "{:?}", kinds(&done));
    assert!(
        !kinds(&done).iter().any(|k| k.starts_with("tool.")),
        "no tool may run after spoken admission: {:?}",
        kinds(&done)
    );
    assert_eq!(h.mock.requests(TAG).len(), 1, "the turn is not retried");
    let memory = b.mem(Command::Snapshot).await?;
    assert!(!memory.to_string().contains("after_text"), "tool ran");
    assert_eq!(status(&b.turn_record(&turn).await?), "failed");
    assert!(
        b.conv(Command::SpokenAnswer { turn_id: turn })
            .await
            .is_err(),
        "a failed turn is not spoken"
    );
    Ok(())
}

const LATENCY_REPLY: &str = "Sure thing. The weather today is sunny and mild. You might want a \
light jacket later, since it cools off fast. Have a great afternoon!";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the isolated engine; run bun run test:integration"]
async fn speech_streams_one_numbered_chunk_per_sentence() -> Result<()> {
    const TAG: &str = "Latency check for streaming";
    // Measured alone so parallel tests do not skew the timing.
    let (h, _exclusive) = start_alone().await;
    h.mock.script(TAG, |r| {
        if r.direct {
            sse::tokens(LATENCY_REPLY, TOKEN_GAP, None)
        } else {
            sse::text("unexpected")
        }
    });
    let b = TestBot::enroll(h).await?;
    let mut events = b.events().await?;
    let started = Instant::now();
    let turn = b.submit("latency", TAG).await?;
    let done = events.wait_terminal(&turn, WAIT).await?;
    assert_eq!(end_kind(&done), Some("turn.completed"));
    let ready: Vec<&Seen> = done
        .iter()
        .filter(|s| s.event.kind == "speech.ready")
        .collect();
    let first = ready.first().context("no speech.ready")?.at - started;
    let gaps: Vec<u128> = ready
        .windows(2)
        .map(|w| (w[1].at - w[0].at).as_millis())
        .collect();
    println!(
        "LATENCY submit_to_first_speech_ready_ms={} chunks={} gaps_ms={gaps:?}",
        first.as_millis(),
        ready.len()
    );
    ensure!(ready.len() >= 4, "one chunk per sentence: {}", ready.len());
    ensure!(
        first < Duration::from_millis(1500),
        "first speech.ready took {first:?}"
    );
    let indexes: Vec<i64> = ready
        .iter()
        .filter_map(|s| s.event.payload["index"].as_i64())
        .collect();
    let count = u32::try_from(ready.len())?;
    let expected: Vec<i64> = (0..i64::from(count)).collect();
    assert_eq!(indexes, expected, "chunks are numbered in order");

    let chunk = |index| Command::SpeechChunk {
        turn_id: turn.clone(),
        index,
    };
    let mut spoken = String::new();
    for index in 0..count {
        let read = b.conv(chunk(index)).await?;
        assert_eq!(read["state"], "ready", "chunk {index}: {read}");
        assert_eq!(read["index"], index);
        assert_eq!(read["voice"]["name"], "Will");
        spoken.push_str(read["text"].as_str().context("chunk text")?);
    }
    assert_eq!(spoken.trim(), LATENCY_REPLY);
    assert_eq!(b.conv(chunk(count)).await?["state"], "done");
    assert!(
        b.conv(chunk(65)).await.is_err(),
        "a cursor past the chunk cap is rejected"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the isolated engine; run bun run test:integration"]
async fn barge_in_closes_the_provider_stream_and_commits_nothing_after() -> Result<()> {
    const TAG: &str = "Barge in on a long story";
    // Measured alone so parallel tests do not skew the timing.
    let (h, _exclusive) = start_alone().await;
    let dropped = Arc::new(std::sync::Mutex::new(None));
    let (notify, mut closed) = tokio::sync::watch::channel(false);
    let notify = Arc::new(notify);
    let guard_slot = dropped.clone();
    h.mock.script(TAG, move |r| {
        if !r.direct {
            return sse::text("unexpected");
        }
        let story = "The robot walked down to the corner store and bought milk. ";
        let reply = format!("Okay, here is a long story. {}", story.repeat(12));
        let guard = DropGuard(guard_slot.clone(), Some(notify.clone()));
        sse::tokens(&reply, TOKEN_GAP, Some(guard))
    });
    let b = TestBot::enroll(h).await?;
    let mut events = b.events().await?;
    let turn = b.submit("barge-in", TAG).await?;
    events
        .until(WAIT, "two speech chunks", |log| {
            let ready = log
                .iter()
                .filter(|s| s.event.turn_id == turn && s.event.kind == "speech.ready")
                .count();
            (ready >= 2).then_some(())
        })
        .await?;
    let cancelled_at = Instant::now();
    b.conv(Command::Cancel {
        turn_id: turn.clone(),
    })
    .await?;
    let acked = cancelled_at.elapsed();
    tokio::time::timeout(Duration::from_secs(5), closed.wait_for(|v| *v))
        .await
        .context("provider stream was never dropped")??;
    let close = dropped
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .context("drop time")?
        .saturating_duration_since(cancelled_at);
    let cancel = events.wait_kind(&turn, "turn.cancelled", WAIT).await?;
    events.pull().await?;
    let ours = events.of_turn(&turn);
    let late = ours
        .iter()
        .filter(|s| s.event.sequence > cancel.event.sequence)
        .count();
    let chunks = ours
        .iter()
        .filter(|s| s.event.kind == "speech.ready")
        .count();
    println!(
        "BARGE_IN cancel_ack_ms={} provider_close_after_cancel_ms={} \
         chunks_before_cancel={chunks} events_after_cancel={late}",
        acked.as_millis(),
        close.as_millis()
    );
    ensure!(late == 0, "{late} events committed after cancellation");
    ensure!(
        close < Duration::from_millis(300),
        "provider closed {close:?} after cancel"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the isolated engine; run bun run test:integration"]
async fn running_turn_pins_voice_rejects_busy_and_cancels_cleanly() -> Result<()> {
    const TAG: &str = "Slow response for the pinned voice test";
    let (h, _shared) = start().await;
    h.mock.script(TAG, |_| {
        sse::hang_after(sse::content("Partial output."), Duration::from_mins(1))
    });
    let b = TestBot::enroll(h).await?;
    let mut events = b.events().await?;
    let turn = b.submit("slow", TAG).await?;
    events.wait_kind(&turn, "speech.ready", WAIT).await?;
    let chunk = |index| Command::SpeechChunk {
        turn_id: turn.clone(),
        index,
    };
    let early = b.conv(chunk(0)).await?;
    assert_eq!(early["state"], "ready");
    assert_eq!(early["text"], "Partial output.");
    assert_eq!(b.conv(chunk(1)).await?["state"], "pending");
    assert!(b.conv_as(&b.agent, chunk(0)).await.is_err(), "device-only");
    assert!(
        b.submit("second", "Another request while busy")
            .await
            .is_err(),
        "a second turn must be rejected while one runs"
    );
    assert!(
        b.conv(Command::SpokenAnswer {
            turn_id: turn.clone()
        })
        .await
        .is_err(),
        "running output must not be spoken"
    );

    let current = b.conv(Command::GetVoiceConfig).await?;
    let mut changed: VoiceConfig = serde_json::from_value(current["config"].clone())?;
    changed.voice = Voice::Jessica;
    b.conv(Command::SetVoiceConfig {
        expected_revision: current["revision"].as_i64().context("revision")?,
        config: changed,
        reason: "Verify pinned turn voice".into(),
    })
    .await?;
    assert_eq!(
        b.conv(chunk(0)).await?["voice"],
        early["voice"],
        "voice is pinned per turn"
    );

    b.conv(Command::Cancel {
        turn_id: turn.clone(),
    })
    .await?;
    assert!(b.conv(chunk(0)).await.is_err(), "cancelled speech stops");
    let cancelled = events.wait_kind(&turn, "turn.cancelled", WAIT).await?;
    let record = b.turn_record(&turn).await?;
    assert_eq!(status(&record), "cancelled");
    assert_eq!(record["answer"], "Partial output.");
    events.pull().await?;
    let after: Vec<String> = events
        .log()
        .into_iter()
        .filter(|s| s.event.sequence > cancelled.event.sequence)
        .map(|s| s.event.kind)
        .collect();
    assert!(after.is_empty(), "events after cancellation: {after:?}");
    let (next, done) = b
        .turn("after-cancel", "A new turn after cancelling")
        .await?;
    assert_eq!(end_kind(&done), Some("turn.completed"));
    let spoken = b.conv(Command::SpokenAnswer { turn_id: next }).await?;
    assert_eq!(spoken["voice"]["name"], "Jessica", "new voice, next turn");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the isolated engine; run bun run test:integration"]
async fn bots_are_isolated_from_each_other() -> Result<()> {
    let (h, _shared) = start().await;
    let a = TestBot::enroll(h).await?;
    let b = TestBot::enroll(h).await?;
    assert!(b.conv_as(&a.device, Command::Snapshot).await.is_err());
    assert!(b.mem_as(&a.device, Command::Snapshot).await.is_err());
    assert!(b.bot_as(&a.admin, Command::Revoke).await.is_err());
    a.mem(Command::Remember {
        key: "secret".into(),
        value: "alpha only".into(),
    })
    .await?;
    let (turn_a, _) = a.turn("iso-a", "Isolation hello alpha").await?;
    let (turn_b, _) = b.turn("iso-b", "Isolation hello bravo").await?;
    let replay_b = b.conv(Command::Replay { after: 0 }).await?.to_string();
    assert!(!replay_b.contains(&turn_a), "bot B sees bot A's turn");
    let replay_a = a.conv(Command::Replay { after: 0 }).await?.to_string();
    assert!(!replay_a.contains(&turn_b), "bot A sees bot B's turn");
    let search = b
        .conv(Command::SearchHistory {
            query: "alpha".into(),
        })
        .await?;
    assert_eq!(search.as_array().context("hits")?.len(), 0);
    let memory_b = b.mem(Command::Snapshot).await?.to_string();
    assert!(!memory_b.contains("alpha only"));
    let feed_a = a.events().await?;
    assert!(feed_a.log().iter().all(|s| s.event.turn_id != turn_b));
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Persona, voice and context settings
// ---------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the isolated engine; run bun run test:integration"]
async fn voice_and_context_settings_are_compare_and_swap() -> Result<()> {
    let (h, _shared) = start().await;
    let b = TestBot::enroll(h).await?;
    let current = b.conv(Command::GetVoiceConfig).await?;
    assert_eq!(current["config"]["voice"], "will", "Will is the default");
    assert_eq!(current["revision"], 1);
    assert_eq!(current["choices"].as_array().map(Vec::len), Some(21));
    let set_voice = |expected_revision, config| Command::SetVoiceConfig {
        expected_revision,
        config,
        reason: "test speaker change".into(),
    };
    b.conv(set_voice(1, voice(Voice::Sarah))).await?;
    assert!(
        b.conv(set_voice(1, voice(Voice::Sarah))).await.is_err(),
        "stale revision"
    );
    let mut empty = voice(Voice::Sarah);
    empty.delivery = String::new();
    assert!(b.conv(set_voice(2, empty)).await.is_err(), "invalid config");
    assert!(
        b.conv_as(&b.admin, set_voice(2, voice(Voice::Will)))
            .await
            .is_err(),
        "admin is not a settings role"
    );
    b.conv_as(&b.agent, set_voice(2, voice(Voice::Bella)))
        .await?;
    let saved = b.conv(Command::GetVoiceConfig).await?;
    assert_eq!(saved["config"]["voice"], "bella");

    let context = |timezone: &str| ContextConfig {
        timezone: timezone.into(),
        location: Some("New York".into()),
    };
    let set_context = |expected_revision, config| Command::SetContextConfig {
        expected_revision,
        config,
        reason: "test location".into(),
    };
    b.conv(set_context(1, context("America/New_York"))).await?;
    assert!(
        b.conv(set_context(1, context("America/New_York")))
            .await
            .is_err(),
        "stale revision"
    );
    assert!(
        b.conv(set_context(2, context("Mars/Olympus")))
            .await
            .is_err(),
        "invalid timezone"
    );
    let saved = b.conv(Command::GetContextConfig).await?;
    assert_eq!(saved["config"]["timezone"], "America/New_York");
    assert_eq!(saved["revision"], 2);
    Ok(())
}

const CAPTAIN: &str = "You are Captain Buddy, a playful and optimistic robot.";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the isolated engine; run bun run test:integration"]
async fn system_prompt_tool_write_applies_to_the_next_turn() -> Result<()> {
    const UPDATE: &str = "Update your system prompt to Captain";
    const NEXT: &str = "Next turn after the prompt change";
    let (h, _shared) = start().await;
    h.mock.script(UPDATE, |r| {
        if r.spoken {
            sse::text("Aye aye.")
        } else if r.tool_results == 0 {
            let args = json!({"expected_revision":1,"prompt":CAPTAIN,"reason":"User asked"});
            sse::tool("persona-call", "update_system_prompt", &args)
        } else {
            sse::text("READY")
        }
    });
    h.mock.script(NEXT, |_| sse::text("Ahoy."));
    let b = TestBot::enroll(h).await?;
    b.conv(Command::SetContextConfig {
        expected_revision: 1,
        config: ContextConfig {
            timezone: "America/New_York".into(),
            location: None,
        },
        reason: "test".into(),
    })
    .await?;
    assert_eq!(b.conv(Command::GetSystemPrompt).await?["revision"], 1);
    let (_, done) = b.turn("persona-update", UPDATE).await?;
    assert_eq!(end_kind(&done), Some("turn.completed"));
    let changed = b.conv(Command::GetSystemPrompt).await?;
    assert_eq!(changed["revision"], 2);
    assert_eq!(changed["prompt"], CAPTAIN);
    let stale = Command::SetSystemPrompt {
        expected_revision: 1,
        prompt: "stale overwrite".into(),
        reason: "test".into(),
    };
    assert!(b.conv(stale).await.is_err());
    let versions = b.conv(Command::SystemPromptHistory).await?;
    assert_eq!(versions.as_array().context("versions")?.len(), 2);

    b.turn("next", NEXT).await?;
    let requests = h.mock.requests(NEXT);
    let request = requests.first().context("next request")?;
    let messages = request["messages"].as_array().context("messages")?;
    let loaded = messages.iter().any(|m| {
        matches!(m["role"].as_str(), Some("system" | "developer"))
            && m["content"].to_string().contains("Captain Buddy")
    });
    assert!(loaded, "next turn did not load the saved system prompt");
    let text = request.to_string();
    assert!(
        text.contains("America/New_York") && text.contains("local_datetime"),
        "runtime context missing"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the isolated engine; run bun run test:integration"]
async fn personas_save_activate_and_roll_back_together() -> Result<()> {
    let (h, _shared) = start().await;
    let b = TestBot::enroll(h).await?;
    let save = |persona| Command::SavePersona { persona };
    b.conv(save(draft("Captain", 0, "Captain version one")))
        .await?;
    assert!(b.conv(save(draft("Captain", 0, "again"))).await.is_err());
    assert!(b.conv(save(draft("Captain", 5, "skip"))).await.is_err());
    b.conv(save(draft("Captain", 1, "Captain version two")))
        .await?;
    assert!(b.conv(save(draft(" Padded", 0, "x"))).await.is_err());
    let list = b.conv(Command::ListPersonas).await?;
    let names: Vec<&str> = list
        .as_array()
        .context("list")?
        .iter()
        .filter_map(|p| p["name"].as_str())
        .collect();
    assert_eq!(names, ["Buddy", "Captain"]);
    b.conv(Command::SetVoiceConfig {
        expected_revision: 1,
        config: voice(Voice::Laura),
        reason: "bump voice revision".into(),
    })
    .await?;
    let activate = |version, prompt, voice| Command::ActivatePersona {
        name: "Captain".into(),
        version,
        expected_prompt_revision: prompt,
        expected_voice_revision: voice,
    };
    // A stale voice revision must roll back the prompt write in the same transaction.
    assert!(b.conv(activate(2, 1, 1)).await.is_err());
    assert_eq!(b.conv(Command::GetSystemPrompt).await?["revision"], 1);
    let activated = b.conv(activate(1, 1, 2)).await?;
    assert_eq!(activated["prompt_revision"], 2);
    assert_eq!(activated["voice_revision"], 3);
    let prompt = b.conv(Command::GetSystemPrompt).await?;
    assert_eq!(prompt["prompt"], "Captain version one");
    let voice_now = b.conv(Command::GetVoiceConfig).await?;
    assert_eq!(voice_now["config"]["voice"], "sarah");
    assert!(b.conv(activate(9, 2, 3)).await.is_err(), "missing version");
    let latest = b
        .conv(Command::GetPersona {
            name: "Captain".into(),
            version: 0,
        })
        .await?;
    assert_eq!(latest["version"], 2);
    assert_eq!(latest["prompt"], "Captain version two");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the isolated engine; run bun run test:integration"]
async fn persona_library_keeps_50_names_and_16_versions() -> Result<()> {
    let (h, _shared) = start().await;
    let b = TestBot::enroll(h).await?;
    let save = |persona| Command::SavePersona { persona };
    // Buddy is seeded, so 49 more names fill the library.
    for n in 1..=49 {
        let name = format!("Persona {n:02}");
        b.conv(save(draft(&name, 0, "A saved character."))).await?;
    }
    let full = b.conv(save(draft("Persona 50", 0, "One too many."))).await;
    assert!(full.is_err(), "the 51st name must be rejected");
    let list = b.conv(Command::ListPersonas).await?;
    assert_eq!(list.as_array().context("list")?.len(), 50);

    for version in 1..=17 {
        let prompt = format!("Revision {}", version + 1);
        b.conv(save(draft("Persona 01", version, &prompt))).await?;
    }
    let get = |version| Command::GetPersona {
        name: "Persona 01".into(),
        version,
    };
    let latest = b.conv(get(0)).await?;
    assert_eq!(latest["version"], 18);
    assert_eq!(latest["versions"].as_array().map(Vec::len), Some(16));
    assert!(b.conv(get(2)).await.is_err(), "older than 16 is pruned");
    assert_eq!(b.conv(get(3)).await?["prompt"], "Revision 3");

    for revision in 1..=17 {
        b.conv(Command::SetSystemPrompt {
            expected_revision: revision,
            prompt: format!("Prompt revision {}", revision + 1),
            reason: "cap test".into(),
        })
        .await?;
    }
    let history = b.conv(Command::SystemPromptHistory).await?;
    let kept = history.as_array().context("history")?.len();
    assert_eq!(kept, 16, "prompt history keeps 16 revisions");
    Ok(())
}

// ---------------------------------------------------------------------------------------
// History and memory
// ---------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the isolated engine; run bun run test:integration"]
async fn history_pages_newest_first_and_searches_phrases() -> Result<()> {
    let (h, _shared) = start().await;
    let b = TestBot::enroll(h).await?;
    let mut events = b.events().await?;
    let mut turns = Vec::new();
    for n in 1..=21 {
        let text = format!("History page turn {n:02}");
        let turn = b.submit(&format!("page-{n}"), &text).await?;
        events.wait_terminal(&turn, WAIT).await?;
        turns.push(turn);
    }
    let page = |before: &str| Command::History {
        before: before.into(),
    };
    let first = b.conv(page("")).await?;
    let first: Vec<&str> = first
        .as_array()
        .context("page")?
        .iter()
        .filter_map(|t| t["id"].as_str())
        .collect();
    let newest_first: Vec<&str> = turns.iter().rev().take(20).map(String::as_str).collect();
    assert_eq!(first, newest_first, "20 per page, newest first");
    let second = b.conv(page(first[19])).await?;
    let second = second.as_array().context("page")?;
    assert_eq!(second.len(), 1);
    assert_eq!(second[0]["id"], turns[0].as_str());
    assert_eq!(second[0]["text"], "History page turn 01");
    let empty = b.conv(page(&turns[0])).await?;
    assert_eq!(empty.as_array().map(Vec::len), Some(0));

    let search = |query: &str| Command::SearchHistory {
        query: query.into(),
    };
    let hits = b.conv(search("History page turn 07")).await?;
    let hits = hits.as_array().context("hits")?;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["turn_id"], turns[6].as_str());
    assert_eq!(hits[0]["assistant"], "Okay.");
    let broad = b.conv(search("History page")).await?;
    assert_eq!(broad.as_array().map(Vec::len), Some(6), "capped at 6 hits");
    b.conv(search("He said \"hi\"")).await?;
    assert!(b.conv(search("")).await.is_err());
    assert!(b.conv(search(&"x".repeat(257))).await.is_err());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the isolated engine; run bun run test:integration"]
async fn memory_remembers_searches_and_forgets() -> Result<()> {
    let (h, _shared) = start().await;
    let b = TestBot::enroll(h).await?;
    let remember = |key: &str, value: &str| Command::Remember {
        key: key.into(),
        value: value.into(),
    };
    b.mem(remember("favorite_color", "teal")).await?;
    b.mem(remember("pet", "a cat named Miso")).await?;
    b.mem(remember("favorite_color", "navy")).await?;
    assert!(b.mem(remember("empty", "")).await.is_err());
    let snapshot = b.mem(Command::Snapshot).await?;
    let facts = snapshot.as_array().context("facts")?;
    assert_eq!(facts.len(), 2, "upsert by key: {snapshot}");
    assert!(
        facts
            .iter()
            .any(|f| f["key"] == "favorite_color" && f["value"] == "navy")
    );
    let search = |query: &str| Command::SearchMemory {
        query: query.into(),
    };
    let found = b.mem(search("Miso")).await?;
    assert!(found.to_string().contains("a cat named Miso"));

    let forget = |key: &str| Command::Forget { key: key.into() };
    assert_eq!(b.mem(forget("pet")).await?["forgotten"], true);
    b.mem(forget("never-saved")).await?;
    let snapshot = b.mem(Command::Snapshot).await?;
    assert!(!snapshot.to_string().contains("Miso"), "{snapshot}");
    let found = b.mem(search("Miso cat")).await?;
    assert!(!found.to_string().contains("Miso"), "still searchable");
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Restart recovery
// ---------------------------------------------------------------------------------------

struct Customized {
    bot: TestBot,
    slow_turn: String,
    buddy_v1: Value,
}

/// A claimed bot with custom settings, persona versions and memory, mid-stream on a turn
/// whose provider never finishes.
async fn customized_bot_streaming(h: &Harness, tag: &str) -> Result<Customized> {
    let b = TestBot::enroll(h).await?;
    b.claim("test-issuer|user").await?;
    b.conv(Command::SetVoiceConfig {
        expected_revision: 1,
        config: voice(Voice::Sarah),
        reason: "custom".into(),
    })
    .await?;
    b.conv(Command::SetContextConfig {
        expected_revision: 1,
        config: ContextConfig {
            timezone: "America/New_York".into(),
            location: Some("New York".into()),
        },
        reason: "custom".into(),
    })
    .await?;
    b.conv(Command::SetSystemPrompt {
        expected_revision: 1,
        prompt: "You are a custom robot.".into(),
        reason: "custom".into(),
    })
    .await?;
    for (version, prompt) in [(0, "Captain one"), (1, "Captain two")] {
        let persona = draft("Captain", version, prompt);
        b.conv(Command::SavePersona { persona }).await?;
    }
    b.mem(Command::Remember {
        key: "style".into(),
        value: "short".into(),
    })
    .await?;
    let buddy_v1 = b
        .conv(Command::GetPersona {
            name: "Buddy".into(),
            version: 1,
        })
        .await?;
    let mut events = b.events().await?;
    let slow_turn = b.submit("slow-restart", tag).await?;
    events.wait_kind(&slow_turn, "text.delta", WAIT).await?;
    Ok(Customized {
        bot: b,
        slow_turn,
        buddy_v1,
    })
}

async fn assert_customized_survived(c: &Customized) -> Result<()> {
    let b = &c.bot;
    let owner = b.bot_as(&b.device, Command::Snapshot).await?;
    assert_eq!(owner["owner"], "test-issuer|user");
    let voice = b.conv(Command::GetVoiceConfig).await?;
    assert_eq!(voice["config"]["voice"], "sarah");
    assert_eq!(voice["revision"], 2);
    let context = b.conv(Command::GetContextConfig).await?;
    assert_eq!(context["config"]["timezone"], "America/New_York");
    let prompt = b.conv(Command::GetSystemPrompt).await?;
    assert_eq!(prompt["prompt"], "You are a custom robot.");
    assert_eq!(prompt["revision"], 2);
    let persona = |name: &str, version| Command::GetPersona {
        name: name.into(),
        version,
    };
    let captain = b.conv(persona("Captain", 0)).await?;
    assert_eq!(captain["version"], 2);
    assert_eq!(captain["prompt"], "Captain two");
    // Factory-companion adoption runs on every start and must leave these rows alone.
    let buddy = b.conv(persona("Buddy", 1)).await?;
    assert_eq!(buddy["prompt"], c.buddy_v1["prompt"]);
    assert_eq!(buddy["voice"], c.buddy_v1["voice"]);
    let memory = b.mem(Command::Snapshot).await?;
    assert!(memory.to_string().contains("short"), "{memory}");
    Ok(())
}

/// A second bot inside a web search: its intent row is committed, its result is not.
struct InFlightTool {
    bot: TestBot,
    turn: String,
    /// Proxy CONNECTs and model requests for the search prompt before this turn.
    connects: usize,
    requests: usize,
    /// Kept open across the restart so a failure pushed by the old process is stamped.
    events: Events,
}

async fn bot_inside_tool_effect(h: &Harness, prompt: &str) -> Result<InFlightTool> {
    let bot = TestBot::enroll(h).await?;
    let mut events = bot.events().await?;
    let connects = h.mock.connects.load(Ordering::SeqCst);
    let requests = h.mock.requests(prompt).len();
    let turn = bot.submit("search", prompt).await?;
    events.wait_kind(&turn, "tool.started", WAIT).await?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while h.mock.connects.load(Ordering::SeqCst) == connects {
        ensure!(Instant::now() < deadline, "search never reached the mock");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Ok(InFlightTool {
        bot,
        turn,
        connects,
        requests,
        events,
    })
}

/// The search client gives the proxy tunnel 5 s (`connect_timeout` in `model::search`). On a
/// loaded machine the graceful stop can reach the actor later than that; the search then fails
/// on its own and the old process pushes `turn.failed` before the restart finishes. That run
/// did not exercise the checkpoint, so the tool leg is retried once.
const TOOL_LEG_ATTEMPTS: usize = 2;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the isolated engine; run bun run test:integration"]
async fn restart_interrupts_streams_and_fails_closed_on_unresolved_tools() -> Result<()> {
    const SLOW: &str = "Slow response across a restart";
    const SEARCH: &str = "Search the web for robot news";
    let (h, _exclusive) = start_alone().await;
    let proxied =
        std::env::var("HTTPS_PROXY").is_ok_and(|p| p.ends_with(&format!(":{}", h.model_port)));
    let keyed = std::env::var("BRAVE_SEARCH_API_KEY").is_ok_and(|k| !k.is_empty());
    ensure!(
        proxied && keyed,
        "run through bun run test:integration, which routes web search into the mock"
    );
    h.mock.script(SLOW, |_| {
        sse::hang_after(sse::content("Partial output."), Duration::from_mins(2))
    });
    h.mock.script(SEARCH, |r| {
        if r.direct && r.tool_results == 0 {
            sse::tool("search-1", "web_search", &json!({"query":"robot news"}))
        } else {
            sse::text("The search must never be answered.")
        }
    });
    let custom = customized_bot_streaming(h, SLOW).await?;
    let mut tool = bot_inside_tool_effect(h, SEARCH).await?;
    let mut attempt = 1;
    loop {
        let downtime = h.restart().await?;
        let restarted = Instant::now();
        println!(
            "RESTART engine_and_registry_ms={} attempt={attempt}",
            downtime.as_millis()
        );

        if attempt == 1 {
            let mut events = custom.bot.events().await?;
            let done = events.wait_terminal(&custom.slow_turn, WAIT).await?;
            let end = terminal(&done).context("terminal")?;
            assert_eq!(end.event.kind, "turn.interrupted", "{:?}", kinds(&done));
            assert_eq!(end.event.payload["reason"], "server_restart");
            let record = custom.bot.turn_record(&custom.slow_turn).await?;
            assert_eq!(status(&record), "interrupted");
            assert_eq!(record["answer"], "Partial output.");
            assert_customized_survived(&custom).await?;
        }

        let done = tool.events.wait_terminal(&tool.turn, WAIT).await?;
        let end = terminal(&done).context("tool terminal")?;
        assert_eq!(end.event.kind, "turn.failed", "{:?}", kinds(&done));
        let starts = kinds(&done)
            .iter()
            .filter(|k| **k == "turn.started")
            .count();
        if starts == 1 && end.at < restarted && attempt < TOOL_LEG_ATTEMPTS {
            println!(
                "RESTART search timed out before the stop reached its actor ({} ms before the \
                 restart finished); retrying the tool leg",
                (restarted - end.at).as_millis()
            );
            attempt += 1;
            tool = bot_inside_tool_effect(h, SEARCH).await?;
            continue;
        }
        assert_eq!(
            starts,
            2,
            "resumed from its ready checkpoint (attempt {attempt}): {:?}",
            kinds(&done)
        );
        assert!(!kinds(&done).contains(&"tool.completed"));
        assert_eq!(
            h.mock.connects.load(Ordering::SeqCst),
            tool.connects + 1,
            "an unresolved tool effect must not be replayed"
        );
        assert_eq!(
            h.mock.requests(SEARCH).len(),
            tool.requests + 1,
            "no new model call"
        );
        break;
    }
    drop(tool.bot);

    let (_, after) = custom
        .bot
        .turn("after-restart", "Hello after restart")
        .await?;
    assert_eq!(end_kind(&after), Some("turn.completed"));
    Ok(())
}

//! Runs against an isolated real Rivet engine on port 16420, never the user's default engine.
use anyhow::{Context, Result, ensure};
use axum::response::IntoResponse;
use rivetkit::{
    EngineSpawnMode, ServeConfig, TypedClientExt,
    client::{Client, ClientConfig, GetOrCreateOptions},
};
use robot_buddy::{
    Bot,
    auth::{self, Session},
    protocol::*,
};
use serde_json::{Value, json};
use std::time::Duration;
fn token(bot: &str, role: &str) -> Result<String> {
    auth::sign(
        std::env::var("BUDDY_SIGNING_KEY")?.as_bytes(),
        "session",
        &Session {
            bot_id: bot.into(),
            role: role.into(),
            expires: auth::now() + 600,
        },
    )
}
#[tokio::test]
#[ignore = "starts isolated real engine; run bun run test:integration"]
async fn registration_streaming_isolation_and_restart() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("warn,robot_buddy=info")
        .try_init();
    static GREETING_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    GREETING_CALLS.store(0, std::sync::atomic::Ordering::SeqCst);
    let mock=axum::Router::new().route("/v1/chat/completions",axum::routing::post(|axum::Json(request):axum::Json<Value>|async move {
        let messages=request["messages"].as_array().expect("messages");
        let greeting=messages.iter().rev().find(|m|m["role"]=="user").is_some_and(|m|m["content"]=="Hello");
        if greeting { GREETING_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst); }
        let direct=messages.iter().any(|m|m["content"].to_string().contains("RESPONSE ROUTING:"));
        let spoken=messages.iter().any(|m|m["content"].to_string().contains("SPOKEN RESPONSE PHASE:"));
        if spoken {assert!(!messages.iter().any(|m|m["content"].to_string().contains("PRIVATE_PLANNING_MUST_STAY_SILENT")),"planner prose leaked into spoken context");assert!(request["tools"].as_array().is_none_or(Vec::is_empty),"spoken phase must not expose tools");}
        if (spoken || direct) && messages.iter().rev().find(|m|m["role"]=="user").is_some_and(|m|m["content"].to_string().contains("Slow response")) {
            assert!(messages.iter().any(|m|["system","developer"].iter().any(|role|m["role"]==*role) && m["content"].to_string().contains("Captain Buddy")),"next turn did not load actor system prompt");
            assert!(messages.iter().any(|m|m["content"].to_string().contains("America/New_York") && m["content"].to_string().contains("local_datetime")),"runtime context missing");
            let text=json!({"id":"chatcmpl-slow","object":"chat.completion.chunk","created":1,"model":"fixture","choices":[{"index":0,"delta":{"role":"assistant","content":"Partial output."},"finish_reason":null}]});
            let stream=futures_util::stream::unfold(Some(format!("data: {text}\n\n")),|state|async move {
                if let Some(text)=state {Some((Ok::<_,std::io::Error>(text),None))} else {tokio::time::sleep(Duration::from_secs(30)).await;None}
            });
            return ([("content-type","text/event-stream")],axum::body::Body::from_stream(stream)).into_response();
        }
        if !spoken && messages.iter().rev().find(|m|m["role"]=="user").is_some_and(|m|m["content"]=="Update your system prompt") && messages.last().is_some_and(|m|m["role"]!="tool") {
            let call=json!({"id":"chatcmpl-persona","object":"chat.completion.chunk","created":1,"model":"fixture","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"persona-call","type":"function","function":{"name":"update_system_prompt","arguments":json!({"expected_revision":1,"prompt":"You are Captain Buddy, a playful and optimistic robot.","reason":"User asked for a new persona"}).to_string()}}]},"finish_reason":"tool_calls"}]});
            return ([("content-type","text/event-stream")],format!("data: {call}\n\ndata: [DONE]\n\n")).into_response();
        }
        let wants_tool=messages.iter().any(|m|m["content"].as_str().is_some_and(|s|s.contains("Remember my color"))) && messages.iter().filter(|m|m["role"]=="tool").count()<2;
        if !spoken && wants_tool {
            assert!(messages.iter().any(|m|m["role"]=="user" && m["content"]=="Hello"), "Rig request lost earlier user history");
            assert!(messages.iter().any(|m|m["role"]=="assistant" && m["content"]=="Hello robot"), "Rig request lost earlier assistant history");
            let value=if messages.iter().any(|m|m["role"]=="tool") {"blue"} else {"red"};
            let arguments=json!({"key":"color","value":value}).to_string();
            let call=json!({"id":"chatcmpl-tool","object":"chat.completion.chunk","created":1,"model":"fixture","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_color","type":"function","function":{"name":"memory_remember","arguments":arguments}}]},"finish_reason":"tool_calls"}]});
            return ([("content-type","text/event-stream")],format!("data: {call}\n\ndata: [DONE]\n\n")).into_response();
        }
        let text=json!({"id":"chatcmpl-fixture","object":"chat.completion.chunk","created":1,"model":"fixture","choices":[{"index":0,"delta":{"role":"assistant","reasoning_content":"PRIVATE_REASONING_MUST_STAY_SILENT","content":if (greeting && direct) || spoken {"Hello robot"} else {"PRIVATE_PLANNING_MUST_STAY_SILENT"}},"finish_reason":null}]});
        let stop=json!({"id":"chatcmpl-fixture","object":"chat.completion.chunk","created":1,"model":"fixture","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}});
        ([("content-type","text/event-stream")],format!("data: {text}\n\ndata: {stop}\n\ndata: [DONE]\n\n")).into_response()
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:19091").await?;
    let mock_task = tokio::spawn(async move { axum::serve(listener, mock).await });
    let shutdown = tokio_util::sync::CancellationToken::new();
    let config = ServeConfig {
        endpoint: "http://127.0.0.1:16420".into(),
        token: Some("default".into()),
        namespace: "default".into(),
        version: 1,
        engine_port: Some(16420),
        engine_spawn: EngineSpawnMode::Always,
        engine_auto_download: true,
        start_services: false,
        pool_name: "robot-buddy-test".into(),
        ..Default::default()
    };
    let serving =
        tokio::spawn(robot_buddy::registry().serve_with_config(config.clone(), shutdown.clone()));
    let http = reqwest::Client::new();
    let mut ready = false;
    for _ in 0..150 {
        if http
            .get("http://127.0.0.1:16420/health")
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
        {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    if serving.is_finished() {
        serving
            .await
            .context("join registry")?
            .context("serve registry")?;
        anyhow::bail!("registry exited");
    }
    ensure!(ready, "engine never became healthy");
    tokio::time::sleep(Duration::from_secs(2)).await;
    if serving.is_finished() {
        serving
            .await
            .context("join registry")?
            .context("serve registry")?;
        anyhow::bail!("registry exited after health");
    }
    let bot = auth::random();
    let admin = token(&bot, "admin")?;
    let device = token(&bot, "device")?;
    let client = Client::new(
        ClientConfig::new("http://127.0.0.1:16420")
            .token("default")
            .pool_name("robot-buddy-test")
            .disable_metadata_lookup(true),
    );
    let h = client.get_or_create_typed::<Bot>(
        "bot",
        [bot.clone()],
        GetOrCreateOptions {
            params: Some(json!({"session":device})),
            ..Default::default()
        },
    )?;
    let enrolled = h
        .send(Call {
            session: admin.clone(),
            command: Command::Enroll {
                device_id: "test-device".into(),
                public_key: "test-key".into(),
            },
        })
        .await?;
    assert_eq!(enrolled["bot_id"], bot);
    let c = client.get_or_create_typed::<robot_buddy::conversation::Conversation>(
        "conversation",
        [bot.clone()],
        GetOrCreateOptions {
            params: Some(json!({"session":device})),
            ..Default::default()
        },
    )?;
    let m = client.get_or_create_typed::<robot_buddy::memory::Memory>(
        "memory",
        [bot.clone()],
        GetOrCreateOptions {
            params: Some(json!({"session":device})),
            ..Default::default()
        },
    )?;
    m.inner().resolve().await?;

    for _ in 0..10 {
        h.send(Call {
            session: admin.clone(),
            command: Command::Enroll {
                device_id: "test-device".into(),
                public_key: "test-key".into(),
            },
        })
        .await?;
    }
    assert!(
        h.send(Call {
            session: token("wrong-bot", "device")?,
            command: Command::Snapshot
        })
        .await
        .is_err()
    );
    m.send(Call {
        session: device.clone(),
        command: Command::Remember {
            key: "style".into(),
            value: "short".into(),
        },
    })
    .await?;
    let first = c
        .send(Call {
            session: device.clone(),
            command: Command::Submit {
                request_id: "same".into(),
                text: "Hello".into(),
            },
        })
        .await?;
    let duplicate = c
        .send(Call {
            session: device.clone(),
            command: Command::Submit {
                request_id: "same".into(),
                text: "Hello".into(),
            },
        })
        .await?;
    assert_eq!(first["turn_id"], duplicate["turn_id"]);
    assert!(
        c.send(Call {
            session: device.clone(),
            command: Command::Submit {
                request_id: "same".into(),
                text: "Different".into()
            }
        })
        .await
        .is_err()
    );
    let mut events = Value::Null;
    for _ in 0..100 {
        events = c
            .send(Call {
                session: device.clone(),
                command: Command::Replay { after: 0 },
            })
            .await?;
        if events
            .as_array()
            .context("events")?
            .iter()
            .any(|e| e["kind"] == "turn.completed")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        events
            .as_array()
            .context("events")?
            .iter()
            .any(|e| e["kind"] == "text.delta" && e["payload"]["text"] == "Hello robot"),
        "{events}"
    );
    assert!(
        !events
            .to_string()
            .contains("PRIVATE_REASONING_MUST_STAY_SILENT")
            && !events
                .to_string()
                .contains("PRIVATE_PLANNING_MUST_STAY_SILENT")
    );
    assert_eq!(
        GREETING_CALLS.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "ordinary chat must use exactly one provider call"
    );
    assert!(!events.to_string().contains("<spoken>"));
    let spoken = c
        .send(Call {
            session: device.clone(),
            command: Command::SpokenAnswer {
                turn_id: first["turn_id"].as_str().context("id")?.into(),
            },
        })
        .await?;
    assert_eq!(spoken["text"], "Hello robot");
    assert!(
        c.send(Call {
            session: token(&bot, "agent")?,
            command: Command::SpokenAnswer {
                turn_id: first["turn_id"].as_str().context("id")?.into()
            }
        })
        .await
        .is_err()
    );
    c.send(Call {
        session: device.clone(),
        command: Command::Cancel {
            turn_id: first["turn_id"].as_str().context("turn")?.into(),
        },
    })
    .await?;
    let replay = c
        .send(Call {
            session: device.clone(),
            command: Command::Replay { after: 0 },
        })
        .await?;
    assert!(
        !replay
            .as_array()
            .context("events")?
            .iter()
            .any(|e| e["kind"] == "turn.cancelled")
    );
    let history = c
        .send(Call {
            session: device.clone(),
            command: Command::SearchHistory {
                query: "Hello".into(),
            },
        })
        .await?;
    assert_eq!(history.as_array().context("history")?.len(), 1);
    c.send(Call {
        session: device.clone(),
        command: Command::Submit {
            request_id: "tool-turn".into(),
            text: "Remember my color is blue".into(),
        },
    })
    .await?;
    let mut saved = false;
    for _ in 0..100 {
        let snapshot = c
            .send(Call {
                session: device.clone(),
                command: Command::Snapshot,
            })
            .await?;
        if snapshot["turns"]
            .as_array()
            .context("turns")?
            .iter()
            .any(|t| t["text"] == "Remember my color is blue" && t["status"] == "completed")
        {
            saved = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(saved, "tool turn did not finish");
    let memory = m
        .send(Call {
            session: device.clone(),
            command: Command::SearchMemory {
                query: "color".into(),
            },
        })
        .await?;
    assert!(
        memory
            .as_array()
            .context("memory")?
            .iter()
            .any(|m| m["key"] == "color" && m["value"] == "blue")
    );
    let v = c
        .send(Call {
            session: device.clone(),
            command: Command::GetVoiceConfig,
        })
        .await?;
    assert_eq!(v["config"]["voice"], "adrian");
    let voice = robot_buddy::voice::VoiceConfig {
        voice: robot_buddy::voice::Voice::Delia,
        delivery: "Calm and warm.".into(),
        synthesis: Default::default(),
        speech_rules: Vec::new(),
    };
    c.send(Call {
        session: device.clone(),
        command: Command::SetVoiceConfig {
            expected_revision: 1,
            config: voice.clone(),
            reason: "test speaker change".into(),
        },
    })
    .await?;
    assert!(
        c.send(Call {
            session: device.clone(),
            command: Command::SetVoiceConfig {
                expected_revision: 1,
                config: voice,
                reason: "stale".into()
            }
        })
        .await
        .is_err()
    );
    let context = robot_buddy::context::ContextConfig {
        timezone: "America/New_York".into(),
        location: Some("New York".into()),
    };
    c.send(Call {
        session: device.clone(),
        command: Command::SetContextConfig {
            expected_revision: 1,
            config: context.clone(),
            reason: "test location".into(),
        },
    })
    .await?;
    assert!(
        c.send(Call {
            session: device.clone(),
            command: Command::SetContextConfig {
                expected_revision: 1,
                config: context,
                reason: "stale".into()
            }
        })
        .await
        .is_err()
    );
    let original = c
        .send(Call {
            session: device.clone(),
            command: Command::GetSystemPrompt,
        })
        .await?;
    assert_eq!(original["revision"], 1);
    c.send(Call {
        session: device.clone(),
        command: Command::Submit {
            request_id: "persona-update".into(),
            text: "Update your system prompt".into(),
        },
    })
    .await?;
    let mut persona_done = false;
    for _ in 0..100 {
        let snapshot = c
            .send(Call {
                session: device.clone(),
                command: Command::Snapshot,
            })
            .await?;
        if snapshot["turns"]
            .as_array()
            .context("turns")?
            .iter()
            .any(|t| t["text"] == "Update your system prompt" && t["status"] == "completed")
        {
            persona_done = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(persona_done, "prompt tool turn did not complete");
    let changed = c
        .send(Call {
            session: device.clone(),
            command: Command::GetSystemPrompt,
        })
        .await?;
    assert_eq!(changed["revision"], 2);
    assert_eq!(
        changed["prompt"],
        "You are Captain Buddy, a playful and optimistic robot."
    );
    assert!(
        c.send(Call {
            session: device.clone(),
            command: Command::SetSystemPrompt {
                expected_revision: 1,
                prompt: "stale overwrite".into(),
                reason: "test".into()
            }
        })
        .await
        .is_err()
    );
    let versions = c
        .send(Call {
            session: device.clone(),
            command: Command::SystemPromptHistory,
        })
        .await?;
    assert_eq!(versions.as_array().context("versions")?.len(), 2);
    let mut draft = robot_buddy::persona::PersonaDraft {
        name: "Captain".into(),
        expected_version: 0,
        prompt: changed["prompt"].as_str().context("prompt")?.into(),
        voice: robot_buddy::voice::VoiceConfig {
            voice: robot_buddy::voice::Voice::Delia,
            delivery: "Calm and warm.".into(),
            synthesis: Default::default(),
            speech_rules: Vec::new(),
        },
        reason: "Save Captain".into(),
    };
    c.send(Call {
        session: device.clone(),
        command: Command::SavePersona {
            persona: draft.clone(),
        },
    })
    .await?;
    assert!(
        c.send(Call {
            session: device.clone(),
            command: Command::SavePersona {
                persona: draft.clone()
            }
        })
        .await
        .is_err()
    );
    draft.expected_version = 1;
    draft.prompt = "Captain version two".into();
    c.send(Call {
        session: device.clone(),
        command: Command::SavePersona { persona: draft },
    })
    .await?;
    // A stale voice revision must roll back the prompt write in the same transaction.
    assert!(
        c.send(Call {
            session: device.clone(),
            command: Command::ActivatePersona {
                name: "Captain".into(),
                version: 2,
                expected_prompt_revision: 2,
                expected_voice_revision: 1
            }
        })
        .await
        .is_err()
    );
    assert_eq!(
        c.send(Call {
            session: device.clone(),
            command: Command::GetSystemPrompt
        })
        .await?["revision"],
        2
    );
    c.send(Call {
        session: device.clone(),
        command: Command::ActivatePersona {
            name: "Captain".into(),
            version: 1,
            expected_prompt_revision: 2,
            expected_voice_revision: 2,
        },
    })
    .await?;
    let consume = || Call {
        session: admin.clone(),
        command: Command::ConsumeChallenge {
            nonce: "used-once".into(),
            expires: auth::now() + 100,
        },
    };
    h.send(consume()).await?;
    assert!(h.send(consume()).await.is_err());
    let claim = h
        .send(Call {
            session: device.clone(),
            command: Command::BeginClaim,
        })
        .await?;
    let proposed = h
        .send(Call {
            session: admin.clone(),
            command: Command::ProposeClaim {
                token: claim["token"].as_str().context("token")?.into(),
                owner: "test-issuer|user".into(),
            },
        })
        .await?;
    h.send(Call {
        session: device.clone(),
        command: Command::ConfirmClaim {
            code: proposed["code"].as_str().context("code")?.into(),
        },
    })
    .await?;
    assert!(
        h.send(Call {
            session: device.clone(),
            command: Command::BeginClaim
        })
        .await
        .is_err()
    );
    let slow = c
        .send(Call {
            session: device.clone(),
            command: Command::Submit {
                request_id: "slow".into(),
                text: "Slow response".into(),
            },
        })
        .await?;
    let mut partial = false;
    for _ in 0..100 {
        let events = c
            .send(Call {
                session: device.clone(),
                command: Command::Replay { after: 0 },
            })
            .await?;
        if events
            .as_array()
            .context("events")?
            .iter()
            .any(|e| e["turn_id"] == slow["turn_id"] && e["kind"] == "text.delta")
        {
            partial = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(partial, "slow model did not stream");
    let speech_id = slow["turn_id"].as_str().context("id")?.to_owned();
    let early = c
        .send(Call {
            session: device.clone(),
            command: Command::SpeechChunk {
                turn_id: speech_id.clone(),
                index: 0,
            },
        })
        .await?;
    assert_eq!(early["state"], "ready");
    assert_eq!(early["text"], "Partial output.");
    assert_eq!(
        c.send(Call {
            session: device.clone(),
            command: Command::SpeechChunk {
                turn_id: speech_id.clone(),
                index: 1
            }
        })
        .await?["state"],
        "pending"
    );
    assert!(
        c.send(Call {
            session: token(&bot, "agent")?,
            command: Command::SpeechChunk {
                turn_id: speech_id.clone(),
                index: 0
            }
        })
        .await
        .is_err()
    );
    let current = c
        .send(Call {
            session: device.clone(),
            command: Command::GetVoiceConfig,
        })
        .await?;
    let mut changed_voice: robot_buddy::voice::VoiceConfig =
        serde_json::from_value(current["config"].clone())?;
    changed_voice.voice = robot_buddy::voice::Voice::Sheila;
    c.send(Call {
        session: device.clone(),
        command: Command::SetVoiceConfig {
            expected_revision: current["revision"].as_i64().context("revision")?,
            config: changed_voice,
            reason: "Verify pinned turn voice".into(),
        },
    })
    .await?;
    assert_eq!(
        c.send(Call {
            session: device.clone(),
            command: Command::SpeechChunk {
                turn_id: speech_id.clone(),
                index: 0
            }
        })
        .await?["voice"],
        early["voice"]
    );
    c.send(Call {
        session: device.clone(),
        command: Command::SetVoiceConfig {
            expected_revision: current["revision"].as_i64().context("revision")? + 1,
            config: serde_json::from_value(current["config"].clone())?,
            reason: "Restore test voice".into(),
        },
    })
    .await?;

    assert!(
        c.send(Call {
            session: device.clone(),
            command: Command::SpokenAnswer {
                turn_id: slow["turn_id"].as_str().context("id")?.into()
            }
        })
        .await
        .is_err(),
        "running output must not be spoken"
    );

    c.send(Call {
        session: device.clone(),
        command: Command::Cancel {
            turn_id: slow["turn_id"].as_str().context("turn id")?.into(),
        },
    })
    .await?;
    assert!(
        c.send(Call {
            session: device.clone(),
            command: Command::SpeechChunk {
                turn_id: speech_id.clone(),
                index: 0
            }
        })
        .await
        .is_err()
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    let cancelled = c
        .send(Call {
            session: device.clone(),
            command: Command::Snapshot,
        })
        .await?;
    assert!(
        cancelled["turns"]
            .as_array()
            .context("turns")?
            .iter()
            .any(|t| t["id"] == slow["turn_id"]
                && t["status"] == "cancelled"
                && t["answer"] == "Partial output.")
    );
    let after_cancel = c
        .send(Call {
            session: device.clone(),
            command: Command::Replay {
                after: cancelled["last_sequence"].as_i64().context("cursor")?,
            },
        })
        .await?;
    assert!(
        after_cancel.as_array().context("events")?.is_empty(),
        "events appeared after cancellation"
    );
    let slow = c
        .send(Call {
            session: device.clone(),
            command: Command::Submit {
                request_id: "slow-restart".into(),
                text: "Slow response".into(),
            },
        })
        .await?;
    for _ in 0..100 {
        let events = c
            .send(Call {
                session: device.clone(),
                command: Command::Replay {
                    after: cancelled["last_sequence"].as_i64().context("cursor")?,
                },
            })
            .await?;
        if events
            .as_array()
            .context("events")?
            .iter()
            .any(|e| e["turn_id"] == slow["turn_id"] && e["kind"] == "text.delta")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    shutdown.cancel();
    serving
        .await
        .context("join registry")?
        .context("serve registry")?;
    let shutdown = tokio_util::sync::CancellationToken::new();
    let serving = tokio::spawn(robot_buddy::registry().serve_with_config(config, shutdown.clone()));
    tokio::time::sleep(Duration::from_secs(3)).await;
    let snapshot = h
        .send(Call {
            session: device.clone(),
            command: Command::Snapshot,
        })
        .await?;
    let turns = c
        .send(Call {
            session: device.clone(),
            command: Command::Snapshot,
        })
        .await?;
    assert!(
        turns["turns"]
            .as_array()
            .context("turns")?
            .iter()
            .any(|t| t["id"] == slow["turn_id"]
                && t["status"] == "interrupted"
                && t["answer"] == "Partial output.")
    );
    assert_eq!(snapshot["owner"], "test-issuer|user");
    let restored = c
        .send(Call {
            session: device.clone(),
            command: Command::GetSystemPrompt,
        })
        .await?;
    assert_eq!(
        c.send(Call {
            session: device.clone(),
            command: Command::GetVoiceConfig
        })
        .await?["config"]["voice"],
        "delia"
    );
    assert_eq!(
        c.send(Call {
            session: device.clone(),
            command: Command::GetContextConfig
        })
        .await?["config"]["timezone"],
        "America/New_York"
    );
    assert_eq!(
        c.send(Call {
            session: device.clone(),
            command: Command::GetPersona {
                name: "Captain".into(),
                version: 0
            }
        })
        .await?["version"],
        2
    );
    assert_eq!(
        c.send(Call {
            session: device.clone(),
            command: Command::GetPersona {
                name: "Captain".into(),
                version: 1
            }
        })
        .await?["prompt"],
        restored["prompt"]
    );
    assert_eq!(restored["revision"], 3);
    assert_eq!(
        restored["prompt"],
        "You are Captain Buddy, a playful and optimistic robot."
    );

    assert!(
        m.send(Call {
            session: device.clone(),
            command: Command::Snapshot
        })
        .await?
        .as_array()
        .context("memory")?
        .iter()
        .any(|m| m["value"] == "short")
    );
    h.send(Call {
        session: admin,
        command: Command::Revoke,
    })
    .await?;
    assert!(
        h.send(Call {
            session: device,
            command: Command::Snapshot
        })
        .await
        .is_err()
    );
    shutdown.cancel();
    serving
        .await
        .context("join registry")?
        .context("serve registry")?;
    mock_task.abort();
    Ok(())
}

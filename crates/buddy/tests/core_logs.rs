//! No log line carries what the user said, what the model answered or what a tool saved.
//! Every `robot_buddy` line at trace level, and every other line at info, is captured
//! across a direct turn, a tool turn and a failed turn, then searched for sentinels.
//!
//! Run with the integration environment of `bun run test:integration`, for example:
//! `cargo test -p robot-buddy --test core_logs -- --ignored --nocapture`.
#![cfg(feature = "server")]
mod support;

use anyhow::{Result, ensure};
use serde_json::json;
use std::{
    io::Write,
    sync::{Arc, Mutex, PoisonError},
};
use support::{TestBot, WAIT, harness, sse};

const USER: &str = "Zebrafinch4471";
const REPLY: &str = "Quokka9012";
const SAVED: &str = "Narwhal3307";
const PARTIAL: &str = "Pangolin5520";

/// Every formatted log line, shared with the subscriber.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the isolated engine; run with the integration environment"]
async fn logs_carry_no_conversation_text() -> Result<()> {
    let captured = Captured::default();
    tracing_subscriber::fmt()
        .with_env_filter("info,robot_buddy=trace")
        .with_ansi(false)
        .with_writer(captured.clone())
        .try_init()
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let h = harness().await;
    let _shared = h.shared().await;
    let direct = format!("Log check direct {USER}");
    let tool = format!("Log check tool {USER}");
    let failed = format!("Log check failure {USER}");
    h.mock
        .script(&direct, |_| sse::text(&format!("Hello {REPLY}. Bye.")));
    h.mock.script(&tool, |r| {
        if r.spoken {
            return sse::text(&format!("Saved {REPLY}."));
        }
        if r.tool_results == 0 {
            let args = json!({ "key": "animal", "value": SAVED });
            return sse::tool("call_log", "memory_remember", &args);
        }
        sse::text("READY")
    });
    h.mock.script(&failed, |_| {
        let partial = sse::chunk(
            &json!({ "role": "assistant", "content": format!("{PARTIAL}. ") }),
            None,
        );
        sse::body(format!("{partial}data: {{not json {USER}\n\n"))
    });

    let b = TestBot::enroll(h).await?;
    let mut events = b.events().await?;
    for (request, text, expected) in [
        ("log-direct", &direct, "turn.completed"),
        ("log-tool", &tool, "turn.completed"),
        ("log-failed", &failed, "turn.failed"),
    ] {
        let turn = b.submit(request, text).await?;
        let done = events.wait_terminal(&turn, WAIT).await?;
        let kind = support::terminal(&done).map(|e| e.event.kind.clone());
        ensure!(kind.as_deref() == Some(expected), "{request}: {kind:?}");
    }
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let logs = String::from_utf8_lossy(&captured.0.lock().unwrap_or_else(PoisonError::into_inner))
        .into_owned();
    ensure!(
        logs.contains("spoken response completed"),
        "the capture sees robot_buddy lines"
    );
    for sentinel in [USER, REPLY, SAVED, PARTIAL] {
        let leaked: Vec<_> = logs.lines().filter(|l| l.contains(sentinel)).collect();
        ensure!(leaked.is_empty(), "{sentinel} logged: {leaked:#?}");
    }
    println!("core_logs: {} lines checked", logs.lines().count());
    Ok(())
}

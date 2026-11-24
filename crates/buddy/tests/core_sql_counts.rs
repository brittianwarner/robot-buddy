//! Measured `SQLite` cost of one turn, counted by the actors' own SQL wrapper
//! (`robot_buddy::db::sql_counts`) against an isolated real engine.
//!
//! Run with the integration environment of `bun run test:integration`, for example:
//! `cargo test -p robot-buddy --test core_sql_counts -- --ignored --nocapture`.
#![cfg(feature = "server")]
mod support;

use anyhow::{Context, Result, ensure};
use robot_buddy::db::{SqlCounts, sql_counts};
use serde_json::json;
use std::time::Duration;
use support::{TestBot, WAIT, harness, init_tracing, sse};

/// Lets trailing work (delivery, the drain recheck) settle before counts are read.
const SETTLE: Duration = Duration::from_millis(500);

async fn measured<F: Future<Output = Result<()>>>(work: F) -> Result<SqlCounts> {
    tokio::time::sleep(SETTLE).await;
    let before = sql_counts();
    work.await?;
    tokio::time::sleep(SETTLE).await;
    Ok(sql_counts().since(before))
}

fn report(label: &str, counts: SqlCounts) {
    println!(
        "sql-counts {label}: statements={} autocommit={} transactions={}",
        counts.statements, counts.autocommit, counts.transactions
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the isolated engine; run with the integration environment"]
async fn per_turn_statement_and_transaction_counts() -> Result<()> {
    const WARM: &str = "Count warm up turn";
    const DIRECT: &str = "Count a direct spoken turn";
    const TOOL: &str = "Count a one tool turn";
    const WAKE: &str = "Count a turn after a wake";
    init_tracing();
    let h = harness().await;
    let _alone = h.exclusive().await;
    h.mock.script(WARM, |_| sse::text("Warm reply."));
    h.mock
        .script(DIRECT, |_| sse::text("Hello there. How are you today?"));
    h.mock.script(WAKE, |_| sse::text("Awake again."));
    h.mock.script(TOOL, |r| {
        if r.spoken {
            return sse::text("Saved your color.");
        }
        if r.tool_results == 0 {
            let args = json!({"key":"color","value":"red"});
            return sse::tool("call_color", "memory_remember", &args);
        }
        sse::text("READY")
    });

    let mut bot = None;
    let cold = measured(Box::pin(async {
        let b = TestBot::enroll(h).await?;
        let (_, done) = b.turn("warm", WARM).await?;
        ensure!(support::terminal(&done).is_some(), "warm turn finished");
        bot = Some(b);
        Ok(())
    }))
    .await?;
    report("enroll+first-turn", cold);
    let b = bot.context("bot enrolled")?;

    let mut events = b.events().await?;
    let direct = measured(async {
        let turn = b.submit("direct", DIRECT).await?;
        let done = events.wait_terminal(&turn, WAIT).await?;
        ensure!(
            support::terminal(&done).is_some_and(|e| e.event.kind == "turn.completed"),
            "direct turn completed"
        );
        Ok(())
    })
    .await?;
    report("direct-turn", direct);
    // Below the counts before the claim transaction and the flush cadence (37 and 23).
    ensure!(
        direct.statements < 37 && direct.autocommit < 23,
        "direct turn SQL regressed: {direct:?}"
    );

    let tool = measured(async {
        let turn = b.submit("tool", TOOL).await?;
        let done = events.wait_terminal(&turn, WAIT).await?;
        ensure!(
            support::terminal(&done).is_some_and(|e| e.event.kind == "turn.completed"),
            "tool turn completed"
        );
        Ok(())
    })
    .await?;
    report("one-tool-turn", tool);
    // Before: 63 statements, 51 auto-commit.
    ensure!(
        tool.statements < 63 && tool.autocommit < 51,
        "tool turn SQL regressed: {tool:?}"
    );
    drop(events);

    // A restart makes every actor wake on its next request, from the same storage.
    h.restart().await?;
    let wake = measured(async {
        let (_, done) = b.turn("wake", WAKE).await?;
        ensure!(
            support::terminal(&done).is_some_and(|e| e.event.kind == "turn.completed"),
            "wake turn completed"
        );
        Ok(())
    })
    .await?;
    report("wake+direct-turn", wake);
    Ok(())
}

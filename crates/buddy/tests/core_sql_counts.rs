//! Measured `SQLite` cost of one turn, counted by the actors' own SQL wrapper
//! (`robot_buddy::db::sql_counts`) against an isolated real engine.
//!
//! Run with the integration environment of `bun run test:integration`, for example:
//! `cargo test -p robot-buddy --test core_sql_counts -- --ignored --nocapture`.
#![cfg(feature = "server")]
// The harness is shared with `actors.rs`; this binary uses only part of it.
#[allow(dead_code)]
mod support;

use anyhow::{Context, Result, ensure};
use robot_buddy::{
    db::{SqlCounts, sql_counts},
    protocol::Command,
};
use serde_json::json;
use std::{sync::atomic::Ordering, time::Duration};
use support::{Events, Harness, REPLAYS, TestBot, WAIT, harness, init_tracing, sse};

/// Lets trailing work (delivery, the drain recheck) settle before counts are read.
const SETTLE: Duration = Duration::from_millis(500);

/// What `work` cost, and how many harness `Replay` calls it made.
struct Measured {
    counts: SqlCounts,
    replays: u64,
}

impl Measured {
    /// The actors' SQL without the harness's own `Replay` calls, each costing `replay`.
    /// Event streams poll `Replay` every 250 ms without a push, so their number follows
    /// wall-clock gaps; netting them out keeps the count deterministic.
    const fn net(&self, replay: SqlCounts) -> SqlCounts {
        SqlCounts {
            statements: self
                .counts
                .statements
                .saturating_sub(replay.statements * self.replays),
            autocommit: self
                .counts
                .autocommit
                .saturating_sub(replay.autocommit * self.replays),
            transactions: self
                .counts
                .transactions
                .saturating_sub(replay.transactions * self.replays),
        }
    }
}

async fn measured<F: Future<Output = Result<()>>>(work: F) -> Result<Measured> {
    tokio::time::sleep(SETTLE).await;
    let before = sql_counts();
    let replays = REPLAYS.load(Ordering::Relaxed);
    work.await?;
    tokio::time::sleep(SETTLE).await;
    Ok(Measured {
        counts: sql_counts().since(before),
        replays: REPLAYS.load(Ordering::Relaxed) - replays,
    })
}

/// The speech service's status probe index (apps/server/src/speech/source.rs).
const STATUS_PROBE: u32 = 64;

/// Reads a finished turn's speech the way the speech service does: each chunk in order
/// until `done`, then the status probe. Returns how many reads it made.
async fn read_speech(b: &TestBot, turn: &str) -> Result<u64> {
    let mut reads = 0;
    for index in 0..STATUS_PROBE {
        let chunk = b
            .conv(Command::SpeechChunk {
                turn_id: turn.into(),
                index,
            })
            .await?;
        reads += 1;
        if chunk["state"] == "done" {
            break;
        }
        ensure!(chunk["state"] == "ready", "chunk {index}: {chunk}");
    }
    let probe = b
        .conv(Command::SpeechChunk {
            turn_id: turn.into(),
            index: STATUS_PROBE,
        })
        .await?;
    ensure!(probe["state"] == "done", "probe: {probe}");
    Ok(reads + 1)
}

/// Statements one speech read costs: its one conversation statement, plus the fresh access
/// check `on_before_connect` makes for every stateless action call, which is two bot
/// statements (the bot's own `on_before_connect` and its `CheckAccess` authorization).
const PER_READ: u64 = 1 + 2;

fn report(label: &str, counts: SqlCounts) {
    println!(
        "sql-counts {label}: statements={} autocommit={} transactions={}",
        counts.statements, counts.autocommit, counts.transactions
    );
}

/// The speech service's reads of a finished turn, then its polls of a turn that has not
/// spoken yet. Returns the still-running polled turn.
async fn speech_reads(
    h: &'static Harness,
    b: &TestBot,
    events: &mut Events,
    direct_turn: &str,
    replay: SqlCounts,
) -> Result<String> {
    const PENDING: &str = "Count pending speech polls";
    const POLLS: u64 = 8;
    // The speech service's reads of a finished turn: no transaction, one conversation
    // statement each (before: two statements and one transaction each).
    let mut reads = 0;
    let pcm = measured(async {
        reads = read_speech(b, direct_turn).await?;
        Ok(())
    })
    .await?;
    let pcm = pcm.net(replay);
    report("pcm-reads", pcm);
    println!("sql-counts pcm-reads: reads={reads}");
    ensure!(
        pcm.transactions == 0 && pcm.statements <= reads * PER_READ,
        "speech reads SQL regressed: {pcm:?} for {reads} reads"
    );

    // Polling a turn that has not spoken yet, as the speech service does while it waits.
    h.mock
        .script(PENDING, |_| sse::hang_after(sse::content("Still"), WAIT));
    let pending_turn = b.submit("pending", PENDING).await?;
    events.wait_kind(&pending_turn, "text.delta", WAIT).await?;
    let polls = measured(async {
        for _ in 0..POLLS {
            let chunk = b
                .conv(Command::SpeechChunk {
                    turn_id: pending_turn.clone(),
                    index: 0,
                })
                .await?;
            ensure!(chunk["state"] == "pending", "pending poll: {chunk}");
        }
        Ok(())
    })
    .await?;
    let polls = polls.net(replay);
    report("pcm-pending-polls", polls);
    ensure!(
        polls.transactions == 0 && polls.statements <= POLLS * PER_READ,
        "pending speech polls SQL regressed: {polls:?} for {POLLS} polls"
    );
    Ok(pending_turn)
}

const WARM: &str = "Count warm up turn";
const DIRECT: &str = "Count a direct spoken turn";
const TOOL: &str = "Count a one tool turn";
const WAKE: &str = "Count a turn after a wake";

/// The model's scripted replies for every measured turn.
fn script_turns(h: &Harness) {
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
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the isolated engine; run with the integration environment"]
async fn per_turn_statement_and_transaction_counts() -> Result<()> {
    init_tracing();
    let h = harness().await;
    let _alone = h.exclusive().await;
    script_turns(h);
    let mut bot = None;
    let cold = measured(Box::pin(async {
        let b = TestBot::enroll(h).await?;
        let (_, done) = b.turn("warm", WARM).await?;
        ensure!(support::terminal(&done).is_some(), "warm turn finished");
        bot = Some(b);
        Ok(())
    }))
    .await?;
    let b = bot.context("bot enrolled")?;
    // One harness `Replay` of an empty page, charged to the harness below.
    let replay = measured(async {
        b.conv(Command::Replay { after: i64::MAX }).await?;
        Ok(())
    })
    .await?
    .counts;
    report("harness-replay", replay);
    println!(
        "sql-counts enroll+first-turn: harness replays={}",
        cold.replays
    );
    let cold = cold.net(replay);
    report("enroll+first-turn", cold);
    // Measured at 60 statements, 31 auto-commit and 11 transactions net of harness
    // replays, including the conversation's start probe for rows past the retention window.
    ensure!(
        cold.statements <= 60 && cold.autocommit <= 31 && cold.transactions <= 11,
        "enrollment and first turn SQL regressed: {cold:?}"
    );

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
    let direct = direct.net(replay);
    report("direct-turn", direct);
    let direct_turn = events
        .log()
        .iter()
        .rev()
        .find(|e| e.event.kind == "turn.completed")
        .map(|e| e.event.turn_id.clone())
        .context("direct turn id")?;
    // Measured at 31, 11 and 6. Before the claim transaction and the flush cadence: 37
    // statements and 23 auto-commit.
    ensure!(
        direct.statements <= 31 && direct.autocommit <= 11 && direct.transactions <= 6,
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
    let tool = tool.net(replay);
    report("one-tool-turn", tool);
    // Measured at 52, 34 and 5: the context read is folded into the spoken pin (wave 5:
    // 53 and 35; before that 63 and 51).
    ensure!(
        tool.statements <= 52 && tool.autocommit <= 34 && tool.transactions <= 5,
        "tool turn SQL regressed: {tool:?}"
    );

    let pending_turn = speech_reads(h, &b, &mut events, &direct_turn, replay).await?;
    b.conv(Command::Cancel {
        turn_id: pending_turn.clone(),
    })
    .await?;
    events.wait_terminal(&pending_turn, WAIT).await?;
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
    let wake = wake.net(replay);
    report("wake+direct-turn", wake);
    // Measured at 32 statements, 14 auto-commit and 5 transactions net of harness
    // replays, including the wake's schema check, requeue, interrupted-turn scan and
    // retention probe.
    ensure!(
        wake.statements <= 32 && wake.autocommit <= 14 && wake.transactions <= 5,
        "wake and turn SQL regressed: {wake:?}"
    );
    Ok(())
}

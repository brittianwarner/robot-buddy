//! Bounded retention against an isolated real engine: old turns lose their events and
//! speech, keep their record, and a replay from the start still works and speaks nothing.
//!
//! Run with the integration environment of `bun run test:integration`, for example:
//! `cargo test -p robot-buddy --test core_retention -- --ignored --nocapture`.
#![cfg(feature = "server")]
// The harness is shared with `actors.rs`; this binary uses only part of it.
#[allow(dead_code)]
mod support;

use anyhow::{Context, Result, ensure};
use robot_buddy::protocol::{Command, OutputEvent};
use std::time::{Duration, Instant};
use support::{TestBot, harness, init_tracing, sse};

/// More than the kept window of 50 turns plus one retention pass of 8.
const TURNS: usize = 58;

async fn replay(b: &TestBot, after: i64) -> Result<Vec<OutputEvent>> {
    Ok(serde_json::from_value(
        b.conv(Command::Replay { after }).await?,
    )?)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the isolated engine; run with the integration environment"]
async fn old_turns_lose_events_and_speech_but_replay_still_works() -> Result<()> {
    const TAG: &str = "Retention check turn";
    init_tracing();
    let h = harness().await;
    let _shared = h.shared().await;
    h.mock.script(TAG, |_| sse::text("Kept short. Done."));
    let b = TestBot::enroll(h).await?;
    let mut events = b.events().await?;
    let mut turns = Vec::with_capacity(TURNS);
    for n in 0..TURNS {
        let turn = b
            .submit(&format!("retention-{n}"), &format!("{TAG} {n}"))
            .await?;
        let done = events.wait_terminal(&turn, support::WAIT).await?;
        ensure!(
            support::terminal(&done).is_some_and(|e| e.event.kind == "turn.completed"),
            "turn {n} completed"
        );
        turns.push(turn);
    }
    let oldest = turns.first().context("turns")?;

    // The pass runs on its own task after the 56th turn.
    let started = Instant::now();
    let first = loop {
        let page = replay(&b, 0).await?;
        let first = page
            .first()
            .context("replay from 0 returns kept events")?
            .clone();
        if first.turn_id != *oldest {
            break first;
        }
        ensure!(started.elapsed() < Duration::from_secs(20), "retention ran");
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    ensure!(
        first.kind == "turn.started",
        "replay starts at a turn boundary"
    );
    // The pass starts after turn 56 and keeps the 50 newest turns it sees: 56 to 58,
    // depending on how many later turns were submitted before it read the cutoff.
    let index = turns.iter().position(|turn| *turn == first.turn_id);
    ensure!(
        matches!(index, Some(6..=8)),
        "the oldest turns are pruned; replay starts at {index:?}"
    );

    // Paging from 0 reaches the newest event, and no page holds a pruned turn.
    let mut cursor = 0;
    let mut seen = 0;
    loop {
        let page = replay(&b, cursor).await?;
        let Some(last) = page.last() else { break };
        ensure!(
            page.iter().all(|e| e.turn_id != *oldest),
            "pruned turn replayed"
        );
        seen += page.len();
        cursor = last.sequence;
    }
    let newest = turns.last().context("turns")?;
    ensure!(seen > 0, "replay returned events");
    let snapshot = b.conv(Command::Snapshot).await?;
    ensure!(
        snapshot["last_sequence"].as_i64() == Some(cursor),
        "replay reached the newest event"
    );

    // A pruned turn reads done: nothing old is spoken again.
    let chunk = b
        .conv(Command::SpeechChunk {
            turn_id: oldest.clone(),
            index: 0,
        })
        .await?;
    ensure!(
        chunk["state"] == "done",
        "pruned speech reads done: {chunk}"
    );
    let kept = b
        .conv(Command::SpeechChunk {
            turn_id: newest.clone(),
            index: 0,
        })
        .await?;
    ensure!(kept["state"] == "ready", "kept speech still reads: {kept}");

    // The turn record and its answer are kept.
    let answer = b
        .conv(Command::SpokenAnswer {
            turn_id: oldest.clone(),
        })
        .await?;
    ensure!(
        answer["text"] == "Kept short. Done.",
        "pruned turn keeps its answer"
    );
    Ok(())
}

/// Turns per actor lifetime in the short-lifetime test: fewer than one retention pass of 8.
const PER_LIFETIME: usize = 5;
/// Eleven lifetimes of five turns: past the kept window, though no lifetime reached a pass.
const SHORT_TURNS: usize = 55;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the isolated engine; run with the integration environment"]
async fn retention_catches_up_across_short_actor_lifetimes() -> Result<()> {
    const TAG: &str = "Short lifetime retention turn";
    init_tracing();
    let h = harness().await;
    let _alone = h.exclusive().await;
    h.mock.script(TAG, |_| sse::text("Short. Done."));
    let b = TestBot::enroll(h).await?;
    let mut turns = Vec::with_capacity(SHORT_TURNS);
    for lifetime in 0..SHORT_TURNS / PER_LIFETIME {
        let mut events = b.events().await?;
        for n in 0..PER_LIFETIME {
            let n = lifetime * PER_LIFETIME + n;
            let turn = b
                .submit(&format!("short-{n}"), &format!("{TAG} {n}"))
                .await?;
            let done = events.wait_terminal(&turn, support::WAIT).await?;
            ensure!(
                support::terminal(&done).is_some_and(|e| e.event.kind == "turn.completed"),
                "turn {n} completed"
            );
            turns.push(turn);
        }
        drop(events);
        // A deploy: every actor stops and wakes on its next request with its storage.
        h.swap_registry(robot_buddy::registry).await?;
    }
    let oldest = turns.first().context("turns")?;
    // The wake's probe finds the rows past the window and runs the pass the in-memory
    // counter never reached.
    let started = Instant::now();
    let first = loop {
        let page = replay(&b, 0).await?;
        let first = page.first().context("kept events")?.clone();
        if first.turn_id != *oldest {
            break first;
        }
        ensure!(
            started.elapsed() < Duration::from_secs(20),
            "retention never caught up after short lifetimes"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    let index = turns.iter().position(|turn| *turn == first.turn_id);
    ensure!(
        index == Some(SHORT_TURNS - 50),
        "the 50 newest turns keep their events; replay starts at {index:?}"
    );
    let chunk = b
        .conv(Command::SpeechChunk {
            turn_id: oldest.clone(),
            index: 0,
        })
        .await?;
    ensure!(
        chunk["state"] == "done",
        "pruned speech reads done: {chunk}"
    );
    Ok(())
}

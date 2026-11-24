//! Bounded retention. Only the newest [`KEPT_TURNS`] turns keep their event log, speech
//! chunks, pinned voice and tool receipts; the turns themselves and their history search
//! rows are kept.
//!
//! A pass runs on its own task every [`PRUNE_EVERY`] finished turns and deletes in
//! transactions of a bounded number of rows, so no commit grows with history. The count
//! lives in memory, and an actor that sleeps after a few turns may never reach it, so every
//! start also probes for rows past the window ([`OVERDUE_SQL`]) and runs a pass if any remain. A cursor
//! older than the window replays from the oldest kept event. A pruned turn has no speech
//! chunks, so a speech poll for it reads `done` and nothing old is spoken again.
use super::{Arc, AtomicBool, Conversation, Ctx, Db, Ordering, P, Result, Tx, named_for};
use crate::db::BATCH_TX_TIMEOUT;
use rivetkit::ColumnValue as V;

/// Newest turns whose events and speech are kept.
pub const KEPT_TURNS: i64 = 50;
/// Finished turns between retention passes.
pub const PRUNE_EVERY: u32 = 8;

/// The oldest kept turn, and the oldest turn still queued or running. Binds
/// `KEPT_TURNS - 1`. No first value means fewer turns than the window.
pub const CUTOFF_SQL: &str = r"
    SELECT (SELECT id FROM turns ORDER BY id DESC LIMIT 1 OFFSET ?),
        (SELECT min(id) FROM turns WHERE status IN ('queued', 'running'))";
/// Each binds the cutoff: rows of turns older than it, a bounded batch at a time.
pub const PRUNE_EVENTS_SQL: &str = r"
    DELETE FROM events WHERE sequence IN (
        SELECT sequence FROM events WHERE turn_id < ? ORDER BY turn_id LIMIT 2048)";
pub const PRUNE_CHUNKS_SQL: &str = r"
    DELETE FROM speech_chunks WHERE rowid IN (
        SELECT rowid FROM speech_chunks WHERE turn_id < ? ORDER BY turn_id LIMIT 512)";
pub const PRUNE_SPEECH_SQL: &str = r"
    DELETE FROM speech_turns WHERE turn_id IN (
        SELECT turn_id FROM speech_turns WHERE turn_id < ? ORDER BY turn_id LIMIT 512)";
pub const PRUNE_TOOLS_SQL: &str = r"
    DELETE FROM tool_calls WHERE rowid IN (
        SELECT rowid FROM tool_calls WHERE turn_id < ? ORDER BY turn_id LIMIT 512)";
/// Whether any table still holds rows of turns older than the oldest kept turn. Binds
/// `KEPT_TURNS - 1`. Each probe stops at its first row of the turn ID index.
pub const OVERDUE_SQL: &str = r"
    WITH cutoff(id) AS (SELECT id FROM turns ORDER BY id DESC LIMIT 1 OFFSET ?)
    SELECT EXISTS(SELECT 1 FROM events, cutoff WHERE events.turn_id < cutoff.id)
        OR EXISTS(SELECT 1 FROM speech_chunks, cutoff WHERE speech_chunks.turn_id < cutoff.id)
        OR EXISTS(SELECT 1 FROM speech_turns, cutoff WHERE speech_turns.turn_id < cutoff.id)
        OR EXISTS(SELECT 1 FROM tool_calls, cutoff WHERE tool_calls.turn_id < cutoff.id)";
const PRUNE_SQL: [&str; 4] = [
    PRUNE_EVENTS_SQL,
    PRUNE_CHUNKS_SQL,
    PRUNE_SPEECH_SQL,
    PRUNE_TOOLS_SQL,
];

/// The turn ID below which rows are pruned: the oldest kept turn, lowered to the oldest
/// queued or running turn so an active turn is never pruned.
pub fn cutoff(row: &[V]) -> Option<String> {
    let text = |index| match row.get(index) {
        Some(V::Text(id)) => Some(id.clone()),
        _ => None,
    };
    let kept = text(0)?;
    Some(text(1).map_or(kept.clone(), |active| kept.min(active)))
}

/// One batch in one transaction. Returns the rows deleted.
async fn prune_batch(ctx: &Ctx<Conversation>) -> Result<i64> {
    let batch = |tx: Tx| async move {
        let offset = Some(vec![P::Integer(KEPT_TURNS - 1)]);
        let rows = tx.execute(CUTOFF_SQL, offset).await?.rows;
        let Some(cutoff) = rows.first().and_then(|row| cutoff(row)) else {
            return Ok(0);
        };
        let mut deleted = 0;
        for sql in PRUNE_SQL {
            let bound = Some(vec![P::Text(cutoff.clone())]);
            deleted += tx.execute(sql, bound).await?.changes;
        }
        Ok(deleted)
    };
    ctx.db()
        .transaction(batch, named_for("conversation.retention", BATCH_TX_TIMEOUT))
        .await
}

/// Single-flight guard for retention passes. Clears the flag when dropped, including on
/// early return, task cancellation, or panic.
struct PruneGuard(Arc<AtomicBool>);

impl PruneGuard {
    fn acquire(flag: &Arc<AtomicBool>) -> Option<Self> {
        flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Self(flag.clone()))
    }
}

impl Drop for PruneGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl Conversation {
    /// Counts a finished turn and starts a retention pass every [`PRUNE_EVERY`] turns.
    pub(super) fn prune_after_turn(&self, ctx: &Ctx<Self>) {
        if self.since_prune.fetch_add(1, Ordering::SeqCst) + 1 < PRUNE_EVERY {
            return;
        }
        self.since_prune.store(0, Ordering::SeqCst);
        self.start_pass(ctx, false);
    }

    /// At start: runs the pass an earlier lifetime owed, if rows past the window remain.
    pub(super) fn prune_if_overdue(&self, ctx: &Ctx<Self>) {
        self.start_pass(ctx, true);
    }

    /// Runs a retention pass on its own task, batch by batch. With `probe`, only when
    /// [`OVERDUE_SQL`] finds rows past the window. Overlapping calls skip rather than contend.
    fn start_pass(&self, ctx: &Ctx<Self>, probe: bool) {
        let Some(guard) = PruneGuard::acquire(&self.pruning) else {
            return;
        };
        let task_ctx = ctx.clone();
        ctx.register_task(async move {
            let _guard = guard;
            let _awake = task_ctx.keep_awake_region();
            if probe {
                let offset = Some(vec![P::Integer(KEPT_TURNS - 1)]);
                match task_ctx.db().query(OVERDUE_SQL, offset).await {
                    Ok(result) if overdue(&result.rows) => {}
                    Ok(_) => return,
                    Err(error) => {
                        tracing::warn!(%error, "retention probe failed");
                        return;
                    }
                }
            }
            let mut deleted = 0;
            while !task_ctx.aborted() {
                match prune_batch(&task_ctx).await {
                    Ok(0) => break,
                    Ok(rows) => deleted += rows,
                    Err(error) => {
                        // The next pass continues; nothing depends on old rows going.
                        tracing::warn!(deleted, %error, "retention pass paused");
                        return;
                    }
                }
                // Turn writes and actions run between batches.
                tokio::task::yield_now().await;
            }
            if deleted > 0 {
                tracing::debug!(deleted, "retention pass finished");
            }
        });
    }
}

/// Reads [`OVERDUE_SQL`]'s one boolean.
fn overdue(rows: &[Vec<V>]) -> bool {
    matches!(
        rows.first().and_then(|row| row.first()),
        Some(V::Integer(1))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn database(turns: usize) -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(include_str!("../conversation.sql"))
            .unwrap();
        for n in 0..turns {
            let id = format!("turn-{n:04}");
            db.execute(
                "INSERT INTO turns(id, request_id, text, status, created) VALUES (?1, ?1, 't', 'completed', 0)",
                [&id],
            )
            .unwrap();
            for kind in [
                "turn.started",
                "text.delta",
                "speech.ready",
                "turn.completed",
            ] {
                db.execute(
                    "INSERT INTO events(turn_id, kind, payload) VALUES (?, ?, '{}')",
                    [&id, kind],
                )
                .unwrap();
            }
            db.execute(
                "INSERT INTO speech_turns(turn_id, voice) VALUES (?, '{}')",
                [&id],
            )
            .unwrap();
            db.execute(
                "INSERT INTO speech_chunks(turn_id, chunk_index, text) VALUES (?, 0, 'Hi.')",
                [&id],
            )
            .unwrap();
            db.execute(
                "INSERT INTO tool_calls(turn_id, call_id, name, args, result) VALUES (?, 'c', 'n', '{}', '{}')",
                [&id],
            )
            .unwrap();
        }
        db
    }

    fn row_cutoff(db: &Connection) -> Option<String> {
        db.query_row(CUTOFF_SQL, [KEPT_TURNS - 1], |r| {
            let text = |i| -> rusqlite::Result<V> {
                Ok(r.get::<_, Option<String>>(i)?.map_or(V::Null, V::Text))
            };
            Ok(cutoff(&[text(0)?, text(1)?]))
        })
        .unwrap()
    }

    fn prune(db: &Connection) -> usize {
        let Some(cutoff) = row_cutoff(db) else {
            return 0;
        };
        PRUNE_SQL
            .iter()
            .map(|sql| db.execute(sql, [&cutoff]).unwrap())
            .sum()
    }

    fn count(db: &Connection, sql: &str) -> i64 {
        db.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn a_full_window_keeps_everything() {
        let kept = usize::try_from(KEPT_TURNS).unwrap();
        assert_eq!(row_cutoff(&database(kept - 1)), None);
        let db = database(kept);
        assert_eq!(row_cutoff(&db).as_deref(), Some("turn-0000"));
        assert_eq!(prune(&db), 0);
    }

    #[test]
    fn only_the_newest_turns_keep_events_and_speech() {
        let db = database(60);
        while prune(&db) > 0 {}
        assert_eq!(count(&db, "SELECT count(*) FROM turns"), 60);
        assert_eq!(
            count(&db, "SELECT count(DISTINCT turn_id) FROM events"),
            KEPT_TURNS
        );
        assert_eq!(count(&db, "SELECT count(*) FROM speech_chunks"), KEPT_TURNS);
        assert_eq!(count(&db, "SELECT count(*) FROM speech_turns"), KEPT_TURNS);
        assert_eq!(count(&db, "SELECT count(*) FROM tool_calls"), KEPT_TURNS);
        let oldest: String = db
            .query_row("SELECT min(turn_id) FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(oldest, "turn-0010");
    }

    #[test]
    fn an_active_turn_is_never_pruned() {
        let db = database(60);
        db.execute(
            "UPDATE turns SET status = 'running' WHERE id = 'turn-0003'",
            [],
        )
        .unwrap();
        while prune(&db) > 0 {}
        let kept: i64 = count(
            &db,
            "SELECT count(*) FROM events WHERE turn_id = 'turn-0003'",
        );
        assert_eq!(kept, 4);
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM events WHERE turn_id < 'turn-0003'"
            ),
            0
        );
    }

    #[test]
    fn batches_are_bounded() {
        let db = database(700);
        let first = prune(&db);
        // 2048 events plus 512 rows of each other table.
        assert_eq!(first, 2048 + 512 * 3);
        while prune(&db) > 0 {}
        assert_eq!(
            count(&db, "SELECT count(DISTINCT turn_id) FROM events"),
            KEPT_TURNS
        );
    }

    fn is_overdue(db: &Connection) -> bool {
        db.query_row(OVERDUE_SQL, [KEPT_TURNS - 1], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn the_start_probe_finds_rows_past_the_window_until_a_pass_clears_them() {
        let kept = usize::try_from(KEPT_TURNS).unwrap();
        assert!(!is_overdue(&database(0)));
        assert!(!is_overdue(&database(kept)));
        let db = database(kept + 3);
        assert!(is_overdue(&db));
        while prune(&db) > 0 {}
        assert!(!is_overdue(&db));
        // A pass interrupted after the events went still leaves work the probe sees.
        db.execute(
            "INSERT INTO speech_chunks(turn_id, chunk_index, text) VALUES ('turn-0000', 1, 'x')",
            [],
        )
        .unwrap();
        assert!(is_overdue(&db));
        assert!(overdue(&[vec![V::Integer(1)]]));
        assert!(!overdue(&[vec![V::Integer(0)]]));
        assert!(!overdue(&[]));
    }

    #[test]
    fn prune_guard_prevents_overlap_and_releases_on_drop_or_panic() {
        let flag = Arc::new(AtomicBool::new(false));
        let first = PruneGuard::acquire(&flag).expect("first acquires");
        assert!(flag.load(Ordering::Acquire));
        assert!(PruneGuard::acquire(&flag).is_none(), "second skips");
        drop(first);
        assert!(!flag.load(Ordering::Acquire));

        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _held = PruneGuard::acquire(&flag).expect("re-acquires after drop");
            assert!(PruneGuard::acquire(&flag).is_none());
            panic!("retention panicked");
        }));
        assert!(unwound.is_err());
        assert!(!flag.load(Ordering::Acquire));
        assert!(PruneGuard::acquire(&flag).is_some());
    }
}

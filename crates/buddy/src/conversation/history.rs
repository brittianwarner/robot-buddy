//! History search: one FTS row per turn, kept in place and replaced by a stable row ID.
//!
//! `history_search` is the version 1 index, and nothing here ever gives a turn a second
//! row, so version 1's own search reads it correctly at every step, including during a
//! rolling deploy or a rollback. `history_rows` maps each turn to its row ID, so the
//! triggers in `conversation.sql` replace a finished turn's row by key. Version 1 deleted
//! by the unindexed `turn_id` column instead, a full index scan per finished turn.
//!
//! Rows written before the map existed are mapped by a rebuild that only reads the
//! index. It runs in transactions of at most [`BATCH`] rows in row ID order, so no
//! commit grows with history. `history_backfill` holds its progress:
//!
//! - `floor` is the highest row ID in use when the rebuild started. Rows at or below it
//!   are mapped by the batches. A sentinel mapping at `floor + 1` makes every row ID the
//!   triggers allocate land above the floor, clear of rows not yet mapped.
//! - `cursor` is the last row ID mapped, stored as text. A resumed rebuild continues after
//!   it. `sweeping` is unused and stays 0.
//!
//! A turn that finishes before its row is mapped (one in flight at the upgrade) adopts
//! its row in the update trigger: the newest row with its ID, a short backward read since
//! a running turn's row is among the newest.
//!
//! Version 1 code that opens the database again recreates its own triggers. The current
//! triggers stand down while those exist, so version 1 keeps the index alone, still one row
//! per turn. The next start of the current code finds them, drops them and maps again
//! above a new floor. The triggers of the unreleased first keyed layout
//! (`turns_index_insert`, `turns_index_update`) are replaced the same way; that layout's
//! rebuild could leave a turn two rows, and each batch deletes a row whose turn already
//! has a mapped row elsewhere. The `turns` table is never rewritten.
//!
//! `tests/history_sql.rs` runs these statements on local `SQLite` in the order [`begin`]
//! and [`step`] run them.
use super::{Conversation, Ctx};
use crate::{
    auth,
    db::{BATCH_TX_TIMEOUT, Db, Tx, integer, named, named_for, string},
};
use anyhow::{Context, Result};
use rivetkit::BindParam as P;

/// Name of the finished rebuild in `schema_migrations`.
pub const MIGRATION: &str = "history_search_rowids";
/// Index rows mapped per transaction.
pub const BATCH: i64 = 256;

/// Older triggers present, rebuild finished, rebuild in progress, any turns stored.
pub const STATUS_SQL: &str = r"
    SELECT
        EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'trigger' AND name IN (
            'turns_search_insert', 'turns_search_update',
            'turns_index_insert', 'turns_index_update')),
        EXISTS(SELECT 1 FROM schema_migrations WHERE name = 'history_search_rowids'),
        EXISTS(SELECT 1 FROM history_backfill),
        EXISTS(SELECT 1 FROM turns)";
/// Older code wrote to this database: its triggers go and the mapping is checked again.
pub const RESET_SQL: &str = r"
    DROP TRIGGER IF EXISTS turns_search_insert;
    DROP TRIGGER IF EXISTS turns_search_update;
    DROP TRIGGER IF EXISTS turns_index_insert;
    DROP TRIGGER IF EXISTS turns_index_update;
    DELETE FROM schema_migrations WHERE name = 'history_search_rowids';
    DELETE FROM history_backfill;
    DELETE FROM history_rows WHERE turn_id = '';";
/// Records the floor and places the sentinel mapping just above it.
pub const START_SQL: &str = r"
    INSERT INTO history_backfill(singleton, floor, cursor, sweeping)
    SELECT 1, max(
        coalesce((SELECT max(rowid) FROM history_search), 0),
        coalesce((SELECT max(fts_rowid) FROM history_rows), 0)), '0', 0;
    INSERT INTO history_rows(fts_rowid, turn_id) SELECT floor + 1, '' FROM history_backfill;";
/// Binds the applied time.
pub const MARK_SQL: &str =
    "INSERT OR IGNORE INTO schema_migrations(name, applied) VALUES ('history_search_rowids', ?)";
pub const STATE_SQL: &str =
    "SELECT floor, cursor, sweeping FROM history_backfill WHERE singleton = 1";
/// Binds cursor, floor. The last row ID of the next batch; no row once every row is mapped.
pub const WINDOW_END_SQL: &str = r"
    SELECT rowid FROM (
        SELECT rowid FROM history_search WHERE rowid > ? AND rowid <= ?
        ORDER BY rowid LIMIT 256)
    ORDER BY rowid DESC LIMIT 1";
/// Binds cursor, end. Deletes a row of the batch whose turn has its mapped row elsewhere,
/// a second row only the first keyed layout's rebuild wrote.
pub const DEDUP_SQL: &str = r"
    DELETE FROM history_search WHERE rowid IN (
        SELECT h.rowid FROM history_search h JOIN history_rows r ON r.turn_id = h.turn_id
        WHERE h.rowid > ? AND h.rowid <= ? AND r.fts_rowid <> h.rowid
            AND EXISTS(SELECT 1 FROM history_search k
                WHERE k.rowid = r.fts_rowid AND k.turn_id = r.turn_id))";
/// Binds cursor, end. Maps each row of the batch to its turn, replacing a stale mapping.
pub const MAP_SQL: &str = r"
    INSERT OR REPLACE INTO history_rows(fts_rowid, turn_id)
    SELECT h.rowid, h.turn_id FROM history_search h
    WHERE h.rowid > ? AND h.rowid <= ?
        AND NOT EXISTS(SELECT 1 FROM history_rows r
            WHERE r.fts_rowid = h.rowid AND r.turn_id = h.turn_id)";
/// Binds the batch's last row ID.
pub const ADVANCE_SQL: &str = "UPDATE history_backfill SET cursor = ? WHERE singleton = 1";
/// Removes the progress row and the sentinel; [`MARK_SQL`] runs in the same transaction.
pub const FINISH_SQL: &str = r"
    DELETE FROM history_backfill;
    DELETE FROM history_rows WHERE turn_id = '';";

/// The search read. The index holds one row per turn, so it is version 1's read.
pub const SEARCH_SQL: &str = r"
    SELECT t.id, t.status, substr(t.text, 1, 1500), substr(t.answer, 1, 2500)
    FROM history_search h JOIN turns t ON t.id = h.turn_id
    WHERE history_search MATCH ?
    ORDER BY rank LIMIT 6";

/// The latest completed turns, oldest first: the prompt context. Served by the
/// `turns_completed` index.
pub const RECENT_TURNS_SQL: &str = r"
    SELECT text, answer FROM (
        SELECT id, substr(text, 1, 4000) AS text, substr(answer, 1, 6000) AS answer
        FROM turns WHERE status = 'completed'
        ORDER BY id DESC LIMIT 12
    ) ORDER BY id";

/// What the start transaction decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Begin {
    /// The mapping is current; nothing to do.
    Done,
    /// No turns yet: mark the rebuild finished.
    Mark,
    /// Continue the rebuild already in progress.
    Resume,
    /// Drop the older triggers, then start over.
    Reset,
    /// Start a new rebuild.
    Start,
}

impl Begin {
    /// Decides from [`STATUS_SQL`]'s flags in column order: older triggers present,
    /// rebuild finished, rebuild in progress, any turns stored.
    #[must_use]
    pub const fn decide(status: [bool; 4]) -> Self {
        let [older, finished, pending, turns] = status;
        if older {
            Self::Reset
        } else if finished {
            Self::Done
        } else if pending {
            Self::Resume
        } else if turns {
            Self::Start
        } else {
            Self::Mark
        }
    }

    /// Whether batches remain after the start transaction commits.
    #[must_use]
    pub const fn pending(self) -> bool {
        matches!(self, Self::Resume | Self::Reset | Self::Start)
    }
}

/// Starts, resumes or restarts the rebuild, or marks it done. Returns whether batches
/// remain. Runs in `create`, before any action.
pub(super) async fn begin(ctx: &Ctx<Conversation>) -> Result<bool> {
    let begin = |tx: Tx| async move {
        let rows = tx.execute(STATUS_SQL, None).await?.rows;
        let row = rows.first().context("history status missing")?;
        let flag = |index| integer(row, index).map(|value| value != 0);
        let decision = Begin::decide([flag(0)?, flag(1)?, flag(2)?, flag(3)?]);
        match decision {
            Begin::Done | Begin::Resume => {}
            Begin::Mark => {
                tx.execute(MARK_SQL, Some(vec![P::Integer(auth::now())]))
                    .await?;
            }
            Begin::Reset => {
                tx.exec(RESET_SQL).await?;
                tx.exec(START_SQL).await?;
            }
            Begin::Start => {
                tx.exec(START_SQL).await?;
            }
        }
        Ok(decision.pending())
    };
    let options = named("conversation.history_backfill_begin");
    ctx.db().transaction(begin, options).await
}

/// Maps one batch in one transaction, or finishes. Returns whether batches remain.
pub(super) async fn step(ctx: &Ctx<Conversation>) -> Result<bool> {
    let step = |tx: Tx| async move {
        let rows = tx.execute(STATE_SQL, None).await?.rows;
        let Some(row) = rows.first() else {
            return Ok(false);
        };
        let floor = integer(row, 0)?;
        let cursor: i64 = string(row, 1)?
            .parse()
            .context("invalid history rebuild cursor")?;
        let bounds = vec![P::Integer(cursor), P::Integer(floor)];
        let end = tx.execute(WINDOW_END_SQL, Some(bounds)).await?.rows;
        if let Some(end) = end.first().map(|row| integer(row, 0)).transpose()? {
            let window = || Some(vec![P::Integer(cursor), P::Integer(end)]);
            tx.execute(DEDUP_SQL, window()).await?;
            tx.execute(MAP_SQL, window()).await?;
            tx.execute(ADVANCE_SQL, Some(vec![P::Integer(end)])).await?;
            return Ok(true);
        }
        tx.exec(FINISH_SQL).await?;
        tx.execute(MARK_SQL, Some(vec![P::Integer(auth::now())]))
            .await?;
        Ok(false)
    };
    ctx.db()
        .transaction(
            step,
            named_for("conversation.history_backfill", BATCH_TX_TIMEOUT),
        )
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn older_triggers_force_a_restart() {
        assert_eq!(Begin::decide([true, true, false, true]), Begin::Reset);
        assert_eq!(Begin::decide([true, false, true, true]), Begin::Reset);
        assert_eq!(Begin::decide([false, true, false, true]), Begin::Done);
        assert_eq!(Begin::decide([false, false, true, true]), Begin::Resume);
        assert_eq!(Begin::decide([false, false, false, true]), Begin::Start);
        assert_eq!(Begin::decide([false, false, false, false]), Begin::Mark);
        assert!(!Begin::Done.pending() && !Begin::Mark.pending());
    }

    #[test]
    fn batch_size_matches_the_statement() {
        assert!(WINDOW_END_SQL.contains(&format!("LIMIT {BATCH}")));
    }
}

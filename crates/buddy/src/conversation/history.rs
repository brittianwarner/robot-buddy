//! The history search index rebuild: stable FTS row IDs, built in bounded batches.
//!
//! Version 1 inserted FTS rows with automatic row IDs and replaced a turn's row by its
//! unindexed `turn_id`, a full index scan per finished turn. The current schema keeps each
//! turn's row ID in `history_rows`, so a replacement is a keyed write.
//!
//! The rebuild runs in transactions of at most [`BATCH`] turns, so no single commit grows
//! with history. `history_backfill` holds its progress:
//!
//! - `floor` is the highest row ID in use when the rebuild started. Every rebuilt row is
//!   placed above it; rows at or below it are leftovers to sweep. A sentinel mapping at
//!   `floor + 1` keeps new row IDs above the floor while old mappings are deleted.
//! - `cursor` is the last turn ID indexed. A resumed rebuild continues after it.
//! - `sweeping` is set once every turn has a row above the floor. The sweep then deletes
//!   the leftovers [`BATCH`] at a time and writes the [`MIGRATION`] marker.
//!
//! Search reads a turn's mapped row, or its version 1 row while the turn is still unmapped,
//! so it returns each turn once at every step of the rebuild; see [`SEARCH_SQL`].
//!
//! Version 1 code that opens the database again during a rolling deploy recreates its own
//! triggers. The next start finds them, drops them and starts over above a new floor, so
//! rows written by version 1 are swept too. The `turns` table itself is never rewritten.
//!
//! `tests/history_sql.rs` runs these statements on local `SQLite` in the order the actor
//! runs them.

/// Name of the finished rebuild in `schema_migrations`.
pub const MIGRATION: &str = "history_search_rowids";
/// Turns indexed, or leftover rows swept, per transaction.
pub const BATCH: i64 = 256;

/// Version 1 triggers present, rebuild finished, rebuild in progress, any turns stored.
pub const STATUS_SQL: &str = r"
    SELECT
        EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'trigger'
            AND name IN ('turns_search_insert', 'turns_search_update')),
        EXISTS(SELECT 1 FROM schema_migrations WHERE name = 'history_search_rowids'),
        EXISTS(SELECT 1 FROM history_backfill),
        EXISTS(SELECT 1 FROM turns)";
/// Version 1 wrote to this database: its triggers go and any finished rebuild is redone.
pub const RESET_SQL: &str = r"
    DROP TRIGGER IF EXISTS turns_search_insert;
    DROP TRIGGER IF EXISTS turns_search_update;
    DELETE FROM schema_migrations WHERE name = 'history_search_rowids';
    DELETE FROM history_backfill;
    DELETE FROM history_rows WHERE turn_id = '';";
/// Records the floor and places the sentinel mapping just above it.
pub const START_SQL: &str = r"
    INSERT INTO history_backfill(singleton, floor, cursor, sweeping)
    SELECT 1, max(
        coalesce((SELECT max(rowid) FROM history_search), 0),
        coalesce((SELECT max(fts_rowid) FROM history_rows), 0)), '', 0;
    INSERT INTO history_rows(fts_rowid, turn_id) SELECT floor + 1, '' FROM history_backfill;";
/// Binds the applied time.
pub const MARK_SQL: &str =
    "INSERT OR IGNORE INTO schema_migrations(name, applied) VALUES ('history_search_rowids', ?)";
pub const STATE_SQL: &str =
    "SELECT floor, cursor, sweeping FROM history_backfill WHERE singleton = 1";
/// The last turn ID of the next batch after the cursor. No row means every turn is indexed.
pub const WINDOW_END_SQL: &str = r"
    SELECT id FROM (SELECT id FROM turns WHERE id > ? ORDER BY id LIMIT 256)
    ORDER BY id DESC LIMIT 1";
/// Binds cursor, end, floor. Drops the batch's mappings at or below the floor.
pub const UNMAP_SQL: &str = r"
    DELETE FROM history_rows WHERE fts_rowid IN (
        SELECT r.fts_rowid FROM turns t JOIN history_rows r ON r.turn_id = t.id
        WHERE t.id > ? AND t.id <= ? AND r.fts_rowid <= ?)";
/// Binds cursor, end. Maps each unmapped turn of the batch above the floor, in ID order.
pub const MAP_SQL: &str = r"
    INSERT OR IGNORE INTO history_rows(turn_id)
    SELECT id FROM turns WHERE id > ? AND id <= ? ORDER BY id";
/// Binds cursor, end. Writes the search row of each mapped turn that has none yet.
pub const INDEX_SQL: &str = r"
    INSERT INTO history_search(rowid, turn_id, user_text, assistant_text)
    SELECT r.fts_rowid, t.id, t.text, t.answer
    FROM turns t JOIN history_rows r ON r.turn_id = t.id
    WHERE t.id > ? AND t.id <= ?
        AND NOT EXISTS(SELECT 1 FROM history_search h WHERE h.rowid = r.fts_rowid)";
/// Binds the batch's last turn ID.
pub const ADVANCE_SQL: &str = "UPDATE history_backfill SET cursor = ? WHERE singleton = 1";
pub const SWEEP_START_SQL: &str = "UPDATE history_backfill SET sweeping = 1 WHERE singleton = 1";
/// Binds floor. Deletes one batch of search rows at or below the floor.
pub const SWEEP_SEARCH_SQL: &str = r"
    DELETE FROM history_search WHERE rowid IN (
        SELECT rowid FROM history_search WHERE rowid <= ? ORDER BY rowid LIMIT 256)";
/// Binds floor. Deletes one batch of mappings left at or below the floor by deleted turns.
pub const SWEEP_ROWS_SQL: &str = r"
    DELETE FROM history_rows WHERE fts_rowid IN (
        SELECT fts_rowid FROM history_rows WHERE fts_rowid <= ? ORDER BY fts_rowid LIMIT 256)";
/// Removes the progress row and the sentinel; [`MARK_SQL`] runs in the same transaction.
pub const FINISH_SQL: &str = r"
    DELETE FROM history_backfill;
    DELETE FROM history_rows WHERE turn_id = '';";

/// Reads each turn once: its mapped row, or while the rebuild runs, its version 1 row
/// below the floor if the turn is not mapped yet. Rows version 1 code wrote during a
/// rolling deploy match neither and stay hidden until the sweep deletes them.
pub const SEARCH_SQL: &str = r"
    SELECT t.id, t.status, substr(t.text, 1, 1500), substr(t.answer, 1, 2500)
    FROM history_search h JOIN turns t ON t.id = h.turn_id
    WHERE history_search MATCH ?
        AND (EXISTS(SELECT 1 FROM history_rows r
                WHERE r.turn_id = h.turn_id AND r.fts_rowid = h.rowid)
            OR (h.rowid <= (SELECT floor FROM history_backfill WHERE singleton = 1)
                AND NOT EXISTS(SELECT 1 FROM history_rows r WHERE r.turn_id = h.turn_id)))
    ORDER BY rank LIMIT 6";

/// What the start transaction decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Begin {
    /// The index is current; nothing to do.
    Done,
    /// No turns yet: mark the rebuild finished.
    Mark,
    /// Continue the rebuild already in progress.
    Resume,
    /// Drop the version 1 triggers, then start over.
    Reset,
    /// Start a new rebuild.
    Start,
}

impl Begin {
    /// Decides from [`STATUS_SQL`]'s flags in column order: version 1 triggers present,
    /// rebuild finished, rebuild in progress, any turns stored.
    #[must_use]
    pub const fn decide(status: [bool; 4]) -> Self {
        let [version_one, finished, pending, turns] = status;
        if version_one {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_one_triggers_force_a_restart() {
        assert_eq!(Begin::decide([true, true, false, true]), Begin::Reset);
        assert_eq!(Begin::decide([true, false, true, true]), Begin::Reset);
        assert_eq!(Begin::decide([false, true, false, true]), Begin::Done);
        assert_eq!(Begin::decide([false, false, true, true]), Begin::Resume);
        assert_eq!(Begin::decide([false, false, false, true]), Begin::Start);
        assert_eq!(Begin::decide([false, false, false, false]), Begin::Mark);
        assert!(!Begin::Done.pending() && !Begin::Mark.pending());
    }

    #[test]
    fn batch_size_matches_the_statements() {
        let limit = format!("LIMIT {BATCH}");
        for sql in [WINDOW_END_SQL, SWEEP_SEARCH_SQL, SWEEP_ROWS_SQL] {
            assert!(sql.contains(&limit), "{sql}");
        }
    }
}

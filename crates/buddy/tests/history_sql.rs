//! The real conversation schema on local `SQLite`: query plans for history reads and the
//! batched mapping of a version 1 history search index in `conversation/history.rs`.
//!
//! `begin` and `step` run the actor's statements in the actor's order, one transaction
//! each, as `history::begin` and `history::step` do. Both search reads, the current one and
//! version 1's, must return exactly the turns a fresh database returns at every boundary:
//! before the upgrade, after each batch, after version 1 code runs again, and after.
use robot_buddy::conversation::history::{self, Begin};
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const SCHEMA: &str = include_str!("../src/conversation.sql");
const SCHEMA_V1: &str = include_str!("fixtures/conversation-v1.sql");
/// Version 1's search read, which version 1 code keeps using during a rolling deploy.
const SEARCH_V1: &str = "SELECT t.id FROM history_search h JOIN turns t ON t.id=h.turn_id \
    WHERE history_search MATCH ? ORDER BY rank LIMIT 6";
/// The rebuild the first keyed layout ran: every turn written again at a new row ID.
/// Measured for comparison only.
const REWRITE_REBUILD: &str = "DELETE FROM history_search; DELETE FROM history_rows; \
    INSERT INTO history_rows(turn_id) SELECT id FROM turns ORDER BY id; \
    INSERT INTO history_search(rowid,turn_id,user_text,assistant_text) \
    SELECT r.fts_rowid,t.id,t.text,t.answer FROM history_rows r JOIN turns t ON t.id=r.turn_id";
/// Rivet's per-transaction page limit.
const RIVET_TRANSACTION_PAGES: i64 = 32_768;
/// Turns for the page measurement.
const LARGE: usize = 5_000;
/// Turns for the interruption scenarios: four batches, the last one partial.
const SMALL: usize = 3 * 256 + 100;
/// Queries with one hit, many hits and hits on rewritten answers.
const QUERIES: &[&str] = &[
    "marker123",
    "marker867",
    "topic7",
    "question",
    "rewritten",
    "queued",
    "late",
    "fresh",
    "old",
];

/// The create path: the startup schema, then the history step.
fn begin(db: &mut Connection) -> Begin {
    db.execute_batch(SCHEMA).expect("schema");
    begin_backfill(db)
}

/// The create path's history step alone: one transaction, as `history::begin`.
fn begin_backfill(db: &mut Connection) -> Begin {
    let tx = db.transaction().expect("begin");
    let flags = tx
        .query_row(history::STATUS_SQL, [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .expect("status");
    let decision = Begin::decide([flags.0, flags.1, flags.2, flags.3]);
    match decision {
        Begin::Done | Begin::Resume => {}
        Begin::Mark => {
            tx.execute(history::MARK_SQL, [0]).expect("mark");
        }
        Begin::Reset => {
            tx.execute_batch(history::RESET_SQL).expect("reset");
            tx.execute_batch(history::START_SQL).expect("start");
        }
        Begin::Start => tx.execute_batch(history::START_SQL).expect("start"),
    }
    tx.commit().expect("commit");
    decision
}

/// One batch, as `history::step`. `commit: false` models a process lost before the
/// commit: the transaction rolls back. Returns whether batches remain.
fn step(db: &mut Connection, commit: bool) -> bool {
    let tx = db.transaction().expect("begin");
    let state: Option<(i64, String)> = tx
        .query_row(history::STATE_SQL, [], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()
        .expect("state");
    let more = if let Some((floor, cursor)) = state {
        let cursor: i64 = cursor.parse().expect("cursor");
        let end: Option<i64> = tx
            .query_row(history::WINDOW_END_SQL, [cursor, floor], |r| r.get(0))
            .optional()
            .expect("window");
        if let Some(end) = end {
            tx.execute(history::DEDUP_SQL, [cursor, end])
                .expect("dedup");
            tx.execute(history::MAP_SQL, [cursor, end]).expect("map");
            tx.execute(history::ADVANCE_SQL, [end]).expect("advance");
            true
        } else {
            tx.execute_batch(history::FINISH_SQL).expect("finish");
            tx.execute(history::MARK_SQL, [0]).expect("mark");
            false
        }
    } else {
        false
    };
    if commit {
        tx.commit().expect("commit");
    }
    more
}

/// Create, then every batch: what one uninterrupted actor start does.
fn apply(db: &mut Connection) -> usize {
    begin(db);
    let mut batches = 0;
    while step(db, true) {
        batches += 1;
    }
    batches
}

fn insert_turns(db: &Connection, count: usize) {
    let mut insert = db
        .prepare(
            "INSERT INTO turns(id,request_id,text,status,created) VALUES(?1,?1,?2,'running',?3)",
        )
        .expect("prepare");
    let mut finish = db
        .prepare("UPDATE turns SET status='completed',answer=?2 WHERE id=?1")
        .expect("prepare");
    for i in 0..count {
        let id = format!("turn-{i:06}");
        insert
            .execute(params![
                id,
                format!("question {i} about topic{}", i % 50),
                i
            ])
            .expect("insert");
        finish
            .execute(params![id, format!("answer {i} mentions marker{i}")])
            .expect("finish");
    }
}

/// `count` completed turns, a queued one, and every 97th answer rewritten, so version 1
/// row IDs have gaps and are out of turn order. Inserted completed: version 1's update
/// trigger scans per row.
fn seed(db: &Connection, count: usize) {
    db.execute_batch("BEGIN").expect("begin");
    let mut insert = db
        .prepare(
            "INSERT INTO turns(id,request_id,text,status,answer,created) \
             VALUES(?1,?1,?2,'completed',?3,?4)",
        )
        .expect("prepare");
    for i in 0..count {
        let id = format!("turn-{i:06}");
        let text = format!("question {i} about topic{}", i % 50);
        insert
            .execute(params![
                id,
                text,
                format!("answer {i} mentions marker{i}"),
                i
            ])
            .expect("insert");
    }
    db.execute(
        "INSERT INTO turns(id,request_id,text,created) VALUES('turn-q','turn-q','queued ask',1)",
        [],
    )
    .expect("queued");
    for i in (0..count).step_by(97) {
        db.execute(
            "UPDATE turns SET answer=?2 WHERE id=?1",
            params![format!("turn-{i:06}"), format!("rewritten answer {i}")],
        )
        .expect("rewrite");
    }
    db.execute_batch("COMMIT").expect("commit");
}

/// A database built by the current code alone, from the same turns.
fn fresh_copy_of(db: &Connection) -> Connection {
    let mut fresh = Connection::open_in_memory().expect("open");
    assert_eq!(begin(&mut fresh), Begin::Mark);
    let mut stmt = db
        .prepare("SELECT id,request_id,text,status,answer,created FROM turns ORDER BY id")
        .expect("turns");
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, i64>(5)?,
            ))
        })
        .expect("rows")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect");
    for row in rows {
        fresh
            .execute(
                "INSERT INTO turns(id,request_id,text,status,answer,created) \
                 VALUES(?1,?2,?3,?4,?5,?6)",
                params![row.0, row.1, row.2, row.3, row.4, row.5],
            )
            .expect("insert");
    }
    fresh
}

fn plan(db: &Connection, sql: &str) -> String {
    let mut stmt = db
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .expect("plan");
    let count = stmt.parameter_count();
    let values = vec![rusqlite::types::Value::Null; count];
    let rows = stmt
        .query_map(rusqlite::params_from_iter(values), |r| {
            r.get::<_, String>(3)
        })
        .expect("rows")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect");
    rows.join(" | ")
}

fn run_search(db: &Connection, sql: &str, query: &str) -> Vec<String> {
    let mut stmt = db.prepare(sql).expect("search");
    stmt.query_map([query], |r| r.get(0))
        .expect("rows")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect")
}

/// The actor's search read.
fn search(db: &Connection, query: &str) -> Vec<String> {
    run_search(db, history::SEARCH_SQL, query)
}

/// Every hit of `query` by `reader` without the ranking limit, sorted. A turn returned
/// twice stays twice, so a duplicate fails the comparison.
fn all_hits(db: &Connection, reader: &str, query: &str) -> Vec<String> {
    let unlimited = reader.replace("LIMIT 6", "");
    let mut hits = run_search(db, &unlimited, query);
    hits.sort();
    hits
}

/// The rebuild's committed cursor: the last row ID mapped.
fn cursor(db: &Connection) -> i64 {
    db.query_row("SELECT cursor FROM history_backfill", [], |r| {
        r.get::<_, String>(0)
    })
    .expect("cursor")
    .parse()
    .expect("integer cursor")
}

fn count(db: &Connection, sql: &str) -> i64 {
    db.query_row(sql, [], |r| r.get(0)).expect("count")
}

/// Search rows, mappings and progress, for comparing two database states.
fn index_state(db: &Connection) -> Vec<String> {
    let mut out = Vec::new();
    for sql in [
        "SELECT rowid||':'||turn_id||':'||user_text||':'||assistant_text FROM history_search \
         ORDER BY rowid",
        "SELECT fts_rowid||':'||turn_id FROM history_rows ORDER BY fts_rowid",
        "SELECT floor||':'||cursor||':'||sweeping FROM history_backfill",
    ] {
        let mut stmt = db.prepare(sql).expect("state");
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .expect("rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect");
        out.extend(rows);
    }
    out
}

/// One search row per turn with the turn's current text, each at its mapping, and the
/// rebuild finished.
fn assert_rebuilt(db: &Connection) {
    let turns = count(db, "SELECT count(*) FROM turns");
    assert_eq!(count(db, "SELECT count(*) FROM history_search"), turns);
    assert_eq!(count(db, "SELECT count(*) FROM history_rows"), turns);
    let matched = count(
        db,
        "SELECT count(*) FROM history_rows r JOIN history_search h ON h.rowid=r.fts_rowid \
         AND h.turn_id=r.turn_id JOIN turns t ON t.id=r.turn_id \
         AND h.user_text=t.text AND h.assistant_text=t.answer",
    );
    assert_eq!(matched, turns);
    assert_eq!(count(db, "SELECT count(*) FROM history_backfill"), 0);
    assert_eq!(
        count(
            db,
            "SELECT count(*) FROM schema_migrations WHERE name='history_search_rowids'"
        ),
        1
    );
    let older = count(
        db,
        "SELECT count(*) FROM sqlite_master WHERE type='trigger' \
         AND (name LIKE 'turns_search_%' OR name LIKE 'turns_index_%')",
    );
    assert_eq!(older, 0);
}

/// The index as `turn_id|user_text|assistant_text`, sorted.
fn index_content(db: &Connection) -> Vec<String> {
    let mut stmt = db
        .prepare(
            "SELECT turn_id||'|'||user_text||'|'||assistant_text FROM history_search \
             ORDER BY 1",
        )
        .expect("content");
    stmt.query_map([], |r| r.get(0))
        .expect("rows")
        .collect::<Result<_, _>>()
        .expect("collect")
}

/// Exact results on both readers. The index holds exactly a fresh database's rows, one per
/// turn with its current text, so every query matches the same turns on both reads. Every
/// hit of each selective query, by the current read and by version 1's, equals a fresh
/// database's, and the ranked top six of the two reads are the same turns, none twice.
fn assert_exact(db: &Connection, fresh: &Connection, at: &str) {
    assert_eq!(index_content(db), index_content(fresh), "{at}");
    for query in QUERIES {
        // Matches every turn: covered by the content check, too slow to list each time.
        if *query != "question" {
            let expected = all_hits(fresh, history::SEARCH_SQL, query);
            let current = all_hits(db, history::SEARCH_SQL, query);
            assert_eq!(current, expected, "{at}: {query}");
            assert_eq!(all_hits(db, SEARCH_V1, query), expected, "{at}: v1 {query}");
        }
        let ranked = search(db, query);
        let mut unique = ranked.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), ranked.len(), "{at}: {query} {ranked:?}");
        assert_eq!(run_search(db, SEARCH_V1, query), ranked, "{at}: {query}");
    }
}

/// A file-backed WAL database with automatic checkpoints off, so the WAL holds exactly
/// the pages one transaction wrote.
struct WalFile {
    dir: PathBuf,
}

impl WalFile {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "history-sql-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        Self { dir }
    }

    fn path(&self, file: &str) -> PathBuf {
        self.dir.join(file)
    }
}

impl Drop for WalFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn open_wal(path: &Path) -> Connection {
    let db = Connection::open(path).expect("open");
    db.execute_batch(
        "PRAGMA page_size=4096; PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;",
    )
    .expect("wal");
    db
}

/// Checkpoints and empties the WAL.
fn reset_wal(db: &Connection) {
    db.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .expect("checkpoint");
}

/// Pages written since the last `reset_wal`: one WAL frame per page per commit.
fn wal_pages(db: &Connection) -> i64 {
    db.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |r| r.get(1))
        .expect("checkpoint")
}

/// A closed version 1 database with `turns` turns; copy it per scenario.
fn version_one_fixture(dir: &WalFile, turns: usize) -> PathBuf {
    let path = dir.path(&format!("v1-{turns}.db"));
    let db = open_wal(&path);
    db.execute_batch(SCHEMA_V1).expect("v1 schema");
    seed(&db, turns);
    drop(db);
    path
}

fn copy_of(dir: &WalFile, fixture: &Path, name: &str) -> Connection {
    let path = dir.path(name);
    std::fs::copy(fixture, &path).expect("copy");
    open_wal(&path)
}

/// Runs `sql` on both databases, keeping the fresh one a mirror of the turns.
fn on_both(db: &Connection, fresh: &Connection, sql: &str) {
    db.execute_batch(sql).expect("write");
    fresh.execute_batch(sql).expect("mirror");
}

const FINISH_QUEUED: &str =
    "UPDATE turns SET status='completed',answer='late answer' WHERE id='turn-q'";
const INSERT_NEW: &str =
    "INSERT INTO turns(id,request_id,text,created) VALUES('turn-new','turn-new','fresh ask',2)";
const INSERT_OLD: &str =
    "INSERT INTO turns(id,request_id,text,created) VALUES('turn-old','turn-old','old ask',3)";
const FINISH_OLD: &str =
    "UPDATE turns SET status='completed',answer='old reply' WHERE id='turn-old'";

#[test]
fn recent_turns_use_the_completed_index() {
    let mut db = Connection::open_in_memory().expect("open");
    apply(&mut db);
    insert_turns(&db, 50);
    let plan = plan(&db, history::RECENT_TURNS_SQL);
    println!("recent turns: {plan}");
    assert!(plan.contains("turns_completed"), "{plan}");
    assert!(!plan.contains("SCAN turns"), "{plan}");
}

#[test]
fn terminal_answer_replaces_its_search_row_by_key() {
    let mut db = Connection::open_in_memory().expect("open");
    apply(&mut db);
    insert_turns(&db, 50);
    // The trigger's lookup, its check that the mapping is the turn's own row, and the
    // rowid access its REPLACE makes.
    let lookup = plan(
        &db,
        "SELECT fts_rowid FROM history_rows WHERE turn_id='turn-000007'",
    );
    let own_row = plan(
        &db,
        "SELECT 1 FROM history_rows r JOIN history_search h ON h.rowid=r.fts_rowid \
         WHERE r.turn_id='turn-000007' AND h.turn_id='turn-000007'",
    );
    let keyed = plan(
        &db,
        "SELECT 1 FROM history_search WHERE rowid=\
         (SELECT fts_rowid FROM history_rows WHERE turn_id='turn-000007')",
    );
    let old_delete = plan(
        &db,
        "DELETE FROM history_search WHERE turn_id='turn-000007'",
    );
    println!(
        "row lookup: {lookup}\nown row: {own_row}\nkeyed row: {keyed}\n\
         version 1 delete: {old_delete}"
    );
    assert!(lookup.contains("USING"), "{lookup}");
    assert!(lookup.contains("history_rows"), "{lookup}");
    // FTS5 plans a rowid equality as `INDEX 0:=`; version 1 scanned every row.
    assert!(own_row.contains("VIRTUAL TABLE INDEX 0:="), "{own_row}");
    assert!(!own_row.contains("SCAN r"), "{own_row}");
    assert!(keyed.contains("VIRTUAL TABLE INDEX 0:="), "{keyed}");
    assert!(!old_delete.contains("INDEX 0:="), "{old_delete}");
    // One row per turn, and a finished answer replaces it.
    assert_eq!(count(&db, "SELECT count(*) FROM history_search"), 50);
    db.execute(
        "UPDATE turns SET answer='replaced wording' WHERE id='turn-000007'",
        [],
    )
    .expect("update");
    assert_eq!(count(&db, "SELECT count(*) FROM history_search"), 50);
    assert_eq!(search(&db, "marker7"), Vec::<String>::new());
    assert_eq!(search(&db, "replaced"), vec!["turn-000007".to_owned()]);
}

#[test]
fn rebuild_statements_read_by_key_and_range() {
    let mut db = Connection::open_in_memory().expect("open");
    db.execute_batch(SCHEMA_V1).expect("v1 schema");
    seed(&db, 300);
    begin(&mut db);
    for sql in [
        history::WINDOW_END_SQL,
        history::DEDUP_SQL,
        history::MAP_SQL,
        history::SEARCH_SQL,
    ] {
        let plan = plan(&db, sql);
        println!(
            "{}: {plan}",
            sql.split_whitespace().take(6).collect::<Vec<_>>().join(" ")
        );
        // No statement scans a whole table; batches read a rowid range of the index.
        assert!(!plan.contains("SCAN turns"), "{plan}");
        assert!(!plan.contains("SCAN history_rows"), "{plan}");
        assert!(!plan.contains("SCAN r "), "{plan}");
        assert!(
            !plan
                .split(" | ")
                .any(|line| line.ends_with("VIRTUAL TABLE INDEX 0:")),
            "{plan}"
        );
    }
}

#[test]
fn version_one_history_migrates_without_loss() {
    let mut db = Connection::open_in_memory().expect("open");
    db.execute_batch(SCHEMA_V1).expect("v1 schema");
    insert_turns(&db, 300);
    db.execute(
        "INSERT INTO turns(id,request_id,text,created) VALUES('turn-q','turn-q','queued ask',1)",
        [],
    )
    .expect("queued");
    let turns = |db: &Connection| -> Vec<(String, String, String)> {
        let mut stmt = db
            .prepare("SELECT id,text,answer FROM turns ORDER BY id")
            .expect("prepare");
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .expect("rows")
            .collect::<Result<_, _>>()
            .expect("collect")
    };
    let before = turns(&db);
    // The startup DDL adds the mapping table and never touches the index.
    db.execute_batch(SCHEMA).expect("schema");
    let rows_before = index_state(&db);
    assert!(apply(&mut db) > 0);
    // Applying again is a no-op: the schema is idempotent and the marker holds.
    assert_eq!(apply(&mut db), 0);
    assert_eq!(before, turns(&db));
    // The index itself is untouched: only the mapping was added.
    let rows_after = index_state(&db);
    assert_eq!(rows_before[..301], rows_after[..301]);
    assert_rebuilt(&db);
    assert_eq!(search(&db, "marker123"), vec!["turn-000123".to_owned()]);
    assert_eq!(search(&db, "queued"), vec!["turn-q".to_owned()]);
    db.execute_batch(FINISH_QUEUED).expect("finish queued");
    db.execute_batch(INSERT_NEW).expect("insert");
    assert_eq!(count(&db, "SELECT count(*) FROM history_search"), 302);
    assert_eq!(search(&db, "late"), vec!["turn-q".to_owned()]);
    assert_eq!(search(&db, "fresh"), vec!["turn-new".to_owned()]);
}

#[test]
fn an_empty_database_marks_the_rebuild_done() {
    let mut db = Connection::open_in_memory().expect("open");
    assert_eq!(begin(&mut db), Begin::Mark);
    assert!(!step(&mut db, true));
    assert_eq!(begin(&mut db), Begin::Done);
    insert_turns(&db, 3);
    assert_rebuilt(&db);
}

/// 5,000 turns: bounded batches that write only the mapping, exact search on both readers
/// before, around writes between batches and after, including the turn that was queued at
/// the upgrade finishing before its row is mapped.
#[test]
fn five_thousand_turns_rebuild_in_bounded_batches() {
    let dir = WalFile::new("bounded");
    let fixture = version_one_fixture(&dir, LARGE);

    // The first keyed layout's one-shot rewrite, for scale.
    let mut rewrite = copy_of(&dir, &fixture, "rewrite.db");
    rewrite.execute_batch(SCHEMA).expect("schema");
    reset_wal(&rewrite);
    let tx = rewrite.transaction().expect("begin");
    tx.execute_batch(REWRITE_REBUILD).expect("rebuild");
    tx.commit().expect("commit");
    let rewrite_pages = wal_pages(&rewrite);
    drop(rewrite);

    let mut db = copy_of(&dir, &fixture, "batched.db");
    let fresh = fresh_copy_of(&db);
    assert_exact(&db, &fresh, "version 1");
    // Startup DDL is measured apart from the rebuild: it runs on every start, and its
    // index builds read all of history once, on the first start after the upgrade.
    reset_wal(&db);
    db.execute_batch(SCHEMA).expect("schema");
    let ddl_pages = wal_pages(&db);
    reset_wal(&db);
    assert_eq!(begin_backfill(&mut db), Begin::Reset);
    let begin_pages = wal_pages(&db);
    assert_exact(&db, &fresh, "start");
    let (mut batches, mut largest) = (0_usize, begin_pages);
    let (mut spent, mut slowest) = (Duration::ZERO, Duration::ZERO);
    loop {
        reset_wal(&db);
        let started = Instant::now();
        let more = step(&mut db, true);
        let took = started.elapsed();
        (spent, slowest) = (spent + took, slowest.max(took));
        largest = largest.max(wal_pages(&db));
        batches += 1;
        if batches == 5 {
            // Turns keep arriving and finishing between batches. The small scenarios
            // below check every batch boundary; this one checks around the writes.
            assert_exact(&db, &fresh, "before writes");
            on_both(&db, &fresh, FINISH_QUEUED);
            on_both(&db, &fresh, INSERT_NEW);
            assert_exact(&db, &fresh, "after writes");
        }
        if !more {
            break;
        }
    }
    // A later start runs the same DDL as a no-op.
    reset_wal(&db);
    db.execute_batch(SCHEMA).expect("schema again");
    let ddl_again_pages = wal_pages(&db);
    println!(
        "{LARGE} turns: startup DDL {ddl_pages} pages (next start {ddl_again_pages}); \
         rebuild start {begin_pages} pages; {batches} batches, largest {largest} pages, \
         {} ms in all, slowest {} ms; rewriting every row {rewrite_pages} pages",
        spent.as_millis(),
        slowest.as_millis()
    );
    assert!(ddl_pages * 20 < RIVET_TRANSACTION_PAGES, "{ddl_pages}");
    assert_eq!(
        ddl_again_pages, 0,
        "startup DDL is not a no-op once applied"
    );
    assert_rebuilt(&db);
    // One batch per 256 index rows at the start (the turns and the queued one), then the
    // finishing step. Rows written during the rebuild sit above the floor.
    let rows = LARGE + 1;
    assert_eq!(batches, rows.div_ceil(256) + 1, "{batches}");
    assert!(largest * 20 < RIVET_TRANSACTION_PAGES, "{largest}");
    // Rewriting the index grows with history; a mapping batch does not, and is smaller.
    assert!(largest * 4 < rewrite_pages, "{largest} vs {rewrite_pages}");
}

/// A process lost inside any batch rolls that batch back; the next start resumes from the
/// committed cursor and ends identical to an uninterrupted rebuild. Replaying a committed
/// batch changes nothing.
#[test]
fn rebuild_resumes_after_interruption_at_any_batch() {
    let dir = WalFile::new("resume");
    let fixture = version_one_fixture(&dir, SMALL);
    let mut reference = copy_of(&dir, &fixture, "reference.db");
    let total = apply(&mut reference);
    assert_eq!(total, (SMALL + 1).div_ceil(256), "every batch boundary");
    let expected = index_state(&reference);
    let fresh = fresh_copy_of(&reference);
    for lost in 0..=total {
        let mut db = copy_of(&dir, &fixture, &format!("lost-{lost}.db"));
        assert_eq!(begin(&mut db), Begin::Reset);
        let mut previous = 0;
        for _ in 0..lost {
            previous = cursor(&db);
            assert!(step(&mut db, true));
        }
        // Replaying the last committed batch, as after a commit whose reply was lost,
        // changes nothing.
        if lost > 0 {
            let before = index_state(&db);
            db.execute(
                "UPDATE history_backfill SET cursor=?1",
                [previous.to_string()],
            )
            .expect("rewind");
            assert!(step(&mut db, true));
            assert_eq!(index_state(&db), before, "replay after batch {lost}");
        }
        // The next batch is lost before its commit.
        step(&mut db, false);
        assert_exact(&db, &fresh, &format!("lost {lost}"));
        drop(db);
        let mut db = open_wal(&dir.path(&format!("lost-{lost}.db")));
        assert_eq!(begin(&mut db), Begin::Resume, "batch {lost}");
        while step(&mut db, true) {}
        assert_rebuilt(&db);
        assert_eq!(index_state(&db), expected, "lost batch {lost}");
    }
}

/// Version 1 code opening the database before, during and after the rebuild, as in a
/// rolling deploy or a rollback: its writes succeed, both readers stay exact at every
/// boundary, and the next start of the current code ends clean.
#[test]
fn version_one_code_during_a_rolling_deploy() {
    let dir = WalFile::new("rolling");
    let fixture = version_one_fixture(&dir, SMALL);
    for rollback_after in [0, 1, 3] {
        let at = |what: &str| format!("rollback after {rollback_after}: {what}");
        let mut db = copy_of(&dir, &fixture, &format!("rolling-{rollback_after}.db"));
        let fresh = fresh_copy_of(&db);
        assert_eq!(begin(&mut db), Begin::Reset);
        for batch in 0..rollback_after {
            assert!(step(&mut db, true));
            assert_exact(&db, &fresh, &at(&format!("batch {batch}")));
        }
        // Version 1 starts on the half-mapped database, writes a turn and finishes the
        // turn that was queued, which moves its row.
        db.execute_batch(SCHEMA_V1).expect("v1 schema");
        assert_exact(&db, &fresh, &at("v1 start"));
        on_both(&db, &fresh, INSERT_OLD);
        on_both(&db, &fresh, FINISH_QUEUED);
        assert_exact(&db, &fresh, &at("v1 writes"));

        // The current code again: it restarts the mapping above a new floor.
        assert_eq!(begin(&mut db), Begin::Reset);
        assert_exact(&db, &fresh, &at("restart"));
        // A turn version 1 created finishes before its row is mapped.
        on_both(&db, &fresh, FINISH_OLD);
        assert_exact(&db, &fresh, &at("adopted"));
        while step(&mut db, true) {
            assert_exact(&db, &fresh, &at("batch"));
        }
        assert_rebuilt(&db);
        assert_exact(&db, &fresh, &at("finished"));

        // Version 1 again after the rebuild finished, then the current code.
        db.execute_batch(SCHEMA_V1).expect("v1 schema");
        on_both(&db, &fresh, INSERT_NEW);
        assert_exact(&db, &fresh, &at("v1 after finish"));
        assert_eq!(begin(&mut db), Begin::Reset);
        while step(&mut db, true) {}
        assert_rebuilt(&db);
        assert_exact(&db, &fresh, &at("final"));
    }
}

/// The current triggers stand down while version 1's exist: a turn version 1 inserts and
/// finishes gets exactly one row, version 1's.
#[test]
fn current_triggers_stand_down_beside_version_one() {
    let mut db = Connection::open_in_memory().expect("open");
    assert_eq!(begin(&mut db), Begin::Mark);
    insert_turns(&db, 5);
    db.execute_batch(SCHEMA_V1).expect("v1 schema");
    db.execute_batch(INSERT_OLD).expect("v1 insert");
    db.execute_batch(FINISH_OLD).expect("v1 finish");
    assert_eq!(count(&db, "SELECT count(*) FROM history_search"), 6);
    assert_eq!(
        run_search(&db, SEARCH_V1, "old"),
        vec!["turn-old".to_owned()]
    );
    assert_eq!(
        count(
            &db,
            "SELECT count(*) FROM history_rows WHERE turn_id='turn-old'"
        ),
        0
    );
}

/// The unreleased first keyed layout could leave a turn a version 1 row below its floor
/// and a mapped row above it. Its triggers force a restart, and the batches delete each
/// second row.
#[test]
fn second_rows_from_the_first_keyed_layout_are_removed() {
    let mut db = Connection::open_in_memory().expect("open");
    db.execute_batch(SCHEMA_V1).expect("v1 schema");
    seed(&db, 600);
    db.execute_batch(SCHEMA).expect("schema");
    // That layout's state after one batch: a floor, a sentinel, the first 256 turns
    // mapped above it and written again there, its triggers, and a turn ID cursor.
    db.execute_batch(
        "DROP TRIGGER turns_search_insert; DROP TRIGGER turns_search_update; \
         CREATE TRIGGER turns_index_insert AFTER INSERT ON turns BEGIN SELECT 1; END; \
         INSERT INTO history_backfill VALUES(1,(SELECT max(rowid) FROM history_search),\
             'turn-000255',0); \
         INSERT INTO history_rows(fts_rowid,turn_id) SELECT floor+1,'' FROM history_backfill; \
         INSERT INTO history_rows(turn_id) SELECT id FROM turns WHERE id<='turn-000255' ORDER BY id; \
         INSERT INTO history_search(rowid,turn_id,user_text,assistant_text) \
             SELECT r.fts_rowid,t.id,t.text,t.answer FROM history_rows r JOIN turns t \
             ON t.id=r.turn_id;",
    )
    .expect("first keyed layout");
    assert_eq!(count(&db, "SELECT count(*) FROM history_search"), 601 + 256);
    assert_eq!(begin_backfill(&mut db), Begin::Reset);
    while step(&mut db, true) {}
    assert_rebuilt(&db);
    let fresh = fresh_copy_of(&db);
    assert_exact(&db, &fresh, "first keyed layout");
}

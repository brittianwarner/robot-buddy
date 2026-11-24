//! The real conversation schema on local `SQLite`: query plans for history reads and the
//! batched version 1 to current history search rebuild in `conversation/history.rs`.
//!
//! `begin` and `step` run the actor's statements in the actor's order, one transaction
//! each, as `begin_history_backfill` and `history_backfill_step` in `conversation.rs` do.
use robot_buddy::conversation::history::{self, Begin};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};

const SCHEMA: &str = include_str!("../src/conversation.sql");
const SCHEMA_V1: &str = include_str!("fixtures/conversation-v1.sql");
/// Copy of the prompt-context read in `conversation/harness.rs`.
const RECENT_TURNS: &str = "SELECT text,answer FROM (SELECT id,substr(text,1,4000) AS text,\
    substr(answer,1,6000) AS answer FROM turns WHERE status='completed' \
    ORDER BY id DESC LIMIT 12) ORDER BY id";
/// Version 1's search read, which version 1 code keeps using during a rolling deploy.
const SEARCH_V1: &str = "SELECT t.id FROM history_search h JOIN turns t ON t.id=h.turn_id \
    WHERE history_search MATCH ? ORDER BY rank LIMIT 6";
/// The unbatched rebuild this replaced, measured for comparison only.
const ONE_SHOT_REBUILD: &str = "DELETE FROM history_search; DELETE FROM history_rows; \
    INSERT INTO history_rows(turn_id) SELECT id FROM turns ORDER BY id; \
    INSERT INTO history_search(rowid,turn_id,user_text,assistant_text) \
    SELECT r.fts_rowid,t.id,t.text,t.answer FROM history_rows r JOIN turns t ON t.id=r.turn_id";
/// Rivet's per-transaction page limit.
const RIVET_TRANSACTION_PAGES: i64 = 32_768;
const LARGE: usize = 5_000;
/// Queries with one hit, many hits and hits on rewritten answers.
const QUERIES: &[&str] = &[
    "marker123",
    "marker4999",
    "topic7",
    "question",
    "rewritten",
    "queued",
    "late",
    "fresh",
];

/// The create path: the startup schema, then the history step.
fn begin(db: &mut Connection) -> Begin {
    db.execute_batch(SCHEMA).expect("schema");
    begin_backfill(db)
}

/// The create path's history step alone: one transaction, as `begin_history_backfill`.
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

/// One batch, as `history_backfill_step`. `commit: false` models a process lost before the
/// commit: the transaction rolls back. Returns whether batches remain.
fn step(db: &mut Connection, commit: bool) -> bool {
    let tx = db.transaction().expect("begin");
    let state: Option<(i64, String, i64)> = tx
        .query_row(history::STATE_SQL, [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .optional()
        .expect("state");
    let more = match state {
        None => false,
        Some((floor, cursor, 0)) => {
            let end: Option<String> = tx
                .query_row(history::WINDOW_END_SQL, [&cursor], |r| r.get(0))
                .optional()
                .expect("window");
            if let Some(end) = end {
                tx.execute(history::UNMAP_SQL, params![cursor, end, floor])
                    .expect("unmap");
                tx.execute(history::MAP_SQL, params![cursor, end])
                    .expect("map");
                tx.execute(history::INDEX_SQL, params![cursor, end])
                    .expect("index");
                tx.execute(history::ADVANCE_SQL, [&end]).expect("advance");
            } else {
                tx.execute(history::SWEEP_START_SQL, []).expect("sweep");
            }
            true
        }
        Some((floor, _, _)) => {
            let swept = tx
                .execute(history::SWEEP_SEARCH_SQL, [floor])
                .expect("sweep search");
            let orphans = tx
                .execute(history::SWEEP_ROWS_SQL, [floor])
                .expect("sweep rows");
            if swept + orphans == 0 {
                tx.execute_batch(history::FINISH_SQL).expect("finish");
                tx.execute(history::MARK_SQL, [0]).expect("mark");
                false
            } else {
                true
            }
        }
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
/// row IDs have gaps. Inserted completed: version 1's update trigger scans per row.
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

/// A database built by the current schema alone, from the same turns.
fn fresh_copy_of(db: &Connection) -> Connection {
    let fresh = Connection::open_in_memory().expect("open");
    fresh.execute_batch(SCHEMA).expect("schema");
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

/// Every hit of `query` the actor's filter admits, without the ranking limit.
fn all_hits(db: &Connection, query: &str) -> Vec<String> {
    let unlimited = history::SEARCH_SQL.replace("LIMIT 6", "");
    let mut hits = run_search(db, &unlimited, query);
    hits.sort();
    hits
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

/// One search row per turn, each at its mapping, and the rebuild finished.
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
    let v1 = count(
        db,
        "SELECT count(*) FROM sqlite_master WHERE type='trigger' AND name LIKE 'turns_search_%'",
    );
    assert_eq!(v1, 0);
}

/// Same search results as a database built fresh from the same turns: ranked lists after
/// the rebuild, admitted hit sets during it.
fn assert_searches_match(db: &Connection, fresh: &Connection, ranked: bool) {
    for query in QUERIES {
        assert_eq!(all_hits(db, query), all_hits(fresh, query), "{query}");
        if ranked {
            assert_eq!(search(db, query), search(fresh, query), "{query}");
        }
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

/// A closed version 1 database with `LARGE` turns; copy it per scenario.
fn version_one_fixture(dir: &WalFile) -> PathBuf {
    let path = dir.path("v1.db");
    let db = open_wal(&path);
    db.execute_batch(SCHEMA_V1).expect("v1 schema");
    seed(&db, LARGE);
    drop(db);
    path
}

fn copy_of(dir: &WalFile, fixture: &Path, name: &str) -> Connection {
    let path = dir.path(name);
    std::fs::copy(fixture, &path).expect("copy");
    open_wal(&path)
}

#[test]
fn recent_turns_use_the_completed_index() {
    let mut db = Connection::open_in_memory().expect("open");
    apply(&mut db);
    insert_turns(&db, 50);
    let plan = plan(&db, RECENT_TURNS);
    println!("recent turns: {plan}");
    assert!(plan.contains("turns_completed"), "{plan}");
    assert!(!plan.contains("SCAN turns"), "{plan}");
}

#[test]
fn terminal_answer_replaces_its_search_row_by_key() {
    let mut db = Connection::open_in_memory().expect("open");
    apply(&mut db);
    insert_turns(&db, 50);
    // The trigger's lookup, and the rowid access its REPLACE makes.
    let lookup = plan(
        &db,
        "SELECT fts_rowid FROM history_rows WHERE turn_id='turn-000007'",
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
    println!("row lookup: {lookup}\nkeyed row: {keyed}\nversion 1 delete: {old_delete}");
    assert!(lookup.contains("USING"), "{lookup}");
    assert!(lookup.contains("history_rows"), "{lookup}");
    // FTS5 plans a rowid equality as `INDEX 0:=`; version 1 scanned every row.
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
        history::UNMAP_SQL,
        history::MAP_SQL,
        history::INDEX_SQL,
        history::SWEEP_SEARCH_SQL,
        history::SWEEP_ROWS_SQL,
        history::SEARCH_SQL,
    ] {
        let plan = plan(&db, sql);
        println!(
            "{}: {plan}",
            sql.split_whitespace().take(6).collect::<Vec<_>>().join(" ")
        );
        // No statement scans a whole table; the FTS sweep reads a rowid range.
        assert!(!plan.contains("SCAN turns"), "{plan}");
        assert!(!plan.contains("SCAN history_rows"), "{plan}");
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
    assert!(apply(&mut db) > 0);
    // Applying again is a no-op: the schema is idempotent and the marker holds.
    assert_eq!(apply(&mut db), 0);
    assert_eq!(before, turns(&db));
    assert_rebuilt(&db);
    assert_eq!(search(&db, "marker123"), vec!["turn-000123".to_owned()]);
    assert_eq!(search(&db, "queued"), vec!["turn-q".to_owned()]);
    db.execute(
        "UPDATE turns SET status='completed',answer='late answer' WHERE id='turn-q'",
        [],
    )
    .expect("finish queued");
    insert_turns_after(&db);
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

/// 5,000 turns: bounded batches, search identical to a fresh build at every batch
/// boundary, and writes between batches kept.
#[test]
fn five_thousand_turns_rebuild_in_bounded_batches() {
    let dir = WalFile::new("bounded");
    let fixture = version_one_fixture(&dir);

    // The replaced one-shot rebuild, for scale.
    let mut one_shot = copy_of(&dir, &fixture, "one-shot.db");
    one_shot.execute_batch(SCHEMA).expect("schema");
    reset_wal(&one_shot);
    let tx = one_shot.transaction().expect("begin");
    tx.execute_batch(ONE_SHOT_REBUILD).expect("rebuild");
    tx.commit().expect("commit");
    let one_shot_pages = wal_pages(&one_shot);
    drop(one_shot);

    let mut db = copy_of(&dir, &fixture, "batched.db");
    // Startup DDL is measured apart from the rebuild: it runs on every start, and its
    // index builds read all of history once, on the first start after the upgrade.
    reset_wal(&db);
    db.execute_batch(SCHEMA).expect("schema");
    let ddl_pages = wal_pages(&db);
    reset_wal(&db);
    assert_eq!(begin_backfill(&mut db), Begin::Reset);
    let begin_pages = wal_pages(&db);
    let mut fresh = fresh_copy_of(&db);
    assert_searches_match(&db, &fresh, false);
    let (mut batches, mut largest) = (0_usize, begin_pages);
    loop {
        reset_wal(&db);
        let more = step(&mut db, true);
        largest = largest.max(wal_pages(&db));
        batches += 1;
        if batches == 5 {
            // Turns keep arriving and finishing between batches.
            for conn in [&db, &fresh] {
                conn.execute(
                    "UPDATE turns SET status='completed',answer='late answer' WHERE id='turn-q'",
                    [],
                )
                .expect("finish queued");
                insert_turns_after(conn);
            }
        }
        assert_searches_match(&db, &fresh, false);
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
         rebuild start {begin_pages} pages; {batches} batches, largest {largest} pages; \
         one-shot rebuild {one_shot_pages} pages"
    );
    assert!(ddl_pages * 20 < RIVET_TRANSACTION_PAGES, "{ddl_pages}");
    assert_eq!(
        ddl_again_pages, 0,
        "startup DDL is not a no-op once applied"
    );
    assert_rebuilt(&db);
    assert_searches_match(&db, &fresh, true);
    // Index batches plus sweep batches, each bounded by BATCH rows.
    let per_phase = LARGE.div_ceil(256);
    assert!(
        batches >= 2 * per_phase && batches <= 2 * per_phase + 4,
        "{batches}"
    );
    assert!(largest * 20 < RIVET_TRANSACTION_PAGES, "{largest}");
    // The one-shot rebuild grows with history; a batch does not.
    assert!(largest < one_shot_pages, "{largest} vs {one_shot_pages}");
    fresh = fresh_copy_of(&db);
    assert_searches_match(&db, &fresh, true);
}

/// A process lost inside any batch rolls that batch back; the next start resumes from the
/// committed cursor and ends identical to an uninterrupted rebuild. Replaying a committed
/// batch changes nothing.
#[test]
fn rebuild_resumes_after_interruption_at_any_batch() {
    let dir = WalFile::new("resume");
    let fixture = version_one_fixture(&dir);
    let mut reference = copy_of(&dir, &fixture, "reference.db");
    let total = apply(&mut reference);
    let expected = index_state(&reference);
    for lost in 0..=total {
        let mut db = copy_of(&dir, &fixture, &format!("lost-{lost}.db"));
        assert_eq!(begin(&mut db), Begin::Reset);
        for _ in 0..lost {
            assert!(step(&mut db, true));
        }
        // Replaying the last committed index batch is a no-op.
        let rewind: Option<String> = db
            .query_row(
                "SELECT cursor FROM history_backfill WHERE sweeping=0 AND cursor<>''",
                [],
                |r| r.get(0),
            )
            .optional()
            .expect("cursor");
        if let Some(cursor) = rewind {
            let before = index_state(&db);
            let previous: String = db
                .query_row(
                    "SELECT coalesce((SELECT id FROM turns WHERE id<?1 ORDER BY id DESC \
                     LIMIT 1 OFFSET 255),'')",
                    [&cursor],
                    |r| r.get(0),
                )
                .expect("previous");
            db.execute("UPDATE history_backfill SET cursor=?1", [&previous])
                .expect("rewind");
            assert!(step(&mut db, true));
            assert_eq!(index_state(&db), before, "replay after batch {lost}");
        }
        // The next batch is lost before its commit.
        step(&mut db, false);
        drop(db);
        let mut db = open_wal(&dir.path(&format!("lost-{lost}.db")));
        let resumed = begin(&mut db);
        let mut more = resumed == Begin::Resume;
        assert!(more || lost == total, "batch {lost}: {resumed:?}");
        while more {
            more = step(&mut db, true);
        }
        assert_rebuilt(&db);
        assert_eq!(index_state(&db), expected, "lost batch {lost}");
    }
}

/// Version 1 code opening the database mid-rebuild and after it, as in a rolling deploy:
/// its writes succeed, and the next start restarts the rebuild and ends clean.
#[test]
fn version_one_code_during_a_rolling_deploy() {
    let dir = WalFile::new("rolling");
    let fixture = version_one_fixture(&dir);
    let mut db = copy_of(&dir, &fixture, "rolling.db");
    begin(&mut db);
    for _ in 0..6 {
        assert!(step(&mut db, true));
    }
    // Version 1 starts on the half-rebuilt database and writes two turns.
    db.execute_batch(SCHEMA_V1).expect("v1 schema");
    db.execute(
        "INSERT INTO turns(id,request_id,text,created) VALUES('turn-old','turn-old','old ask',3)",
        [],
    )
    .expect("v1 insert");
    db.execute(
        "UPDATE turns SET status='completed',answer='late answer' WHERE id='turn-q'",
        [],
    )
    .expect("v1 finish");
    // Version 1's own search still answers; mapped turns can appear twice.
    assert!(!run_search(&db, SEARCH_V1, "late").is_empty());
    assert!(!run_search(&db, SEARCH_V1, "marker10").is_empty());
    // The current read still returns each turn once.
    assert_eq!(all_hits(&db, "late"), vec!["turn-q".to_owned()]);
    assert_eq!(all_hits(&db, "old"), vec!["turn-old".to_owned()]);

    assert_eq!(begin(&mut db), Begin::Reset);
    while step(&mut db, true) {}
    assert_rebuilt(&db);
    let fresh = fresh_copy_of(&db);
    assert_searches_match(&db, &fresh, true);

    // Version 1 again after the rebuild finished, then the current code.
    db.execute_batch(SCHEMA_V1).expect("v1 schema");
    insert_turns_after(&db);
    assert_eq!(all_hits(&db, "fresh"), vec!["turn-new".to_owned()]);
    assert_eq!(begin(&mut db), Begin::Reset);
    while step(&mut db, true) {}
    assert_rebuilt(&db);
    let fresh = fresh_copy_of(&db);
    assert_searches_match(&db, &fresh, true);
}

fn insert_turns_after(db: &Connection) {
    db.execute(
        "INSERT INTO turns(id,request_id,text,created) VALUES('turn-new','turn-new','fresh ask',2)",
        [],
    )
    .expect("insert");
}

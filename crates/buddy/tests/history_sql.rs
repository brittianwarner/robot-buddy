//! The real conversation schema on local `SQLite`: query plans for history reads and the
//! version 1 to version 2 history search migration.
use rusqlite::{Connection, params};

const SCHEMA: &str = include_str!("../src/conversation.sql");
const SCHEMA_V1: &str = include_str!("fixtures/conversation-v1.sql");
const REBUILD: &str = include_str!("../src/conversation/history-rowids.sql");
/// Copy of the prompt-context read in `conversation/harness.rs`.
const RECENT_TURNS: &str = "SELECT text,answer FROM (SELECT id,substr(text,1,4000) AS text,\
    substr(answer,1,6000) AS answer FROM turns WHERE status='completed' \
    ORDER BY id DESC LIMIT 12) ORDER BY id";
/// Copy of the history search read in `conversation.rs`.
const SEARCH: &str = "SELECT t.id FROM history_search h JOIN turns t ON t.id=h.turn_id \
    WHERE history_search MATCH ? ORDER BY rank LIMIT 6";

/// The create path: schema, then the one-time rebuild guarded by its marker.
fn apply(db: &mut Connection) {
    db.execute_batch(SCHEMA).expect("schema");
    let tx = db.transaction().expect("begin");
    let done: bool = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM schema_migrations WHERE name='history_search_rowids')",
            [],
            |r| r.get(0),
        )
        .expect("marker");
    if !done {
        tx.execute_batch(REBUILD).expect("rebuild");
        tx.execute(
            "INSERT INTO schema_migrations(name,applied) VALUES('history_search_rowids',0)",
            [],
        )
        .expect("mark");
    }
    tx.commit().expect("commit");
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

fn plan(db: &Connection, sql: &str) -> String {
    let mut stmt = db
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .expect("plan");
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(3))
        .expect("rows")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect");
    rows.join(" | ")
}

fn search(db: &Connection, query: &str) -> Vec<String> {
    let mut stmt = db.prepare(SEARCH).expect("search");
    stmt.query_map([query], |r| r.get(0))
        .expect("rows")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect")
}

fn count(db: &Connection, sql: &str) -> i64 {
    db.query_row(sql, [], |r| r.get(0)).expect("count")
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
    // The trigger's statements, planned on their own.
    let lookup = plan(
        &db,
        "SELECT fts_rowid FROM history_rows WHERE turn_id='turn-000007'",
    );
    let delete = plan(
        &db,
        "DELETE FROM history_search WHERE rowid=\
         (SELECT fts_rowid FROM history_rows WHERE turn_id='turn-000007')",
    );
    let old_delete = plan(
        &db,
        "DELETE FROM history_search WHERE turn_id='turn-000007'",
    );
    println!("row lookup: {lookup}\nkeyed delete: {delete}\nversion 1 delete: {old_delete}");
    assert!(lookup.contains("USING"), "{lookup}");
    assert!(lookup.contains("history_rows"), "{lookup}");
    // FTS5 plans a rowid equality as `INDEX 0:=`; version 1 scanned every row.
    assert!(delete.contains("VIRTUAL TABLE INDEX 0:="), "{delete}");
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
fn version_one_history_migrates_without_loss() {
    let mut db = Connection::open_in_memory().expect("open");
    db.execute_batch(SCHEMA_V1).expect("v1 schema");
    insert_turns(&db, 300);
    db.execute(
        "INSERT INTO turns(id,request_id,text,created) VALUES('turn-q','turn-q','queued ask',1)",
        [],
    )
    .expect("queued");
    let before: Vec<(String, String, String)> = {
        let mut stmt = db
            .prepare("SELECT id,text,answer FROM turns ORDER BY id")
            .expect("prepare");
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .expect("rows")
            .collect::<Result<_, _>>()
            .expect("collect")
    };
    apply(&mut db);
    // Applying again is a no-op: the schema is idempotent and the marker holds.
    apply(&mut db);
    let after: Vec<(String, String, String)> = {
        let mut stmt = db
            .prepare("SELECT id,text,answer FROM turns ORDER BY id")
            .expect("prepare");
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .expect("rows")
            .collect::<Result<_, _>>()
            .expect("collect")
    };
    assert_eq!(before, after);
    assert_eq!(count(&db, "SELECT count(*) FROM history_search"), 301);
    assert_eq!(count(&db, "SELECT count(*) FROM history_rows"), 301);
    assert_eq!(
        count(
            &db,
            "SELECT count(*) FROM history_rows r JOIN history_search h ON h.rowid=r.fts_rowid \
             AND h.turn_id=r.turn_id"
        ),
        301
    );
    assert_eq!(search(&db, "marker123"), vec!["turn-000123".to_owned()]);
    assert_eq!(search(&db, "queued"), vec!["turn-q".to_owned()]);
    // The old triggers are gone and the new ones keep one row per turn.
    let triggers = count(
        &db,
        "SELECT count(*) FROM sqlite_master WHERE type='trigger' AND name LIKE 'turns_search_%'",
    );
    assert_eq!(triggers, 0);
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

fn insert_turns_after(db: &Connection) {
    db.execute(
        "INSERT INTO turns(id,request_id,text,created) VALUES('turn-new','turn-new','fresh ask',2)",
        [],
    )
    .expect("insert");
}

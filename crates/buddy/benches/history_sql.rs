//! History SQL at 10,000 turns: the version 1 schema against the current one.
//! Run: `cargo bench -p robot-buddy --bench history_sql -- --warm-up-time 1 --measurement-time 3`
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use rusqlite::{Connection, params};
use std::hint::black_box;

const SCHEMA: &str = include_str!("../src/conversation.sql");
const SCHEMA_V1: &str = include_str!("../tests/fixtures/conversation-v1.sql");
/// Copy of the prompt-context read in `conversation/harness.rs`.
const RECENT_TURNS: &str = "SELECT text,answer FROM (SELECT id,substr(text,1,4000) AS text,\
    substr(answer,1,6000) AS answer FROM turns WHERE status='completed' \
    ORDER BY id DESC LIMIT 12) ORDER BY id";
const TURNS: usize = 10_000;

fn database(current: bool) -> Connection {
    let db = Connection::open_in_memory().expect("open");
    // A fresh current database needs no rebuild: its triggers map every turn.
    db.execute_batch(if current { SCHEMA } else { SCHEMA_V1 })
        .expect("schema");
    db.execute_batch("BEGIN").expect("begin");
    for i in 0..TURNS {
        let id = format!("turn-{i:06}");
        db.execute(
            "INSERT INTO turns(id,request_id,text,status,created) VALUES(?1,?1,?2,'running',?3)",
            params![id, format!("question {i} about topic{}", i % 50), i],
        )
        .expect("insert");
        db.execute(
            "UPDATE turns SET status='completed',answer=?2 WHERE id=?1",
            params![
                id,
                format!("answer {i} with a few more words of reply text")
            ],
        )
        .expect("finish");
    }
    db.execute_batch("COMMIT").expect("commit");
    db
}

fn recent(db: &Connection) -> usize {
    let mut stmt = db.prepare_cached(RECENT_TURNS).expect("prepare");
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(1))
        .expect("query");
    rows.count()
}

/// The terminal write of one turn: its answer replaces the turn's search row.
fn finish(db: &Connection, i: u64) {
    let mut stmt = db
        .prepare_cached("UPDATE turns SET answer=?2 WHERE id=?1")
        .expect("prepare");
    let id = format!("turn-{:06}", i % TURNS as u64);
    stmt.execute(params![id, format!("revised answer {i}")])
        .expect("update");
}

fn bench(c: &mut Criterion) {
    let mut group = c.benchmark_group("history_sql_10k");
    for (name, current) in [("v1", false), ("current", true)] {
        let db = database(current);
        group.bench_function(BenchmarkId::new("recent_12", name), |b| {
            b.iter(|| black_box(recent(&db)));
        });
        let mut i = 0u64;
        group.bench_function(BenchmarkId::new("finish_turn", name), |b| {
            b.iter(|| {
                i += 7919;
                finish(&db, black_box(i));
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);

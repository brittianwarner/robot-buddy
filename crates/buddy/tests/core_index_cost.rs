//! Measured cost of building `turns_completed` on a history that predates it: the pages
//! the build writes, against the 32,768-page transaction limit, and how long it takes.
//!
//! `cargo test --release -p robot-buddy --test core_index_cost -- --ignored --nocapture`
use rusqlite::Connection;
use std::time::Instant;

const TURNS_SQL: &str = "CREATE TABLE turns(id TEXT PRIMARY KEY,request_id TEXT NOT NULL UNIQUE,\
    text TEXT NOT NULL,status TEXT NOT NULL DEFAULT 'queued',answer TEXT NOT NULL DEFAULT '',\
    created INTEGER NOT NULL); CREATE INDEX turns_status ON turns(status,created);";
const INDEX_SQL: &str = "CREATE INDEX IF NOT EXISTS turns_completed ON turns(status,id DESC)";
/// The most pages one Rivet `SQLite` transaction may write.
const PAGE_LIMIT: i64 = 32_768;

fn pages(db: &Connection) -> i64 {
    db.query_row("PRAGMA page_count", [], |r| r.get(0)).unwrap()
}

/// Builds `turns` completed turns with time-ordered IDs, a short request and answer.
fn history(turns: usize) -> Connection {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch("PRAGMA page_size = 4096;").unwrap();
    db.execute_batch(TURNS_SQL).unwrap();
    let tx = db.unchecked_transaction().unwrap();
    {
        let mut insert = tx
            .prepare("INSERT INTO turns VALUES (?, ?, ?, 'completed', ?, ?)")
            .unwrap();
        for n in 0..turns {
            let id = format!("0190{n:012x}-7000-8000-0000-000000000000");
            let text = format!("What is the weather like today, question {n}?");
            let answer = format!("It is sunny and mild. Answer number {n}, kept short.");
            insert
                .execute(rusqlite::params![id, id, text, answer, n])
                .unwrap();
        }
    }
    tx.commit().unwrap();
    db
}

#[test]
#[ignore = "measurement; run explicitly"]
fn turns_completed_build_cost() {
    println!("index-cost turns pages_written seconds share_of_limit");
    for turns in [5_000, 50_000, 100_000, 500_000, 1_000_000] {
        let db = history(turns);
        let before = pages(&db);
        let started = Instant::now();
        db.execute_batch(INDEX_SQL).unwrap();
        let seconds = started.elapsed().as_secs_f64();
        let written = pages(&db) - before;
        let share = f64::from(u32::try_from(written).unwrap())
            / f64::from(u32::try_from(PAGE_LIMIT).unwrap());
        println!("index-cost {turns} {written} {seconds:.3} {share:.3}");
        assert!(written < PAGE_LIMIT, "{turns} turns exceed one transaction");
    }
}

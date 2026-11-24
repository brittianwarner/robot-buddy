//! Measured costs on local `SQLite`: building `turns_completed` on a history that predates
//! it (pages written against the 32,768-page transaction limit, and time), the history
//! search read, and the Memory search read.
//!
//! `cargo test --release -p robot-buddy --test core_index_cost -- --ignored --nocapture`
use rusqlite::Connection;
#[cfg(feature = "server")]
use std::time::Duration;
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

/// Median time of `runs` executions of `sql` bound to each of `queries`, read to the end.
#[cfg(feature = "server")]
fn median_read(db: &Connection, sql: &str, queries: &[String], runs: usize) -> Duration {
    let mut stmt = db.prepare(sql).unwrap();
    let mut times = Vec::with_capacity(runs * queries.len());
    for _ in 0..runs {
        for query in queries {
            let started = Instant::now();
            let bound = vec![query; stmt.parameter_count()];
            let mut rows = stmt.query(rusqlite::params_from_iter(bound)).unwrap();
            while rows.next().unwrap().is_some() {}
            times.push(started.elapsed());
        }
    }
    times.sort();
    times[times.len() / 2]
}

/// The actor's history search on a database the current schema built, at several sizes.
/// Rare and common terms, so both a short and a long candidate list are ranked.
#[cfg(feature = "server")]
#[test]
#[ignore = "measurement; run explicitly"]
fn history_search_latency() {
    use robot_buddy::conversation::history::SEARCH_SQL;
    let queries: Vec<String> = [
        "\"marker123\"",
        "\"topic7\"",
        "\"question\"",
        "\"sunny\" OR \"mild\"",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    println!("history-search turns median_us");
    for turns in [1_000, 5_000, 20_000] {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(include_str!("../src/conversation.sql"))
            .unwrap();
        let tx = db.unchecked_transaction().unwrap();
        {
            let mut insert = tx
                .prepare(
                    "INSERT INTO turns(id,request_id,text,status,created) \
                     VALUES(?1,?1,?2,'running',?3)",
                )
                .unwrap();
            let mut finish = tx
                .prepare("UPDATE turns SET status='completed',answer=?2 WHERE id=?1")
                .unwrap();
            for n in 0..turns {
                let id = format!("turn-{n:06}");
                let text = format!("question {n} about topic{}", n % 50);
                insert.execute(rusqlite::params![id, text, n]).unwrap();
                let answer = format!("It is sunny and mild. Answer {n} mentions marker{n}.");
                finish.execute(rusqlite::params![id, answer]).unwrap();
            }
        }
        tx.commit().unwrap();
        let median = median_read(&db, SEARCH_SQL, &queries, 50);
        println!("history-search {turns} {}", median.as_micros());
    }
}

/// The two reads Memory search made before it became one statement, measured against it.
#[cfg(feature = "server")]
const MEMORY_MATCHES_BEFORE: &str =
    "SELECT key,value FROM facts_search WHERE facts_search MATCH ? ORDER BY rank LIMIT 6";
#[cfg(feature = "server")]
const MEMORY_RECENT_BEFORE: &str = "SELECT key,value FROM facts ORDER BY updated DESC LIMIT 2";

/// Memory search at the 500-fact cap with a 16-term query, the largest the actor accepts:
/// the two statements it replaced, without the `facts_updated` index, against the one
/// statement with it. Each statement is a round trip to the actor's database.
#[cfg(feature = "server")]
#[test]
#[ignore = "measurement; run explicitly"]
fn memory_search_cost() {
    use robot_buddy::memory::{MAX_FACTS, SCHEMA, SEARCH};
    let terms: Vec<String> = (0..16).map(|n| format!("\"word{n}\"")).collect();
    let query = vec![terms.join(" OR ")];
    let build = |schema: &str| {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(schema).unwrap();
        let tx = db.unchecked_transaction().unwrap();
        for n in 0..MAX_FACTS {
            let value = format!(
                "fact {n} says word{} and word{} with some longer text {}",
                n % 16,
                (n * 7) % 31,
                "padding ".repeat(20)
            );
            tx.execute(
                "INSERT INTO facts(key,value,updated) VALUES(?1,?2,?3)",
                rusqlite::params![format!("key-{n}"), value, n],
            )
            .unwrap();
        }
        tx.commit().unwrap();
        db
    };
    let unindexed = SCHEMA.replace(
        "CREATE INDEX IF NOT EXISTS facts_updated ON facts(updated); ",
        "",
    );
    assert_ne!(unindexed, SCHEMA);
    let before = build(&unindexed);
    let matches = median_read(&before, MEMORY_MATCHES_BEFORE, &query, 200);
    let recent = median_read(&before, MEMORY_RECENT_BEFORE, &query, 200);
    let after = build(SCHEMA);
    let one = median_read(&after, SEARCH, &query, 200);
    println!(
        "memory-search facts={MAX_FACTS} terms=16 before: 2 statements {}us + {}us; \
         after: 1 statement {}us",
        matches.as_micros(),
        recent.as_micros(),
        one.as_micros()
    );
}

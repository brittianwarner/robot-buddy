//! Bot-scoped durable facts, independently addressable from conversation history.
use crate::{
    auth,
    db::{Db, bot_key, string},
    protocol::{Call, Command, Connection},
};
use anyhow::{Result, bail, ensure};
use async_trait::async_trait;
use rivetkit::TypedClientExt;
use rivetkit::{BindParam as P, action, prelude::*};
use serde_json::{Value, json};
use std::sync::Arc;
pub struct Memory {
    signing_key: auth::Secret<Vec<u8>>,
}
/// Facts per bot. Updating an existing key is always allowed; new keys beyond this fail.
pub const MAX_FACTS: i64 = 500;
const KEY_MAX: usize = 128;
const VALUE_MAX: usize = 2048;
/// Matches the Conversation `Submit` text cap: the harness searches with the user's text.
const QUERY_MAX: usize = 8192;
const QUERY_TERMS: usize = 16;
const TERM_MAX: usize = 128;
/// Upsert that refuses a new key once the table holds `MAX_FACTS` rows. One statement, so
/// concurrent writes cannot both pass the count check.
const REMEMBER: &str = "INSERT INTO facts(key,value,updated) SELECT ?1,?2,?3 \
    WHERE (SELECT count(*) FROM facts)<?4 OR EXISTS(SELECT 1 FROM facts WHERE key=?1) \
    ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated=excluded.updated RETURNING key";
/// The Memory schema, idempotent and run on every start. `facts_updated` serves the
/// newest-first reads; an older build ignores it.
pub const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS facts(key TEXT PRIMARY KEY,value TEXT NOT NULL,updated INTEGER NOT NULL); \
    CREATE INDEX IF NOT EXISTS facts_updated ON facts(updated); \
    CREATE VIRTUAL TABLE IF NOT EXISTS facts_search USING fts5(key,value); \
    CREATE TRIGGER IF NOT EXISTS facts_insert AFTER INSERT ON facts BEGIN INSERT INTO facts_search(key,value) VALUES(new.key,new.value); END; \
    CREATE TRIGGER IF NOT EXISTS facts_delete AFTER DELETE ON facts BEGIN DELETE FROM facts_search WHERE key=old.key; END; \
    CREATE TRIGGER IF NOT EXISTS facts_update AFTER UPDATE ON facts BEGIN DELETE FROM facts_search WHERE key=old.key; INSERT INTO facts_search(key,value) VALUES(new.key,new.value); END;";
/// The 100 newest facts.
pub const SNAPSHOT: &str = "SELECT key,value FROM facts ORDER BY updated DESC LIMIT 100";
/// One read: up to six best matches for the bound FTS query, then the two newest facts,
/// which keep standing preferences in context without lexical overlap. A fact in both
/// appears twice; the caller keeps its first row.
pub const SEARCH: &str = "SELECT key,value FROM (\
    SELECT * FROM (SELECT 0 AS part,rank AS n,key,value FROM facts_search \
        WHERE facts_search MATCH ?1 ORDER BY rank LIMIT 6) \
    UNION ALL \
    SELECT * FROM (SELECT 1,-updated,key,value FROM facts ORDER BY updated DESC LIMIT 2)) \
    ORDER BY part,n";
/// [`SEARCH`] for a query with no terms: the two newest facts.
pub const RECENT: &str = "SELECT key,value FROM facts ORDER BY updated DESC LIMIT 2";
fn valid_fact(key: &str, value: &str) -> bool {
    !key.is_empty() && key.len() <= KEY_MAX && !value.is_empty() && value.len() <= VALUE_MAX
}
/// FTS5 query: at most 16 quoted terms, each truncated to a bounded number of bytes.
fn search_terms(query: &str) -> String {
    query
        .split_whitespace()
        .take(QUERY_TERMS)
        .map(|w| {
            let mut end = w.len().min(TERM_MAX);
            while !w.is_char_boundary(end) {
                end -= 1;
            }
            format!("\"{}\"", w[..end].replace('"', "\"\""))
        })
        .collect::<Vec<_>>()
        .join(" OR ")
}
#[async_trait]
impl Actor for Memory {
    type Input = ();
    type Actions = (Call,);
    type Events = ();
    type ConnParams = Connection;
    type Action = action::Raw;
    async fn create(ctx: &Ctx<Self>) -> Result<Self> {
        ctx.db().exec(SCHEMA).await?;
        Ok(Self {
            signing_key: crate::config::signing_key()?,
        })
    }
    /// Refuses a connection without a current session for this bot. Memory has no events
    /// or state to read over a connection, and every action authorizes in full, including
    /// the Bot's revocation check, so the connection skips that second actor call. An HTTP
    /// action opens a connection per call.
    async fn on_before_connect(self: Arc<Self>, ctx: Ctx<Self>, p: &Connection) -> Result<()> {
        self.session(&ctx, &p.session).map(drop)
    }
}
impl Memory {
    fn session(&self, ctx: &Ctx<Self>, token: &str) -> Result<String> {
        let session: auth::Session = auth::verify(self.signing_key.expose(), "session", token)?;
        let bot = bot_key(ctx)?;
        session.validate(&bot, None)?;
        Ok(bot)
    }

    async fn authorize(&self, ctx: &Ctx<Self>, token: &str) -> Result<()> {
        let bot = self.session(ctx, token)?;
        crate::actor::check_access(ctx.client()?, &bot, token).await
    }
}
impl Handler<Call> for Memory {
    async fn handle(self: Arc<Self>, ctx: Ctx<Self>, call: Call) -> Result<Value> {
        self.authorize(&ctx, &call.session).await?;
        match call.command {
            Command::Remember { key, value } => {
                ensure!(valid_fact(&key, &value), "invalid memory");
                let values = vec![
                    P::Text(key),
                    P::Text(value),
                    P::Integer(auth::now()),
                    P::Integer(MAX_FACTS),
                ];
                let saved = ctx.db().query(REMEMBER, Some(values)).await?.rows;
                ensure!(!saved.is_empty(), "memory is full; forget a fact first");
                Ok(json!({"saved":true}))
            }
            Command::Forget { key } => {
                ensure!(
                    !key.is_empty() && key.len() <= KEY_MAX,
                    "invalid memory key"
                );
                ctx.db()
                    .execute("DELETE FROM facts WHERE key=?", Some(vec![P::Text(key)]))
                    .await?;
                Ok(json!({"forgotten":true}))
            }
            Command::Snapshot => {
                let rows = ctx.db().query(SNAPSHOT, None).await?.rows;
                Ok(json!(
                    rows.into_iter()
                        .map(|r| Ok(json!({"key":string(&r,0)?,"value":string(&r,1)?})))
                        .collect::<Result<Vec<_>>>()?
                ))
            }
            Command::SearchMemory { query } => {
                ensure!(query.len() <= QUERY_MAX, "query too long");
                let terms = search_terms(&query);
                let rows = if terms.is_empty() {
                    ctx.db().query(RECENT, None).await?.rows
                } else {
                    ctx.db()
                        .query(SEARCH, Some(vec![P::Text(terms)]))
                        .await?
                        .rows
                };
                let mut seen = std::collections::HashSet::new();
                let mut out = Vec::new();
                for r in rows {
                    let key = string(&r, 0)?;
                    if seen.insert(key.clone()) {
                        out.push(json!({"key":key,"value":string(&r,1)?}));
                    }
                }
                Ok(json!(out))
            }
            _ => bail!("command is not owned by memory actor"),
        }
    }
}
pub async fn call(
    client: rivetkit::client::Client,
    bot: &str,
    session: &str,
    command: Command,
) -> Result<Value> {
    client
        .get_typed::<Memory>(
            "memory",
            [bot],
            rivetkit::client::GetOptions {
                params: Some(json!({"session":session})),
            },
        )?
        .send(Call {
            session: session.into(),
            command,
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{Connection, params};

    fn remember(db: &Connection, key: &str, max: i64) -> bool {
        db.prepare(REMEMBER)
            .unwrap()
            .exists(params![key, "value", 1, max])
            .unwrap()
    }

    #[test]
    fn fact_cap_rejects_new_keys_but_allows_updates() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE facts(key TEXT PRIMARY KEY,value TEXT NOT NULL,updated INTEGER NOT NULL)",
        )
        .unwrap();
        for i in 0..3 {
            assert!(remember(&db, &format!("k{i}"), 3));
        }
        assert!(!remember(&db, "k3", 3), "fourth key must be refused");
        assert!(remember(&db, "k1", 3), "existing key stays writable");
        let count: i64 = db
            .query_row("SELECT count(*) FROM facts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 3);
    }

    fn rows(db: &Connection, sql: &str, terms: &str) -> Vec<String> {
        let mut stmt = db.prepare(sql).unwrap();
        let bound = vec![terms; stmt.parameter_count()];
        stmt.query_map(rusqlite::params_from_iter(bound), |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    #[test]
    fn search_reads_matches_then_the_newest_facts_in_one_statement() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(SCHEMA).unwrap();
        for (i, (key, value)) in [
            ("color", "favorite color is green"),
            ("pet", "has a dog named rex"),
            ("food", "likes green curry"),
            ("city", "lives in lisbon"),
            ("music", "plays the cello"),
        ]
        .into_iter()
        .enumerate()
        {
            db.execute(
                "INSERT INTO facts(key,value,updated) VALUES(?1,?2,?3)",
                params![key, value, i64::try_from(i).unwrap()],
            )
            .unwrap();
        }
        // Matches by rank, then the two newest; "food" is in both.
        db.execute("UPDATE facts SET updated=10 WHERE key='food'", [])
            .unwrap();
        let found = rows(&db, SEARCH, &search_terms("green"));
        assert_eq!(found.len(), 4, "{found:?}");
        assert_eq!(found[2..], ["food", "music"]);
        let mut matched = found[..2].to_vec();
        matched.sort();
        assert_eq!(matched, ["color", "food"]);
        assert_eq!(
            rows(&db, SEARCH, &search_terms("nothing")),
            ["food", "music"]
        );
        assert_eq!(rows(&db, RECENT, ""), ["food", "music"]);
        // The update trigger replaced the search row.
        db.execute(
            "UPDATE facts SET value='likes red curry' WHERE key='food'",
            [],
        )
        .unwrap();
        assert_eq!(
            rows(&db, SEARCH, &search_terms("green")),
            ["color", "food", "music"]
        );
    }

    #[test]
    fn newest_fact_reads_use_the_updated_index() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(SCHEMA).unwrap();
        for sql in [SEARCH, RECENT, SNAPSHOT] {
            let mut stmt = db.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
            let values = vec![rusqlite::types::Value::Null; stmt.parameter_count()];
            let plan: Vec<String> = stmt
                .query_map(rusqlite::params_from_iter(values), |r| r.get(3))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            let plan = plan.join(" | ");
            assert!(plan.contains("facts_updated"), "{plan}");
            let lines = || plan.split(" | ");
            assert!(
                !lines()
                    .any(|l| l == "SCAN facts"
                        || l.starts_with("SCAN facts ") && !l.contains("USING")),
                "{plan}"
            );
            // Only the merged search sorts, and only its eight rows.
            assert!(sql == SEARCH || !plan.contains("TEMP B-TREE"), "{plan}");
        }
    }

    #[test]
    fn inputs_are_bounded() {
        assert!(valid_fact("k", "v"));
        assert!(!valid_fact("", "v"));
        assert!(!valid_fact("k", ""));
        assert!(!valid_fact(&"k".repeat(KEY_MAX + 1), "v"));
        assert!(!valid_fact("k", &"v".repeat(VALUE_MAX + 1)));
        let terms = search_terms(&format!("{} {}", "é".repeat(200), "x ".repeat(40)));
        assert_eq!(terms.matches(" OR ").count(), QUERY_TERMS - 1);
        assert!(terms.split(" OR ").all(|t| t.len() <= TERM_MAX + 2));
        assert_eq!(search_terms("say \"hi\""), "\"say\" OR \"\"\"hi\"\"\"");
    }
}

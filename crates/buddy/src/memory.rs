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
        ctx.db().exec("CREATE TABLE IF NOT EXISTS facts(key TEXT PRIMARY KEY,value TEXT NOT NULL,updated INTEGER NOT NULL); CREATE VIRTUAL TABLE IF NOT EXISTS facts_search USING fts5(key,value); CREATE TRIGGER IF NOT EXISTS facts_insert AFTER INSERT ON facts BEGIN INSERT INTO facts_search(key,value) VALUES(new.key,new.value); END; CREATE TRIGGER IF NOT EXISTS facts_delete AFTER DELETE ON facts BEGIN DELETE FROM facts_search WHERE key=old.key; END; CREATE TRIGGER IF NOT EXISTS facts_update AFTER UPDATE ON facts BEGIN DELETE FROM facts_search WHERE key=old.key; INSERT INTO facts_search(key,value) VALUES(new.key,new.value); END;").await?;
        Ok(Self {
            signing_key: crate::config::signing_key()?,
        })
    }
    async fn on_before_connect(self: Arc<Self>, ctx: Ctx<Self>, p: &Connection) -> Result<()> {
        self.authorize(&ctx, &p.session).await
    }
}
impl Memory {
    async fn authorize(&self, ctx: &Ctx<Self>, token: &str) -> Result<()> {
        let session: auth::Session = auth::verify(self.signing_key.expose(), "session", token)?;
        session.validate(&bot_key(ctx)?, None)?;
        crate::actor::check_access(ctx.client()?, &bot_key(ctx)?, token).await
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
                let rows = ctx
                    .db()
                    .query(
                        "SELECT key,value FROM facts ORDER BY updated DESC LIMIT 100",
                        None,
                    )
                    .await?
                    .rows;
                Ok(json!(
                    rows.into_iter()
                        .map(|r| Ok(json!({"key":string(&r,0)?,"value":string(&r,1)?})))
                        .collect::<Result<Vec<_>>>()?
                ))
            }
            Command::SearchMemory { query } => {
                ensure!(query.len() <= QUERY_MAX, "query too long");
                let terms = search_terms(&query);
                let mut rows = if terms.is_empty() {
                    Vec::new()
                } else {
                    ctx.db().query("SELECT key,value FROM facts_search WHERE facts_search MATCH ? ORDER BY rank LIMIT 6",Some(vec![P::Text(terms)])).await?.rows
                };
                // Two recent explicit facts maintain useful preferences even when lexical overlap is absent.
                rows.extend(
                    ctx.db()
                        .query(
                            "SELECT key,value FROM facts ORDER BY updated DESC LIMIT 2",
                            None,
                        )
                        .await?
                        .rows,
                );
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

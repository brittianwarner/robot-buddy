//! Bot-scoped durable facts, independently addressable from conversation history.
use crate::{auth, protocol::*};
use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;
use rivetkit::TypedClientExt;
use rivetkit::{BindParam as P, ColumnValue as V, action, prelude::*};
use serde_json::{Value, json};
use std::sync::Arc;
pub struct Memory {
    signing_key: Vec<u8>,
}
fn key(ctx: &Ctx<Memory>) -> Result<String> {
    match ctx.key().first() {
        Some(rivetkit::ActorKeySegment::String(s)) => Ok(s.clone()),
        _ => bail!("memory requires bot key"),
    }
}
fn text(row: &[V], i: usize) -> Result<String> {
    match row.get(i) {
        Some(V::Text(s)) => Ok(s.clone()),
        _ => bail!("invalid memory row"),
    }
}
#[async_trait]
impl Actor for Memory {
    type Input = ();
    type Actions = (Call,);
    type Events = ();
    type ConnParams = Connection;
    type Action = action::Raw;
    async fn create(ctx: &Ctx<Self>) -> Result<Self> {
        ctx.sql().exec("CREATE TABLE IF NOT EXISTS facts(key TEXT PRIMARY KEY,value TEXT NOT NULL,updated INTEGER NOT NULL); CREATE VIRTUAL TABLE IF NOT EXISTS facts_search USING fts5(key,value); CREATE TRIGGER IF NOT EXISTS facts_insert AFTER INSERT ON facts BEGIN INSERT INTO facts_search(key,value) VALUES(new.key,new.value); END; CREATE TRIGGER IF NOT EXISTS facts_delete AFTER DELETE ON facts BEGIN DELETE FROM facts_search WHERE key=old.key; END; CREATE TRIGGER IF NOT EXISTS facts_update AFTER UPDATE ON facts BEGIN DELETE FROM facts_search WHERE key=old.key; INSERT INTO facts_search(key,value) VALUES(new.key,new.value); END;").await?;
        Ok(Self {
            signing_key: std::env::var("BUDDY_SIGNING_KEY")
                .context("signing key required")?
                .into_bytes(),
        })
    }
    async fn on_before_connect(self: Arc<Self>, ctx: Ctx<Self>, p: &Connection) -> Result<()> {
        self.authorize(&ctx, &p.session).await
    }
}
impl Memory {
    async fn authorize(&self, ctx: &Ctx<Self>, token: &str) -> Result<()> {
        let session: auth::Session = auth::verify(&self.signing_key, "session", token)?;
        session.validate(&key(ctx)?, None)?;
        crate::actor::check_access(ctx.client()?, &key(ctx)?, token).await
    }
}
impl Handler<Call> for Memory {
    async fn handle(self: Arc<Self>, ctx: Ctx<Self>, call: Call) -> Result<Value> {
        self.authorize(&ctx, &call.session).await?;
        match call.command {
            Command::Remember { key, value } => {
                ensure!(
                    !key.is_empty() && key.len() <= 128 && !value.is_empty() && value.len() <= 2048,
                    "invalid memory"
                );
                ctx.sql().execute("INSERT INTO facts(key,value,updated) VALUES(?,?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated=excluded.updated",Some(vec![P::Text(key),P::Text(value),P::Integer(auth::now())])).await?;
                Ok(json!({"saved":true}))
            }
            Command::Forget { key } => {
                ctx.sql()
                    .execute("DELETE FROM facts WHERE key=?", Some(vec![P::Text(key)]))
                    .await?;
                Ok(json!({"forgotten":true}))
            }
            Command::Snapshot => {
                let rows = ctx
                    .sql()
                    .query(
                        "SELECT key,value FROM facts ORDER BY updated DESC LIMIT 100",
                        None,
                    )
                    .await?
                    .rows;
                Ok(json!(
                    rows.into_iter()
                        .map(|r| Ok(json!({"key":text(&r,0)?,"value":text(&r,1)?})))
                        .collect::<Result<Vec<_>>>()?
                ))
            }
            Command::SearchMemory { query } => {
                ensure!(query.len() <= 8192, "query too long");
                let terms = query
                    .split_whitespace()
                    .take(16)
                    .map(|w| format!("\"{}\"", w.replace('"', "\"\"")))
                    .collect::<Vec<_>>()
                    .join(" OR ");
                let mut rows = if terms.is_empty() {
                    Vec::new()
                } else {
                    ctx.sql().query("SELECT key,value FROM facts_search WHERE facts_search MATCH ? ORDER BY rank LIMIT 6",Some(vec![P::Text(terms)])).await?.rows
                };
                // Two recent explicit facts maintain useful preferences even when lexical overlap is absent.
                rows.extend(
                    ctx.sql()
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
                    let key = text(&r, 0)?;
                    if seen.insert(key.clone()) {
                        out.push(json!({"key":key,"value":text(&r,1)?}));
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

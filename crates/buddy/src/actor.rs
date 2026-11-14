//! Bot identity, ownership and configuration. Conversation and memory have separate actors.
use crate::{auth, protocol::*};
use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;
use rivetkit::TypedClientExt;
use rivetkit::{BindParam as P, ColumnValue as V, action, prelude::*};
use serde_json::{Value, json};
use std::sync::Arc;

pub struct Bot {
    signing_key: Vec<u8>,
}
fn string(row: &[V], index: usize) -> Result<String> {
    match row.get(index) {
        Some(V::Text(v)) => Ok(v.clone()),
        _ => bail!("invalid stored text"),
    }
}
fn params(values: &[&str]) -> Option<Vec<P>> {
    Some(values.iter().map(|v| P::Text((*v).to_owned())).collect())
}
fn bot_key(ctx: &Ctx<Bot>) -> Result<String> {
    match ctx.key().first() {
        Some(rivetkit::ActorKeySegment::String(s)) => Ok(s.clone()),
        _ => bail!("bot requires a string key"),
    }
}

#[async_trait]
impl Actor for Bot {
    type Input = ();
    type Actions = (Call,);
    type Events = (OutputEvent,);
    type ConnParams = Connection;
    type Action = action::Raw;
    async fn create(ctx: &Ctx<Self>) -> Result<Self> {
        let signing_key = std::env::var("BUDDY_SIGNING_KEY")
            .context("BUDDY_SIGNING_KEY required")?
            .into_bytes();
        ensure!(
            signing_key.len() >= 32,
            "BUDDY_SIGNING_KEY must be at least 32 bytes"
        );
        ctx.sql().exec(include_str!("schema.sql")).await?;
        Ok(Self { signing_key })
    }
    async fn on_before_connect(self: Arc<Self>, ctx: Ctx<Self>, p: &Connection) -> Result<()> {
        self.authorize(&ctx, &p.session, None).await.map(|_| ())
    }
    async fn on_subscribe(
        self: Arc<Self>,
        ctx: Ctx<Self>,
        conn: ConnCtx<Self>,
        _: String,
    ) -> Result<()> {
        self.authorize(&ctx, &conn.params()?.session, None)
            .await
            .map(|_| ())
    }
}
impl Bot {
    async fn authorize(
        &self,
        ctx: &Ctx<Self>,
        token: &str,
        role: Option<&str>,
    ) -> Result<auth::Session> {
        let session: auth::Session = auth::verify(&self.signing_key, "session", token)?;
        session.validate(&bot_key(ctx)?, role)?;
        let rows = ctx
            .sql()
            .query("SELECT revoked FROM identity WHERE singleton=1", None)
            .await?
            .rows;
        ensure!(
            rows.first()
                .is_none_or(|r| matches!(r.first(), Some(V::Integer(0)))),
            "device revoked"
        );
        Ok(session)
    }
}
impl Handler<Call> for Bot {
    async fn handle(self: Arc<Self>, ctx: Ctx<Self>, call: Call) -> Result<Value> {
        let session = self.authorize(&ctx, &call.session, None).await?;
        match call.command {
            Command::Enroll {
                device_id,
                public_key,
            } => {
                session.validate(&bot_key(&ctx)?, Some("admin"))?;
                ctx.sql().execute("INSERT OR IGNORE INTO identity(singleton,device_id,public_key) VALUES(1,?,?)",params(&[&device_id,&public_key])).await?;
                let row = ctx
                    .sql()
                    .query(
                        "SELECT device_id,public_key FROM identity WHERE singleton=1",
                        None,
                    )
                    .await?
                    .rows;
                let row = row.first().context("identity missing")?;
                ensure!(
                    string(row, 0)? == device_id && string(row, 1)? == public_key,
                    "device identity mismatch"
                );
                Ok(json!({"bot_id":bot_key(&ctx)?}))
            }
            Command::ConsumeChallenge { nonce, expires } => {
                session.validate(&bot_key(&ctx)?, Some("admin"))?;
                ensure!(
                    nonce.len() <= 128 && expires > auth::now() && expires <= auth::now() + 120,
                    "invalid challenge"
                );
                ctx.sql()
                    .execute(
                        "DELETE FROM challenges WHERE expires<=?",
                        Some(vec![P::Integer(auth::now())]),
                    )
                    .await?;
                ctx.sql()
                    .execute(
                        "INSERT INTO challenges(nonce,expires) VALUES(?,?)",
                        Some(vec![P::Text(nonce), P::Integer(expires)]),
                    )
                    .await?;
                Ok(json!({"consumed":true}))
            }
            Command::CheckAccess => Ok(json!({"authorized":true})),
            Command::Snapshot => {
                let rows = ctx
                    .sql()
                    .query(
                        "SELECT owner,pending_owner,confirm_code,claim_expires FROM identity WHERE singleton=1",
                        None,
                    )
                    .await?
                    .rows;
                let row = rows.first().context("not enrolled")?;
                Ok(
                    json!({"bot_id":bot_key(&ctx)?,"owner":string(row,0)?,"pending_owner":string(row,1)?,"confirm_code":string(row,2)?,"config_revision":1,"claim_expires":match row.get(3){Some(V::Integer(n))=>*n,_=>0}}),
                )
            }
            Command::BeginClaim => {
                ensure!(session.role == "device", "device session required");
                let token = auth::random();
                let expires = auth::now() + 600;
                let rows=ctx.sql().query("UPDATE identity SET claim_hash=?,claim_expires=?,pending_owner='',confirm_code='' WHERE singleton=1 AND owner='' AND (pending_owner='' OR claim_expires<=?) RETURNING device_id",Some(vec![P::Text(auth::digest(&token)),P::Integer(expires),P::Integer(auth::now())])).await?.rows;
                ensure!(!rows.is_empty(), "already claimed");
                Ok(json!({"token":token,"expires":expires}))
            }
            Command::ProposeClaim { token, owner } => {
                session.validate(&bot_key(&ctx)?, Some("admin"))?;
                ensure!(!owner.is_empty() && owner.len() <= 512, "invalid owner");
                let code = format!("{:06}", rand::random::<u32>() % 1_000_000);
                let rows=ctx.sql().query("UPDATE identity SET pending_owner=?,confirm_code=? WHERE singleton=1 AND owner='' AND pending_owner='' AND claim_hash=? AND claim_expires>? RETURNING device_id",Some(vec![P::Text(owner),P::Text(code.clone()),P::Text(auth::digest(&token)),P::Integer(auth::now())])).await?.rows;
                ensure!(!rows.is_empty(), "claim expired or already used");
                Ok(json!({"code":code}))
            }
            Command::ConfirmClaim { code } => {
                ensure!(
                    session.role == "device",
                    "physical device confirmation required"
                );
                let rows=ctx.sql().query("UPDATE identity SET owner=pending_owner,pending_owner='',confirm_code='',claim_hash='' WHERE singleton=1 AND owner='' AND pending_owner<>'' AND confirm_code=? AND claim_expires>? RETURNING owner",Some(vec![P::Text(code),P::Integer(auth::now())])).await?.rows;
                ensure!(!rows.is_empty(), "confirmation expired or invalid");
                Ok(json!({"claimed":true}))
            }
            Command::Revoke => {
                session.validate(&bot_key(&ctx)?, Some("admin"))?;
                ctx.sql()
                    .execute("UPDATE identity SET revoked=1 WHERE singleton=1", None)
                    .await?;
                for conn in ctx.conns_vec() {
                    let _ = conn.disconnect(Some("revoked")).await;
                }
                Ok(json!({"revoked":true}))
            }
            _ => bail!("command is not owned by bot actor"),
        }
    }
}
pub async fn check_access(
    client: rivetkit::client::Client,
    bot: &str,
    session: &str,
) -> Result<()> {
    client
        .get_typed::<Bot>(
            "bot",
            [bot],
            rivetkit::client::GetOptions {
                params: Some(json!({"session":session})),
            },
        )?
        .send(Call {
            session: session.into(),
            command: Command::CheckAccess,
        })
        .await?;
    Ok(())
}
pub fn registry() -> Registry {
    let mut r = Registry::new();
    r.register_actor::<Bot>("bot");
    r.register_actor::<crate::conversation::Conversation>("conversation");
    r.register_actor::<crate::memory::Memory>("memory");
    r
}

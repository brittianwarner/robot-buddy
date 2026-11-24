//! Bot identity, ownership and configuration. Conversation and memory have separate actors.
use crate::{
    auth,
    protocol::{Call, Command, Connection, OutputEvent},
};
use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;
use rivetkit::TypedClientExt;
use rivetkit::{BindParam as P, ColumnValue as V, action, prelude::*};
use serde_json::{Value, json};
use std::sync::Arc;

pub struct Bot {
    signing_key: auth::Secret<Vec<u8>>,
}
/// Wrong confirmation codes allowed per pending claim before it is invalidated.
pub const MAX_CLAIM_FAILURES: i64 = 5;
const DEVICE_ID_MAX: usize = 128;
/// Base64 of a 32-byte Ed25519 key is 43 characters.
const PUBLIC_KEY_MAX: usize = 64;
const CLAIM_TOKEN_MAX: usize = 128;
const OWNER_MAX: usize = 512;

/// Idempotent migration: the failure counter was added after `schema.sql` shipped.
const HAS_CLAIM_FAILURES: &str =
    "SELECT 1 FROM pragma_table_info('identity') WHERE name='claim_failures'";
const ADD_CLAIM_FAILURES: &str =
    "ALTER TABLE identity ADD COLUMN claim_failures INTEGER NOT NULL DEFAULT 0";
const BEGIN_CLAIM: &str = "UPDATE identity SET claim_hash=?,claim_expires=?,pending_owner='',\
    confirm_code='',claim_failures=0 WHERE singleton=1 AND owner='' \
    AND (pending_owner='' OR claim_expires<=?) RETURNING device_id";
const PROPOSE_CLAIM: &str = "UPDATE identity SET pending_owner=?,confirm_code=?,claim_failures=0 \
    WHERE singleton=1 AND owner='' AND pending_owner='' AND claim_hash=? AND claim_expires>? \
    RETURNING device_id";
const CONFIRM_CLAIM: &str = "UPDATE identity SET owner=pending_owner,pending_owner='',\
    confirm_code='',claim_hash='',claim_failures=0 WHERE singleton=1 AND owner='' \
    AND pending_owner<>'' AND confirm_code=? AND claim_failures<5 AND claim_expires>? \
    RETURNING owner";
/// Counts a wrong code. The fifth failure clears the pending claim in the same statement,
/// so the QR token and the code both stop working.
const FAIL_CLAIM: &str = "UPDATE identity SET claim_failures=claim_failures+1,\
    pending_owner=CASE WHEN claim_failures+1>=5 THEN '' ELSE pending_owner END,\
    confirm_code=CASE WHEN claim_failures+1>=5 THEN '' ELSE confirm_code END,\
    claim_hash=CASE WHEN claim_failures+1>=5 THEN '' ELSE claim_hash END,\
    claim_expires=CASE WHEN claim_failures+1>=5 THEN 0 ELSE claim_expires END \
    WHERE singleton=1 AND owner='' AND pending_owner<>'' AND claim_expires>? \
    RETURNING claim_failures";

/// A six-digit code drawn uniformly from the OS generator.
fn confirmation_code() -> String {
    use rand::Rng;
    format!("{:06}", rand::rngs::OsRng.gen_range(0..1_000_000u32))
}
fn valid_code(code: &str) -> bool {
    code.len() == 6 && code.bytes().all(|b| b.is_ascii_digit())
}
fn bounded(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max
}
async fn migrate(ctx: &Ctx<Bot>) -> Result<()> {
    let present = || async {
        Ok::<_, anyhow::Error>(
            !ctx.sql()
                .query(HAS_CLAIM_FAILURES, None)
                .await?
                .rows
                .is_empty(),
        )
    };
    if !present().await?
        && let Err(error) = ctx.sql().execute(ADD_CLAIM_FAILURES, None).await
    {
        ensure!(present().await?, "identity migration failed: {error}");
    }
    Ok(())
}
fn string(row: &[V], index: usize) -> Result<String> {
    match row.get(index) {
        Some(V::Text(v)) => Ok(v.clone()),
        _ => bail!("invalid stored text"),
    }
}
#[allow(
    clippy::unnecessary_wraps,
    reason = "rivetkit SQL takes Option<Vec<P>>"
)]
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
        let signing_key = auth::signing_key_from_env()?;
        ctx.sql().exec(include_str!("schema.sql")).await?;
        migrate(ctx).await?;
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
        let session: auth::Session = auth::verify(self.signing_key.expose(), "session", token)?;
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
    #[allow(clippy::too_many_lines, reason = "one match arm per command")]
    async fn handle(self: Arc<Self>, ctx: Ctx<Self>, call: Call) -> Result<Value> {
        let session = self.authorize(&ctx, &call.session, None).await?;
        match call.command {
            Command::Enroll {
                device_id,
                public_key,
            } => {
                session.validate(&bot_key(&ctx)?, Some("admin"))?;
                ensure!(
                    bounded(&device_id, DEVICE_ID_MAX) && bounded(&public_key, PUBLIC_KEY_MAX),
                    "invalid device identity"
                );
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
                let rows = ctx
                    .sql()
                    .query(
                        BEGIN_CLAIM,
                        Some(vec![
                            P::Text(auth::digest(&token)),
                            P::Integer(expires),
                            P::Integer(auth::now()),
                        ]),
                    )
                    .await?
                    .rows;
                ensure!(!rows.is_empty(), "already claimed");
                Ok(json!({"token":token,"expires":expires}))
            }
            Command::ProposeClaim { token, owner } => {
                session.validate(&bot_key(&ctx)?, Some("admin"))?;
                ensure!(bounded(&owner, OWNER_MAX), "invalid owner");
                ensure!(bounded(&token, CLAIM_TOKEN_MAX), "invalid claim");
                let code = confirmation_code();
                let rows = ctx
                    .sql()
                    .query(
                        PROPOSE_CLAIM,
                        Some(vec![
                            P::Text(owner),
                            P::Text(code.clone()),
                            P::Text(auth::digest(&token)),
                            P::Integer(auth::now()),
                        ]),
                    )
                    .await?
                    .rows;
                ensure!(!rows.is_empty(), "claim expired or already used");
                Ok(json!({"code":code}))
            }
            Command::ConfirmClaim { code } => {
                ensure!(
                    session.role == "device",
                    "physical device confirmation required"
                );
                ensure!(valid_code(&code), "invalid confirmation code");
                let now = auth::now();
                let rows = ctx
                    .sql()
                    .query(CONFIRM_CLAIM, Some(vec![P::Text(code), P::Integer(now)]))
                    .await?
                    .rows;
                if rows.is_empty() {
                    ctx.sql()
                        .query(FAIL_CLAIM, Some(vec![P::Integer(now)]))
                        .await?;
                    bail!("confirmation expired or invalid");
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{Connection, params};

    fn db() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(include_str!("schema.sql")).unwrap();
        db
    }
    fn migrate_sync(db: &Connection) {
        let present = db.prepare(HAS_CLAIM_FAILURES).unwrap().exists([]).unwrap();
        if !present {
            db.execute(ADD_CLAIM_FAILURES, []).unwrap();
        }
    }
    fn returns(db: &Connection, sql: &str, values: &[&dyn rusqlite::ToSql]) -> bool {
        db.prepare(sql).unwrap().exists(values).unwrap()
    }

    #[test]
    fn migration_is_idempotent_and_keeps_rows() {
        let db = db();
        db.execute(
            "INSERT INTO identity(singleton,device_id,public_key) VALUES(1,'d','k')",
            [],
        )
        .unwrap();
        migrate_sync(&db);
        migrate_sync(&db);
        let failures: i64 = db
            .query_row("SELECT claim_failures FROM identity", [], |r| r.get(0))
            .unwrap();
        assert_eq!(failures, 0);
    }

    #[test]
    fn five_wrong_codes_invalidate_the_pending_claim() {
        let db = db();
        migrate_sync(&db);
        db.execute(
            "INSERT INTO identity(singleton,device_id,public_key) VALUES(1,'d','k')",
            [],
        )
        .unwrap();
        let now = auth::now();
        let hash = auth::digest("claim-token");
        assert!(returns(&db, BEGIN_CLAIM, params![hash, now + 600, now]));
        assert!(returns(
            &db,
            PROPOSE_CLAIM,
            params!["owner", "123456", hash, now]
        ));
        for attempt in 1..=MAX_CLAIM_FAILURES {
            assert!(!returns(&db, CONFIRM_CLAIM, params!["000000", now]));
            let failures: i64 = db
                .query_row(FAIL_CLAIM, params![now], |r| r.get(0))
                .unwrap();
            assert_eq!(failures, attempt);
        }
        // The correct code no longer works, and the QR token is dead.
        assert!(!returns(&db, CONFIRM_CLAIM, params!["123456", now]));
        assert!(!returns(
            &db,
            PROPOSE_CLAIM,
            params!["owner", "654321", hash, now]
        ));
        let (pending, expires): (String, i64) = db
            .query_row(
                "SELECT pending_owner,claim_expires FROM identity",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(pending.is_empty());
        assert_eq!(expires, 0);
        // A fresh claim starts with a clean counter.
        assert!(returns(&db, BEGIN_CLAIM, params![hash, now + 600, now]));
        assert!(returns(
            &db,
            PROPOSE_CLAIM,
            params!["owner", "222222", hash, now]
        ));
        assert!(returns(&db, CONFIRM_CLAIM, params!["222222", now]));
    }

    #[test]
    fn a_correct_code_after_some_failures_claims() {
        let db = db();
        migrate_sync(&db);
        db.execute(
            "INSERT INTO identity(singleton,device_id,public_key) VALUES(1,'d','k')",
            [],
        )
        .unwrap();
        let now = auth::now();
        let hash = auth::digest("t");
        assert!(returns(&db, BEGIN_CLAIM, params![hash, now + 600, now]));
        assert!(returns(
            &db,
            PROPOSE_CLAIM,
            params!["o", "111111", hash, now]
        ));
        for _ in 0..4 {
            db.query_row(FAIL_CLAIM, params![now], |r| r.get::<_, i64>(0))
                .unwrap();
        }
        assert!(returns(&db, CONFIRM_CLAIM, params!["111111", now]));
        let owner: String = db
            .query_row("SELECT owner FROM identity", [], |r| r.get(0))
            .unwrap();
        assert_eq!(owner, "o");
    }

    #[test]
    fn codes_are_six_digits_and_inputs_are_bounded() {
        for _ in 0..200 {
            assert!(valid_code(&confirmation_code()));
        }
        assert!(!valid_code("12345"));
        assert!(!valid_code("1234567"));
        assert!(!valid_code("12a456"));
        assert!(!valid_code("١٢٣٤٥٦"));
        assert!(bounded("x", 1));
        assert!(!bounded("", 1));
        assert!(!bounded("xx", 1));
    }
}

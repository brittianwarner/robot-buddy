//! Versioned runtime context: the timezone and location every model request sees.
use super::{
    Context, Conversation, Ctx, Db, Receipt, Result, Tx, V, Value, auth, ensure,
    ensure_receipt_running, integer, json, named, persona::Setting, string, write_receipt,
};
use crate::context::ContextConfig;

pub(super) const CONTEXT_SQL: &str =
    "SELECT revision, config, updated FROM context_settings WHERE singleton = 1";
/// The runtime context as a versioned setting.
const CONTEXT: Setting = Setting {
    update: r"
        UPDATE context_settings SET revision = revision + 1, config = ?, updated = ?
        WHERE singleton = 1 AND revision = ?
        RETURNING revision",
    insert_version: r"
        INSERT INTO context_versions(revision, config, reason, updated) VALUES (?, ?, ?, ?)",
    trim: "DELETE FROM context_versions WHERE revision < ?",
    conflict: "context revision conflict; read it again",
};

/// A [`CONTEXT_SQL`] row with the clock read now.
pub(super) fn context_json(row: &[V]) -> Result<Value> {
    let config: ContextConfig = serde_json::from_str(&string(row, 1)?)?;
    Ok(json!({
        "revision": integer(row, 0)?,
        "config": config,
        "updated": integer(row, 2)?,
        "current": config.at(auth::now())?,
    }))
}

impl Conversation {
    pub(super) async fn context_config(&self, ctx: &Ctx<Self>) -> Result<Value> {
        let rows = ctx.db().query(CONTEXT_SQL, None).await?.rows;
        context_json(rows.first().context("context configuration missing")?)
    }

    pub(super) async fn set_context(
        &self,
        ctx: &Ctx<Self>,
        expected_revision: i64,
        config: &ContextConfig,
        reason: &str,
        receipt: Receipt<'_>,
    ) -> Result<Value> {
        config.validate()?;
        ensure!(
            expected_revision >= 1 && !reason.trim().is_empty() && reason.len() <= 500,
            "invalid context update"
        );
        let encoded = serde_json::to_string(config)?;
        let write = |tx: Tx| async move {
            ensure_receipt_running(&tx, receipt).await?;
            let revision = CONTEXT
                .write(&tx, expected_revision, &encoded, reason, auth::now())
                .await?;
            let result = json!({
                "revision": revision,
                "saved": true,
                "applies": "next_model_request",
            });
            write_receipt(&tx, receipt, &result).await?;
            Ok(result)
        };
        ctx.db()
            .transaction(write, named("conversation.set_context"))
            .await
    }
}

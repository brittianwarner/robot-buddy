//! Versioned runtime context: the timezone and location every model request sees.
use super::{
    Context, Conversation, Ctx, P, Receipt, Result, SqliteDbExt, SqliteTransaction, TxOptions,
    Value, auth, ensure, ensure_receipt_running, integer, json, string, trim_versions,
    write_receipt,
};
use crate::context::ContextConfig;

const CONTEXT_SQL: &str =
    "SELECT revision, config, updated FROM context_settings WHERE singleton = 1";
const UPDATE_CONTEXT_SQL: &str = r"
    UPDATE context_settings SET revision = revision + 1, config = ?, updated = ?
    WHERE singleton = 1 AND revision = ?
    RETURNING revision";
const INSERT_VERSION_SQL: &str = r"
    INSERT INTO context_versions(revision, config, reason, updated) VALUES (?, ?, ?, ?)";
const TRIM_VERSIONS_SQL: &str = "DELETE FROM context_versions WHERE revision < ?";

impl Conversation {
    pub(super) async fn context_config(&self, ctx: &Ctx<Self>) -> Result<Value> {
        let rows = ctx.sql().query(CONTEXT_SQL, None).await?.rows;
        let row = rows.first().context("context configuration missing")?;
        let config: ContextConfig = serde_json::from_str(&string(row, 1)?)?;
        Ok(json!({
            "revision": integer(row, 0)?,
            "config": config,
            "updated": integer(row, 2)?,
            "current": config.at(auth::now())?,
        }))
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
        let write = |tx: SqliteTransaction| async move {
            ensure_receipt_running(&tx, receipt).await?;
            let now = auth::now();
            let update = vec![
                P::Text(encoded.clone()),
                P::Integer(now),
                P::Integer(expected_revision),
            ];
            let rows = tx.execute(UPDATE_CONTEXT_SQL, Some(update)).await?.rows;
            let row = rows
                .first()
                .context("context revision conflict; read it again")?;
            let revision = integer(row, 0)?;
            let version = vec![
                P::Integer(revision),
                P::Text(encoded),
                P::Text(reason.into()),
                P::Integer(now),
            ];
            tx.execute(INSERT_VERSION_SQL, Some(version)).await?;
            trim_versions(&tx, TRIM_VERSIONS_SQL, revision).await?;
            let result = json!({
                "revision": revision,
                "saved": true,
                "applies": "next_model_request",
            });
            write_receipt(&tx, receipt, &result).await?;
            Ok(result)
        };
        ctx.sql().transaction(write, TxOptions::default()).await
    }
}

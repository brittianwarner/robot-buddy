//! The conversation owns its bounded, versioned harness configuration.
use super::{
    Context, Conversation, Ctx, P, Receipt, Result, SqliteDbExt, SqliteTransaction, TxOptions,
    Value, auth, ensure, ensure_receipt_running, integer, json, string, trim_versions,
    write_receipt,
};

const PROMPT_SQL: &str = "SELECT revision, prompt, updated FROM system_prompt WHERE singleton = 1";
pub(super) const UPDATE_PROMPT_SQL: &str = r"
    UPDATE system_prompt SET revision = revision + 1, prompt = ?, updated = ?
    WHERE singleton = 1 AND revision = ?
    RETURNING revision";
pub(super) const INSERT_PROMPT_VERSION_SQL: &str = r"
    INSERT INTO prompt_versions(revision, prompt, reason, updated) VALUES (?, ?, ?, ?)";
pub(super) const TRIM_PROMPT_VERSIONS_SQL: &str = "DELETE FROM prompt_versions WHERE revision < ?";
const PROMPT_HISTORY_SQL: &str = r"
    SELECT revision, prompt, reason, updated FROM prompt_versions
    ORDER BY revision DESC LIMIT 16";

impl Conversation {
    pub(super) async fn persona(&self, ctx: &Ctx<Self>) -> Result<Value> {
        let rows = ctx.sql().query(PROMPT_SQL, None).await?.rows;
        let row = rows.first().context("system prompt missing")?;
        Ok(json!({
            "revision": integer(row, 0)?,
            "prompt": string(row, 1)?,
            "updated": integer(row, 2)?,
        }))
    }

    pub(super) async fn set_persona(
        &self,
        ctx: &Ctx<Self>,
        expected_revision: i64,
        prompt: &str,
        reason: &str,
        receipt: Receipt<'_>,
    ) -> Result<Value> {
        ensure!(
            expected_revision >= 1
                && !prompt.trim().is_empty()
                && prompt.len() <= 8192
                && !reason.trim().is_empty()
                && reason.len() <= 500,
            "invalid system prompt update"
        );
        let write = |tx: SqliteTransaction| async move {
            ensure_receipt_running(&tx, receipt).await?;
            let now = auth::now();
            let update = vec![
                P::Text(prompt.into()),
                P::Integer(now),
                P::Integer(expected_revision),
            ];
            let rows = tx.execute(UPDATE_PROMPT_SQL, Some(update)).await?.rows;
            let row = rows
                .first()
                .context("system prompt revision conflict; read it again")?;
            let revision = integer(row, 0)?;
            let version = vec![
                P::Integer(revision),
                P::Text(prompt.into()),
                P::Text(reason.into()),
                P::Integer(now),
            ];
            tx.execute(INSERT_PROMPT_VERSION_SQL, Some(version)).await?;
            trim_versions(&tx, TRIM_PROMPT_VERSIONS_SQL, revision).await?;
            let result = json!({ "revision": revision, "saved": true, "applies": "next_turn" });
            // The prompt and its tool receipt commit together; a recovery cannot apply it twice.
            write_receipt(&tx, receipt, &result).await?;
            Ok(result)
        };
        ctx.sql().transaction(write, TxOptions::default()).await
    }

    pub(super) async fn persona_history(&self, ctx: &Ctx<Self>) -> Result<Value> {
        let rows = ctx.sql().query(PROMPT_HISTORY_SQL, None).await?.rows;
        let versions = rows
            .iter()
            .map(|r| {
                Ok(json!({
                    "revision": integer(r, 0)?,
                    "prompt": string(r, 1)?,
                    "reason": string(r, 2)?,
                    "updated": integer(r, 3)?,
                }))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(json!(versions))
    }
}

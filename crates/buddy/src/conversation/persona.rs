//! The conversation owns its bounded, versioned harness configuration.
use super::{
    Context, Conversation, Ctx, Db, P, Receipt, Result, Tx, Value, auth, ensure,
    ensure_receipt_running, integer, json, named, string, trim_versions, write_receipt,
};

/// A singleton setting with a revision and a bounded version history: the system prompt,
/// voice and context. Each write is a compare-and-set on the revision, one version row
/// and a trim, inside the caller's transaction.
pub(super) struct Setting {
    /// Binds value, updated, expected revision; returns the new revision.
    pub update: &'static str,
    /// Binds revision, value, reason, updated.
    pub insert_version: &'static str,
    /// Binds the oldest revision kept.
    pub trim: &'static str,
    /// The error when the revision moved.
    pub conflict: &'static str,
}

impl Setting {
    /// Moves the setting from `expected` to the next revision with `value`, records the
    /// version and trims the history. Returns the new revision.
    pub(super) async fn write(
        &self,
        tx: &Tx,
        expected: i64,
        value: &str,
        reason: &str,
        now: i64,
    ) -> Result<i64> {
        let update = vec![P::Text(value.into()), P::Integer(now), P::Integer(expected)];
        let rows = tx.execute(self.update, Some(update)).await?.rows;
        let revision = integer(rows.first().context(self.conflict)?, 0)?;
        let version = vec![
            P::Integer(revision),
            P::Text(value.into()),
            P::Text(reason.into()),
            P::Integer(now),
        ];
        tx.execute(self.insert_version, Some(version)).await?;
        trim_versions(tx, self.trim, revision).await?;
        Ok(revision)
    }
}

pub(super) const PROMPT_SQL: &str =
    "SELECT revision, prompt, updated FROM system_prompt WHERE singleton = 1";
/// The system prompt as a versioned setting.
pub(super) const PROMPT: Setting = Setting {
    update: r"
        UPDATE system_prompt SET revision = revision + 1, prompt = ?, updated = ?
        WHERE singleton = 1 AND revision = ?
        RETURNING revision",
    insert_version: r"
        INSERT INTO prompt_versions(revision, prompt, reason, updated) VALUES (?, ?, ?, ?)",
    trim: "DELETE FROM prompt_versions WHERE revision < ?",
    conflict: "system prompt revision conflict; read it again",
};
const PROMPT_HISTORY_SQL: &str = r"
    SELECT revision, prompt, reason, updated FROM prompt_versions
    ORDER BY revision DESC LIMIT 16";

impl Conversation {
    pub(super) async fn persona(&self, ctx: &Ctx<Self>) -> Result<Value> {
        let rows = ctx.db().query(PROMPT_SQL, None).await?.rows;
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
        let write = |tx: Tx| async move {
            ensure_receipt_running(&tx, receipt).await?;
            let revision = PROMPT
                .write(&tx, expected_revision, prompt, reason, auth::now())
                .await?;
            let result = json!({ "revision": revision, "saved": true, "applies": "next_turn" });
            // The prompt and its tool receipt commit together; a recovery cannot apply it twice.
            write_receipt(&tx, receipt, &result).await?;
            Ok(result)
        };
        ctx.db()
            .transaction(write, named("conversation.set_persona"))
            .await
    }

    pub(super) async fn persona_history(&self, ctx: &Ctx<Self>) -> Result<Value> {
        let rows = ctx.db().query(PROMPT_HISTORY_SQL, None).await?.rows;
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

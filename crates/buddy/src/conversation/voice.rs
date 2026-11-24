//! Versioned voice settings: the curated voice and delivery every speech request uses.
use super::{
    Context, Conversation, Ctx, Db, P, Receipt, Result, Tx, Value, auth, ensure,
    ensure_receipt_running, integer, json, named, string, trim_versions, write_receipt,
};
use crate::voice::VoiceConfig;

const VOICE_SQL: &str = "SELECT revision, config, updated FROM voice_settings WHERE singleton = 1";
pub(super) const UPDATE_VOICE_SQL: &str = r"
    UPDATE voice_settings SET revision = revision + 1, config = ?, updated = ?
    WHERE singleton = 1 AND revision = ?
    RETURNING revision";
pub(super) const INSERT_VOICE_VERSION_SQL: &str = r"
    INSERT INTO voice_versions(revision, config, reason, updated) VALUES (?, ?, ?, ?)";
pub(super) const TRIM_VOICE_VERSIONS_SQL: &str = "DELETE FROM voice_versions WHERE revision < ?";

impl Conversation {
    pub(super) async fn voice_config(&self, ctx: &Ctx<Self>) -> Result<Value> {
        let rows = ctx.db().query(VOICE_SQL, None).await?.rows;
        let row = rows.first().context("voice configuration missing")?;
        let config: VoiceConfig = serde_json::from_str(&string(row, 1)?)?;
        Ok(json!({
            "revision": integer(row, 0)?,
            "config": config,
            "voice_id": config.voice.id(),
            "name": config.voice.name(),
            "updated": integer(row, 2)?,
            "choices": crate::voice::catalog(),
        }))
    }

    pub(super) async fn set_voice(
        &self,
        ctx: &Ctx<Self>,
        expected_revision: i64,
        config: &VoiceConfig,
        reason: &str,
        receipt: Receipt<'_>,
    ) -> Result<Value> {
        config.validate()?;
        ensure!(
            expected_revision >= 1 && !reason.trim().is_empty() && reason.len() <= 500,
            "invalid voice update"
        );
        let encoded = serde_json::to_string(config)?;
        let write = |tx: Tx| async move {
            ensure_receipt_running(&tx, receipt).await?;
            let now = auth::now();
            let update = vec![
                P::Text(encoded.clone()),
                P::Integer(now),
                P::Integer(expected_revision),
            ];
            let rows = tx.execute(UPDATE_VOICE_SQL, Some(update)).await?.rows;
            let row = rows
                .first()
                .context("voice revision conflict; read it again")?;
            let revision = integer(row, 0)?;
            let version = vec![
                P::Integer(revision),
                P::Text(encoded),
                P::Text(reason.into()),
                P::Integer(now),
            ];
            tx.execute(INSERT_VOICE_VERSION_SQL, Some(version)).await?;
            trim_versions(&tx, TRIM_VOICE_VERSIONS_SQL, revision).await?;
            let result = json!({
                "revision": revision,
                "saved": true,
                "applies": "next_speech_request",
                "voice": config.voice,
                "voice_id": config.voice.id(),
            });
            write_receipt(&tx, receipt, &result).await?;
            Ok(result)
        };
        ctx.db()
            .transaction(write, named("conversation.set_voice"))
            .await
    }
}

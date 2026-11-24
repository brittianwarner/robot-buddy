//! Versioned voice settings: the curated voice and delivery every speech request uses.
use super::{
    Context, Conversation, Ctx, Db, Receipt, Result, Tx, Value, auth, ensure,
    ensure_receipt_running, integer, json, named, persona::Setting, string, write_receipt,
};
use crate::voice::VoiceConfig;

const VOICE_SQL: &str = "SELECT revision, config, updated FROM voice_settings WHERE singleton = 1";
/// The voice settings as a versioned setting.
pub(super) const VOICE: Setting = Setting {
    update: r"
        UPDATE voice_settings SET revision = revision + 1, config = ?, updated = ?
        WHERE singleton = 1 AND revision = ?
        RETURNING revision",
    insert_version: r"
        INSERT INTO voice_versions(revision, config, reason, updated) VALUES (?, ?, ?, ?)",
    trim: "DELETE FROM voice_versions WHERE revision < ?",
    conflict: "voice revision conflict; read it again",
};

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
            let revision = VOICE
                .write(&tx, expected_revision, &encoded, reason, auth::now())
                .await?;
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

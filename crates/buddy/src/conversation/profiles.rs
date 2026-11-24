//! Bounded persona library; selecting a profile updates prompt and voice atomically.
use super::{
    Context, Conversation, Ctx, P, Receipt, Result, SqliteDbExt, SqliteTransaction, V, Value, auth,
    ensure, ensure_receipt_running, integer, json, named, params,
    persona::{INSERT_PROMPT_VERSION_SQL, TRIM_PROMPT_VERSIONS_SQL, UPDATE_PROMPT_SQL},
    string, trim_versions,
    voice::{INSERT_VOICE_VERSION_SQL, TRIM_VOICE_VERSIONS_SQL, UPDATE_VOICE_SQL},
    write_receipt,
};
use crate::{persona::PersonaDraft, voice::VoiceConfig};

/// The most persona names the library holds.
const MAX_PERSONAS: i64 = 50;
const LIST_SQL: &str = r"
    SELECT p.name, p.latest, substr(v.reason, 1, 160), v.updated
    FROM personas p JOIN persona_versions v ON v.name = p.name AND v.version = p.latest
    ORDER BY p.name LIMIT 50";
/// Version 0 selects the latest version.
const VERSION_SQL: &str = r"
    SELECT v.name, v.version, v.prompt, v.voice, v.reason, v.updated
    FROM persona_versions v JOIN personas p ON p.name = v.name
    WHERE v.name = ? AND v.version = CASE WHEN ? = 0 THEN p.latest ELSE ? END";
const VERSIONS_SQL: &str = r"
    SELECT version, reason, updated FROM persona_versions
    WHERE name = ? ORDER BY version DESC LIMIT 16";
const COUNT_SQL: &str = "SELECT COUNT(*) FROM personas";
const INSERT_NAME_SQL: &str = "INSERT INTO personas(name, latest) VALUES (?, 1)";
const BUMP_LATEST_SQL: &str = r"
    UPDATE personas SET latest = latest + 1 WHERE name = ? AND latest = ?
    RETURNING latest";
const INSERT_VERSION_SQL: &str = r"
    INSERT INTO persona_versions(name, version, prompt, voice, reason, updated)
    VALUES (?, ?, ?, ?, ?, ?)";
const TRIM_VERSIONS_SQL: &str = "DELETE FROM persona_versions WHERE name = ? AND version < ?";
const PROFILE_SQL: &str =
    "SELECT prompt, voice FROM persona_versions WHERE name = ? AND version = ?";

/// The settings revision a write moved to.
fn revision(rows: &[Vec<V>], conflict: &'static str) -> Result<i64> {
    integer(rows.first().context(conflict)?, 0)
}

impl Conversation {
    pub(super) async fn list_personas(&self, ctx: &Ctx<Self>) -> Result<Value> {
        let rows = ctx.sql().query(LIST_SQL, None).await?.rows;
        let personas = rows
            .iter()
            .map(|r| {
                Ok(json!({
                    "name": string(r, 0)?,
                    "latest_version": integer(r, 1)?,
                    "reason": string(r, 2)?,
                    "updated": integer(r, 3)?,
                }))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(json!(personas))
    }

    pub(super) async fn get_persona(
        &self,
        ctx: &Ctx<Self>,
        name: &str,
        version: i64,
    ) -> Result<Value> {
        crate::persona::validate_name(name)?;
        ensure!(
            version >= 0,
            "version must be nonnegative; 0 selects latest"
        );
        let read = |tx: SqliteTransaction| async move {
            let selector = vec![
                P::Text(name.into()),
                P::Integer(version),
                P::Integer(version),
            ];
            let rows = tx.execute(VERSION_SQL, Some(selector)).await?.rows;
            let r = rows.first().context("persona version not found")?;
            let versions = tx.execute(VERSIONS_SQL, params(&[name])).await?.rows;
            let versions = versions
                .iter()
                .map(|v| {
                    Ok(json!({
                        "version": integer(v, 0)?,
                        "reason": string(v, 1)?,
                        "updated": integer(v, 2)?,
                    }))
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(json!({
                "name": string(r, 0)?,
                "version": integer(r, 1)?,
                "prompt": string(r, 2)?,
                "voice": serde_json::from_str::<VoiceConfig>(&string(r, 3)?)?,
                "reason": string(r, 4)?,
                "updated": integer(r, 5)?,
                "versions": versions,
            }))
        };
        ctx.sql()
            .transaction(read, named("conversation.get_persona"))
            .await
    }

    pub(super) async fn save_profile(
        &self,
        ctx: &Ctx<Self>,
        draft: &PersonaDraft,
        receipt: Receipt<'_>,
    ) -> Result<Value> {
        draft.validate()?;
        let voice = serde_json::to_string(&draft.voice)?;
        let write = |tx: SqliteTransaction| async move {
            ensure_receipt_running(&tx, receipt).await?;
            if draft.expected_version == 0 {
                let count = tx.execute(COUNT_SQL, None).await?.rows;
                let count = integer(count.first().context("count missing")?, 0)?;
                ensure!(count < MAX_PERSONAS, "persona library is full (50 names)");
                tx.execute(INSERT_NAME_SQL, params(&[&draft.name])).await?;
            } else {
                let expected = vec![
                    P::Text(draft.name.clone()),
                    P::Integer(draft.expected_version),
                ];
                let changed = tx.execute(BUMP_LATEST_SQL, Some(expected)).await?.rows;
                ensure!(
                    !changed.is_empty(),
                    "persona version conflict; read it again"
                );
            }
            let version = draft.expected_version + 1;
            let row = vec![
                P::Text(draft.name.clone()),
                P::Integer(version),
                P::Text(draft.prompt.clone()),
                P::Text(voice),
                P::Text(draft.reason.clone()),
                P::Integer(auth::now()),
            ];
            tx.execute(INSERT_VERSION_SQL, Some(row)).await?;
            let trim = vec![
                P::Text(draft.name.clone()),
                P::Integer((version - 15).max(1)),
            ];
            tx.execute(TRIM_VERSIONS_SQL, Some(trim)).await?;
            let result = json!({
                "name": draft.name,
                "version": version,
                "saved": true,
                "active": false,
            });
            write_receipt(&tx, receipt, &result).await?;
            Ok(result)
        };
        ctx.sql()
            .transaction(write, named("conversation.save_persona"))
            .await
    }

    pub(super) async fn activate_profile(
        &self,
        ctx: &Ctx<Self>,
        name: &str,
        version: i64,
        expected: (i64, i64),
        receipt: Receipt<'_>,
    ) -> Result<Value> {
        crate::persona::validate_name(name)?;
        ensure!(
            version > 0 && expected.0 > 0 && expected.1 > 0,
            "activation requires explicit profile version and current prompt/voice revisions"
        );
        let write = |tx: SqliteTransaction| async move {
            ensure_receipt_running(&tx, receipt).await?;
            let selector = vec![P::Text(name.into()), P::Integer(version)];
            let rows = tx.execute(PROFILE_SQL, Some(selector)).await?.rows;
            let row = rows.first().context("persona version not found")?;
            let prompt = string(row, 0)?;
            let config = serde_json::from_str::<VoiceConfig>(&string(row, 1)?)?;
            config.validate()?;
            let voice = serde_json::to_string(&config)?;
            let now = auth::now();
            let reason = format!("Activate {name} version {version}");
            let update = vec![
                P::Text(prompt.clone()),
                P::Integer(now),
                P::Integer(expected.0),
            ];
            let rows = tx.execute(UPDATE_PROMPT_SQL, Some(update)).await?.rows;
            let prompt_revision = revision(&rows, "prompt revision conflict")?;
            let update = vec![
                P::Text(voice.clone()),
                P::Integer(now),
                P::Integer(expected.1),
            ];
            let rows = tx.execute(UPDATE_VOICE_SQL, Some(update)).await?.rows;
            let voice_revision = revision(&rows, "voice revision conflict")?;
            let prompt_version = vec![
                P::Integer(prompt_revision),
                P::Text(prompt),
                P::Text(reason.clone()),
                P::Integer(now),
            ];
            tx.execute(INSERT_PROMPT_VERSION_SQL, Some(prompt_version))
                .await?;
            let voice_version = vec![
                P::Integer(voice_revision),
                P::Text(voice),
                P::Text(reason),
                P::Integer(now),
            ];
            tx.execute(INSERT_VOICE_VERSION_SQL, Some(voice_version))
                .await?;
            trim_versions(&tx, TRIM_PROMPT_VERSIONS_SQL, prompt_revision).await?;
            trim_versions(&tx, TRIM_VOICE_VERSIONS_SQL, voice_revision).await?;
            let result = json!({
                "name": name,
                "version": version,
                "activated": true,
                "prompt_revision": prompt_revision,
                "voice_revision": voice_revision,
                "prompt_applies": "next_turn",
                "voice_applies": "next_speech_request",
            });
            write_receipt(&tx, receipt, &result).await?;
            Ok(result)
        };
        ctx.sql()
            .transaction(write, named("conversation.activate_persona"))
            .await
    }
}

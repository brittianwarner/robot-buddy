//! Durable speech admission: only a running turn can commit chunks.
use super::{
    Context, Conversation, Ctx, OutputEvent, P, RUNNING_TURN_SQL, Result, SqliteDbExt,
    SqliteTransaction, Value, ensure, integer, json, named, params, string,
};

const SPEECH_SETTINGS_SQL: &str = r"
    SELECT p.prompt, v.config, v.revision, v.updated
    FROM system_prompt p CROSS JOIN voice_settings v
    WHERE p.singleton = 1 AND v.singleton = 1";
/// The first pin wins: a turn that begins speech again keeps the voice it already speaks in.
const PIN_VOICE_SQL: &str = r"
    INSERT INTO speech_turns(turn_id, voice) VALUES (?, ?)
    ON CONFLICT(turn_id) DO NOTHING";
const TEXT_DELTA_SQL: &str = r"
    INSERT INTO events(turn_id, kind, payload) SELECT ?, 'text.delta', ?
    WHERE EXISTS(SELECT 1 FROM turns WHERE id = ? AND status = 'running')
    RETURNING sequence";
const CHUNK_SQL: &str = r"
    INSERT INTO speech_chunks(turn_id, chunk_index, text) SELECT ?, ?, ?
    WHERE EXISTS(SELECT 1 FROM turns WHERE id = ? AND status = 'running')
    RETURNING chunk_index";
const SPEECH_READY_SQL: &str = r"
    INSERT INTO events(turn_id, kind, payload) SELECT ?, 'speech.ready', ?
    WHERE EXISTS(SELECT 1 FROM turns WHERE id = ? AND status = 'running')
    RETURNING sequence";
const READ_CHUNK_SQL: &str = r"
    SELECT c.text, s.voice FROM speech_chunks c
    JOIN speech_turns s ON s.turn_id = c.turn_id
    WHERE c.turn_id = ? AND c.chunk_index = ?";
const SPEECH_VOICE_SQL: &str = "SELECT voice FROM speech_turns WHERE turn_id = ?";
const TURN_STATUS_SQL: &str = "SELECT status FROM turns WHERE id = ?";
/// Chunks one turn may speak, and the largest chunk the splitter can produce.
const MAX_CHUNKS: u32 = 64;
const MAX_CHUNK_BYTES: usize = crate::sentences::HARD_LIMIT + 4;

/// Inserts one event inside a transaction. Zero rows means the turn stopped running.
async fn insert_event(tx: &SqliteTransaction, sql: &str, id: &str, payload: &Value) -> Result<i64> {
    let rows = tx
        .execute(sql, params(&[id, &payload.to_string(), id]))
        .await?
        .rows;
    integer(rows.first().context("turn cancelled")?, 0)
}

impl Conversation {
    /// Pins this turn's voice before its first spoken text and returns the character prompt.
    /// It may run more than once per turn; the first pin is kept.
    pub(super) async fn begin_speech(&self, ctx: &Ctx<Self>, id: &str) -> Result<String> {
        let pin = |tx: SqliteTransaction| async move {
            let rows = tx.execute(SPEECH_SETTINGS_SQL, None).await?.rows;
            let row = rows.first().context("speech settings missing")?;
            let config: crate::voice::VoiceConfig = serde_json::from_str(&string(row, 1)?)?;
            // The voice catalog is not pinned: a chunk read needs only the chosen voice.
            let voice = json!({
                "config": config,
                "revision": integer(row, 2)?,
                "updated": integer(row, 3)?,
                "voice_id": config.voice.id(),
                "name": config.voice.name(),
            });
            let running = tx.execute(RUNNING_TURN_SQL, params(&[id])).await?.rows;
            ensure!(!running.is_empty(), "turn cancelled");
            tx.execute(PIN_VOICE_SQL, params(&[id, &voice.to_string()]))
                .await?;
            string(row, 0)
        };
        ctx.sql()
            .transaction(pin, named("conversation.begin_speech"))
            .await
    }

    pub(super) async fn speech_voice(&self, ctx: &Ctx<Self>, id: &str) -> Result<Value> {
        let rows = ctx.sql().query(SPEECH_VOICE_SQL, params(&[id])).await?.rows;
        match rows.first() {
            Some(row) => Ok(serde_json::from_str(&string(row, 0)?)?),
            None => self.voice_config(ctx).await,
        }
    }

    /// Commits streamed text and the sentence chunks it completed in one transaction
    /// guarded on the running turn: `text.delta` first, then each chunk with its
    /// `speech.ready`. Events are delivered only after the commit.
    pub(super) async fn commit_stream(
        &self,
        ctx: &Ctx<Self>,
        id: &str,
        text: String,
        chunks: Vec<(u32, String)>,
    ) -> Result<()> {
        if text.is_empty() && chunks.is_empty() {
            return Ok(());
        }
        for (index, chunk) in &chunks {
            ensure!(
                *index < MAX_CHUNKS && chunk.len() <= MAX_CHUNK_BYTES,
                "speech chunk budget exceeded"
            );
        }
        let mut payloads = Vec::with_capacity(chunks.len() + 1);
        if !text.is_empty() {
            payloads.push(("text.delta", json!({ "text": text })));
        }
        let (payloads_ref, chunks_ref) = (&payloads, &chunks);
        let write = move |tx: SqliteTransaction| async move {
            let mut sequences = Vec::with_capacity(payloads_ref.len() + chunks_ref.len());
            if let Some((_, delta)) = payloads_ref.first() {
                sequences.push(insert_event(&tx, TEXT_DELTA_SQL, id, delta).await?);
            }
            for (index, chunk) in chunks_ref {
                let index = i64::from(*index);
                let values = vec![
                    P::Text(id.into()),
                    P::Integer(index),
                    P::Text(chunk.clone()),
                    P::Text(id.into()),
                ];
                let rows = tx.execute(CHUNK_SQL, Some(values)).await?.rows;
                ensure!(!rows.is_empty(), "turn cancelled");
                let ready = json!({ "index": index });
                sequences.push(insert_event(&tx, SPEECH_READY_SQL, id, &ready).await?);
            }
            Ok(sequences)
        };
        let sequences = ctx
            .sql()
            .transaction(write, named("conversation.commit_stream"))
            .await?;
        let ready = chunks
            .iter()
            .map(|(index, _)| ("speech.ready", json!({ "index": index })));
        let events = payloads.into_iter().chain(ready);
        for (sequence, (kind, payload)) in sequences.into_iter().zip(events) {
            let event = OutputEvent {
                sequence,
                turn_id: id.into(),
                kind: kind.into(),
                payload,
            };
            self.deliver(ctx, &event).await;
        }
        Ok(())
    }

    pub(super) async fn speech_chunk(
        &self,
        ctx: &Ctx<Self>,
        id: &str,
        index: u32,
    ) -> Result<Value> {
        ensure!(
            !id.is_empty() && id.len() <= 128 && index <= MAX_CHUNKS,
            "invalid speech cursor"
        );
        let read = |tx: SqliteTransaction| async move {
            let rows = tx.execute(TURN_STATUS_SQL, params(&[id])).await?.rows;
            let status = string(rows.first().context("turn missing")?, 0)?;
            ensure!(
                ["running", "completed"].contains(&status.as_str()),
                "speech turn stopped"
            );
            let cursor = Some(vec![P::Text(id.into()), P::Integer(i64::from(index))]);
            let rows = tx.execute(READ_CHUNK_SQL, cursor).await?.rows;
            if let Some(row) = rows.first() {
                let voice: Value = serde_json::from_str(&string(row, 1)?)?;
                let text = string(row, 0)?;
                return Ok(json!({
                    "state": "ready",
                    "index": index,
                    "text": text,
                    "voice": voice,
                }));
            }
            let state = if status == "completed" {
                "done"
            } else {
                "pending"
            };
            Ok(json!({ "state": state }))
        };
        ctx.sql()
            .transaction(read, named("conversation.speech_chunk"))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{Connection, OptionalExtension};

    fn database() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(include_str!("../conversation.sql"))
            .unwrap();
        db.execute(
            "INSERT INTO turns(id, request_id, text, status, created) VALUES (?1, ?1, '', ?2, 0)",
            ["live", "running"],
        )
        .unwrap();
        db.execute(
            "INSERT INTO turns(id, request_id, text, status, created) VALUES (?1, ?1, '', ?2, 0)",
            ["stopped", "cancelled"],
        )
        .unwrap();
        db
    }

    /// The SQL half of `begin_speech`: the running check, then the pin.
    fn pin(db: &Connection, id: &str, voice: &str) -> Result<()> {
        let running = db
            .query_row(RUNNING_TURN_SQL, [id], |_| Ok(()))
            .optional()?;
        ensure!(running.is_some(), "turn cancelled");
        db.execute(PIN_VOICE_SQL, [id, voice])?;
        Ok(())
    }

    fn pinned(db: &Connection, id: &str) -> Option<String> {
        db.query_row(SPEECH_VOICE_SQL, [id], |row| row.get(0))
            .optional()
            .unwrap()
    }

    #[test]
    fn pinning_twice_keeps_the_first_voice() {
        let db = database();
        pin(&db, "live", "first").unwrap();
        pin(&db, "live", "second").unwrap();
        assert_eq!(pinned(&db, "live").as_deref(), Some("first"));
    }

    #[test]
    fn a_stopped_turn_is_never_pinned() {
        let db = database();
        assert!(pin(&db, "stopped", "voice").is_err());
        assert!(pin(&db, "missing", "voice").is_err());
        assert_eq!(pinned(&db, "stopped"), None);
    }
}

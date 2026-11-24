//! Conversation transcript and Rig run ownership, independent of device identity and memory.
mod context;
mod delivery;
mod harness;
pub mod history;
mod persona;
mod profiles;
mod speech;
mod tools;
mod voice;
use crate::{
    auth,
    db::{bot_key, integer, params, string},
    model::{Delta, Model, ToolCall},
    protocol::{Call, Command, Connection, OutputEvent},
};
use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;
use rivetkit::{
    BindParam as P, ColumnValue as V, SqliteDbExt, SqliteTransaction,
    SqliteTransactionOptions as TxOptions, action, prelude::*,
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;

pub struct Conversation {
    signing_key: auth::Secret<Vec<u8>>,
    /// Batches of the history search rebuild remain; see [`history`].
    history_backfill: bool,
    draining: AtomicBool,
    /// Shared so a model stream can run on its own task.
    model: Arc<Model>,
    last_authorized: tokio::sync::Mutex<Option<std::time::Instant>>,
    /// In-process stop signal for the running turn. A terminal `finish_turn` fires it so
    /// a provider stream drops at once. Durable turn status stays the source of truth.
    running: std::sync::Mutex<Option<(String, CancellationToken)>>,
    /// Verified session expiry per connection, so events do not re-verify each session.
    sessions: delivery::Sessions,
}

/// The turn and tool call a settings write records its result for, when a tool made it.
type Receipt<'a> = Option<(&'a str, &'a str)>;

/// How long one turn may run, tool rounds and speech included.
const TURN_DEADLINE: Duration = Duration::from_mins(3);
/// How long a live-access check is trusted.
const LIVE_CHECK_TTL: Duration = Duration::from_secs(1);
/// Lifetime of the agent session the conversation signs for memory and access checks.
const AGENT_SESSION_SECONDS: i64 = 60;
/// Settings keep their newest versions; older ones are deleted on each write.
const KEPT_VERSIONS: i64 = 15;
const MAX_SPOKEN_ANSWER_BYTES: usize = 12_000;

/// Transaction options with a static name, which Rivet reports in errors and profiles.
const fn named(name: &'static str) -> TxOptions<'static> {
    TxOptions {
        name: Some(name),
        timeout: None,
    }
}

const SEED_PROMPT_SQL: &str = r"
    INSERT OR IGNORE INTO system_prompt(singleton, revision, prompt, updated)
    VALUES (1, 1, ?, ?)";
const SEED_PROMPT_VERSION_SQL: &str = r"
    INSERT OR IGNORE INTO prompt_versions(revision, prompt, reason, updated)
    SELECT revision, prompt, 'Initial Buddy persona', updated
    FROM system_prompt WHERE singleton = 1 AND revision = 1";
const SEED_VOICE_SQL: &str = r"
    INSERT OR IGNORE INTO voice_settings(singleton, revision, config, updated)
    VALUES (1, 1, ?, ?)";
const SEED_VOICE_VERSION_SQL: &str = r"
    INSERT OR IGNORE INTO voice_versions(revision, config, reason, updated)
    SELECT revision, config, 'Initial curated voice', updated
    FROM voice_settings WHERE singleton = 1 AND revision = 1";
const SEED_CONTEXT_SQL: &str = r"
    INSERT OR IGNORE INTO context_settings(singleton, revision, config, updated)
    VALUES (1, 1, ?, ?)";
const SEED_CONTEXT_VERSION_SQL: &str = r"
    INSERT OR IGNORE INTO context_versions(revision, config, reason, updated)
    SELECT revision, config, 'Initial UTC fallback', updated
    FROM context_settings WHERE singleton = 1 AND revision = 1";
const SEED_BUDDY_SQL: &str = "INSERT OR IGNORE INTO personas(name, latest) VALUES ('Buddy', 1)";
const SEED_BUDDY_VERSION_SQL: &str = r"
    INSERT OR IGNORE INTO persona_versions(name, version, prompt, voice, reason, updated)
    SELECT 'Buddy', 1, ?, ?, 'J.P. companion on Will', ?
    WHERE EXISTS(SELECT 1 FROM personas WHERE name = 'Buddy' AND latest = 1)";

/// A run checkpointed at `ready` sits between effects, so it resumes from its saved state.
const REQUEUE_READY_SQL: &str = r"
    UPDATE turns SET status = 'queued'
    WHERE status = 'running' AND id IN (SELECT turn_id FROM runs WHERE phase = 'ready')";
const RUNNING_TURNS_SQL: &str = "SELECT id FROM turns WHERE status = 'running'";

const FACTORY_PROMPT_SQL: &str = "SELECT revision, prompt FROM system_prompt WHERE singleton = 1";
const FACTORY_VOICE_SQL: &str = "SELECT revision, config FROM voice_settings WHERE singleton = 1";
const BUDDY_LATEST_SQL: &str = "SELECT latest FROM personas WHERE name = 'Buddy'";
const BUDDY_FIRST_SQL: &str =
    "SELECT prompt, voice FROM persona_versions WHERE name = 'Buddy' AND version = 1";
/// Compare-and-set on the factory text read just before, so a save that lands between the
/// read and this write is kept.
const ADOPT_BUDDY_SQL: &str = r"
    UPDATE persona_versions SET prompt = ?, voice = ?, reason = ?, updated = ?
    WHERE name = 'Buddy' AND version = 1 AND prompt = ? AND voice = ?
        AND EXISTS(SELECT 1 FROM personas WHERE name = 'Buddy' AND latest = 1)";

const INSERT_EVENT_SQL: &str = r"
    INSERT INTO events(turn_id, kind, payload) SELECT ?, ?, ?
    WHERE EXISTS(SELECT 1 FROM turns WHERE id = ? AND status = 'running')
    RETURNING sequence";
const QUEUED_SQL: &str = "SELECT id FROM turns WHERE status = 'queued' LIMIT 1";
const CLAIM_SQL: &str = r"
    UPDATE turns SET status = 'running'
    WHERE id = (SELECT id FROM turns WHERE status = 'queued' ORDER BY created LIMIT 1)
    RETURNING id, text";
const RUNNING_TURN_SQL: &str = "SELECT id FROM turns WHERE id = ? AND status = 'running'";
const RECEIPT_SQL: &str = "UPDATE tool_calls SET result = ? WHERE turn_id = ? AND call_id = ?";
const PARTIAL_ANSWER_SQL: &str = r"
    SELECT json_extract(payload, '$.text') FROM events
    WHERE turn_id = ? AND kind = 'text.delta'
    ORDER BY sequence";
const FINISH_SQL: &str = r"
    UPDATE turns SET status = ?, answer = ?
    WHERE id = ? AND status IN ('queued', 'running')
    RETURNING id";
const TERMINAL_EVENT_SQL: &str = "INSERT INTO events(turn_id, kind, payload) VALUES (?, ?, ?)";
const DELETE_RUN_SQL: &str = "DELETE FROM runs WHERE turn_id = ?";

const PRIOR_REQUEST_SQL: &str = "SELECT id, text FROM turns WHERE request_id = ?";
const BUSY_SQL: &str = "SELECT id FROM turns WHERE status IN ('queued', 'running') LIMIT 1";
const INSERT_TURN_SQL: &str =
    "INSERT INTO turns(id, request_id, text, created) VALUES (?, ?, ?, ?)";
const REPLAY_SQL: &str = r"
    SELECT sequence, turn_id, kind, payload FROM events
    WHERE sequence > ? ORDER BY sequence LIMIT 128";
const SNAPSHOT_SQL: &str = r"
    SELECT id, status, text, answer,
        EXISTS(SELECT 1 FROM speech_chunks c WHERE c.turn_id = turns.id)
    FROM turns ORDER BY id DESC LIMIT 20";
const LAST_SEQUENCE_SQL: &str = "SELECT COALESCE(MAX(sequence), 0) FROM events";
const ANSWER_SQL: &str = "SELECT answer FROM turns WHERE id = ? AND status = 'completed'";
const HISTORY_PAGE_SQL: &str = r"
    SELECT id, status, text, answer FROM turns
    WHERE (? = '' OR id < ?) ORDER BY id DESC LIMIT 20";

/// Starts, resumes or restarts the history search rebuild, or marks it done. Returns
/// whether batches remain. Runs in `create`, before any action.
async fn begin_history_backfill(ctx: &Ctx<Conversation>) -> Result<bool> {
    let begin = |tx: SqliteTransaction| async move {
        let rows = tx.execute(history::STATUS_SQL, None).await?.rows;
        let row = rows.first().context("history status missing")?;
        let flag = |index| integer(row, index).map(|value| value != 0);
        let decision = history::Begin::decide([flag(0)?, flag(1)?, flag(2)?, flag(3)?]);
        match decision {
            history::Begin::Done | history::Begin::Resume => {}
            history::Begin::Mark => {
                tx.execute(history::MARK_SQL, Some(vec![P::Integer(auth::now())]))
                    .await?;
            }
            history::Begin::Reset => {
                tx.exec(history::RESET_SQL).await?;
                tx.exec(history::START_SQL).await?;
            }
            history::Begin::Start => {
                tx.exec(history::START_SQL).await?;
            }
        }
        Ok(decision.pending())
    };
    let options = named("conversation.history_backfill_begin");
    ctx.sql().transaction(begin, options).await
}

/// One rebuild batch in one transaction. Returns whether batches remain.
async fn history_backfill_step(ctx: &Ctx<Conversation>) -> Result<bool> {
    let step = |tx: SqliteTransaction| async move {
        let rows = tx.execute(history::STATE_SQL, None).await?.rows;
        let Some(row) = rows.first() else {
            return Ok(false);
        };
        let floor = P::Integer(integer(row, 0)?);
        let cursor = string(row, 1)?;
        if integer(row, 2)? == 0 {
            let end = tx
                .execute(history::WINDOW_END_SQL, params(&[&cursor]))
                .await?
                .rows;
            let Some(end) = end.first().map(|row| string(row, 0)).transpose()? else {
                tx.execute(history::SWEEP_START_SQL, None).await?;
                return Ok(true);
            };
            let window = || vec![P::Text(cursor.clone()), P::Text(end.clone())];
            let mut unmap = window();
            unmap.push(floor);
            tx.execute(history::UNMAP_SQL, Some(unmap)).await?;
            tx.execute(history::MAP_SQL, Some(window())).await?;
            tx.execute(history::INDEX_SQL, Some(window())).await?;
            tx.execute(history::ADVANCE_SQL, params(&[&end])).await?;
            return Ok(true);
        }
        let swept = tx
            .execute(history::SWEEP_SEARCH_SQL, Some(vec![floor.clone()]))
            .await?
            .changes;
        let orphans = tx
            .execute(history::SWEEP_ROWS_SQL, Some(vec![floor]))
            .await?
            .changes;
        if swept + orphans > 0 {
            return Ok(true);
        }
        tx.exec(history::FINISH_SQL).await?;
        tx.execute(history::MARK_SQL, Some(vec![P::Integer(auth::now())]))
            .await?;
        Ok(false)
    };
    ctx.sql()
        .transaction(step, named("conversation.history_backfill"))
        .await
}

/// Seeds a singleton settings row and its first version; existing rows are kept.
async fn seed_settings(
    ctx: &Ctx<Conversation>,
    settings: &'static str,
    versions: &'static str,
    value: String,
) -> Result<()> {
    let seed = |tx: SqliteTransaction| async move {
        let row = vec![P::Text(value), P::Integer(auth::now())];
        tx.execute(settings, Some(row)).await?;
        tx.execute(versions, None).await?;
        Ok(())
    };
    ctx.sql()
        .transaction(seed, named("conversation.seed_settings"))
        .await
}

/// Seeds the Buddy persona at version 1 with the default prompt and voice.
async fn seed_buddy(ctx: &Ctx<Conversation>, voice: String) -> Result<()> {
    let seed = |tx: SqliteTransaction| async move {
        tx.execute(SEED_BUDDY_SQL, None).await?;
        let version = vec![
            P::Text(include_str!("prompts/persona.txt").into()),
            P::Text(voice),
            P::Integer(auth::now()),
        ];
        tx.execute(SEED_BUDDY_VERSION_SQL, Some(version)).await?;
        Ok(())
    };
    ctx.sql()
        .transaction(seed, named("conversation.seed_buddy"))
        .await
}

/// Fails a settings write made by a tool once its turn has stopped running.
async fn ensure_receipt_running(tx: &SqliteTransaction, receipt: Receipt<'_>) -> Result<()> {
    if let Some((turn, _)) = receipt {
        let rows = tx.execute(RUNNING_TURN_SQL, params(&[turn])).await?.rows;
        ensure!(!rows.is_empty(), "turn cancelled");
    }
    Ok(())
}

/// Records a tool's result in the same transaction as its write, so a recovered turn
/// cannot apply the write twice.
async fn write_receipt(tx: &SqliteTransaction, receipt: Receipt<'_>, result: &Value) -> Result<()> {
    if let Some((turn, call)) = receipt {
        let values = params(&[&result.to_string(), turn, call]);
        tx.execute(RECEIPT_SQL, values).await?;
    }
    Ok(())
}

/// Deletes the settings versions older than the kept window below `revision`.
async fn trim_versions(tx: &SqliteTransaction, sql: &str, revision: i64) -> Result<()> {
    let oldest = (revision - KEPT_VERSIONS).max(1);
    tx.execute(sql, Some(vec![P::Integer(oldest)])).await?;
    Ok(())
}

/// Runs the history search rebuild one batch at a time beside turns and actions. A stop
/// between batches loses nothing: the next start resumes from the cursor.
fn start_history_backfill(ctx: &Ctx<Conversation>) {
    let task_ctx = ctx.clone();
    ctx.register_task(async move {
        let _awake = task_ctx.keep_awake_region();
        let mut batches = 0_u32;
        while !task_ctx.aborted() {
            match history_backfill_step(&task_ctx).await {
                Ok(true) => batches += 1,
                Ok(false) => {
                    tracing::info!(batches, "history search rebuild finished");
                    return;
                }
                Err(error) => {
                    // The next start resumes from the committed cursor.
                    tracing::warn!(batches, %error, "history search rebuild paused");
                    return;
                }
            }
            // Queued actions and turn writes run between batches.
            tokio::task::yield_now().await;
        }
    });
}

/// Clears the drain flag when dropped, including during a panic, then runs `recheck`.
struct DrainGuard<'a, F: FnOnce()> {
    draining: &'a AtomicBool,
    recheck: Option<F>,
}

impl<F: FnOnce()> Drop for DrainGuard<'_, F> {
    fn drop(&mut self) {
        self.draining.store(false, Ordering::Release);
        if let Some(recheck) = self.recheck.take() {
            recheck();
        }
    }
}

#[async_trait]
impl Actor for Conversation {
    type Input = ();
    type Actions = (Call,);
    type Events = (OutputEvent,);
    type ConnParams = Connection;
    type Action = action::Raw;
    async fn create(ctx: &Ctx<Self>) -> Result<Self> {
        let signing_key = crate::config::signing_key()?;
        ctx.sql().exec(include_str!("conversation.sql")).await?;
        let history_backfill = begin_history_backfill(ctx).await?;
        let prompt = include_str!("prompts/persona.txt").to_owned();
        seed_settings(ctx, SEED_PROMPT_SQL, SEED_PROMPT_VERSION_SQL, prompt).await?;
        let default_voice = serde_json::to_string(&crate::voice::VoiceConfig::default())?;
        let voice = default_voice.clone();
        seed_settings(ctx, SEED_VOICE_SQL, SEED_VOICE_VERSION_SQL, voice).await?;
        seed_buddy(ctx, default_voice).await?;
        let context = serde_json::to_string(&crate::context::ContextConfig::default())?;
        seed_settings(ctx, SEED_CONTEXT_SQL, SEED_CONTEXT_VERSION_SQL, context).await?;
        Ok(Self {
            signing_key,
            history_backfill,
            draining: AtomicBool::new(false),
            model: Arc::new(Model::from_env()?),
            last_authorized: tokio::sync::Mutex::new(None),
            running: std::sync::Mutex::new(None),
            sessions: delivery::Sessions::default(),
        })
    }
    async fn on_start(self: Arc<Self>, ctx: Ctx<Self>) -> Result<()> {
        self.adopt_factory_companion(&ctx).await?;
        ctx.sql().execute(REQUEUE_READY_SQL, None).await?;
        // A provider stream cannot resume after process loss. Preserve its output and make
        // the interruption explicit.
        let interrupted = ctx.sql().query(RUNNING_TURNS_SQL, None).await?;
        for row in interrupted.rows {
            let payload = json!({ "reason": "server_restart" });
            let id = string(&row, 0)?;
            self.finish_turn(&ctx, &id, "interrupted", "", payload)
                .await?;
        }
        self.start_drain(&ctx);
        if self.history_backfill {
            start_history_backfill(&ctx);
        }
        Ok(())
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

impl Conversation {
    /// Replace an untouched factory seed with the J.P. companion and Will.
    /// Customized prompts, voices, and later persona versions stay as saved.
    async fn adopt_factory_companion(&self, ctx: &Ctx<Self>) -> Result<()> {
        let factory_prompt = include_str!("prompts/persona.factory-v1.txt");
        let prompt_rows = ctx.sql().query(FACTORY_PROMPT_SQL, None).await?.rows;
        if let Some(row) = prompt_rows.first()
            && integer(row, 0)? == 1
            && string(row, 1)? == factory_prompt
        {
            let prompt = include_str!("prompts/persona.txt");
            self.set_persona(ctx, 1, prompt, "Adopt J.P. companion wording", None)
                .await?;
        }
        let voice_rows = ctx.sql().query(FACTORY_VOICE_SQL, None).await?.rows;
        if let Some(row) = voice_rows.first()
            && integer(row, 0)? == 1
        {
            let config: crate::voice::VoiceConfig = serde_json::from_str(&string(row, 1)?)?;
            if config.is_factory_v1() {
                let will = crate::voice::VoiceConfig::default();
                self.set_voice(ctx, 1, &will, "Adopt Will on Eleven v4 Turbo", None)
                    .await?;
            }
        }
        let buddy = ctx.sql().query(BUDDY_LATEST_SQL, None).await?.rows;
        if buddy
            .first()
            .is_none_or(|row| integer(row, 0).ok() != Some(1))
        {
            return Ok(());
        }
        let versions = ctx.sql().query(BUDDY_FIRST_SQL, None).await?.rows;
        let Some(row) = versions.first() else {
            return Ok(());
        };
        let prompt = string(row, 0)?;
        let stored_voice = string(row, 1)?;
        let voice: crate::voice::VoiceConfig = serde_json::from_str(&stored_voice)?;
        if prompt == factory_prompt && voice.is_factory_v1() {
            let encoded = serde_json::to_string(&crate::voice::VoiceConfig::default())?;
            let values = vec![
                P::Text(include_str!("prompts/persona.txt").into()),
                P::Text(encoded),
                P::Text("J.P. companion on Will".into()),
                P::Integer(auth::now()),
                P::Text(prompt),
                P::Text(stored_voice),
            ];
            ctx.sql().execute(ADOPT_BUDDY_SQL, Some(values)).await?;
        }
        Ok(())
    }

    async fn authorize(
        &self,
        ctx: &Ctx<Self>,
        token: &str,
        role: Option<&str>,
    ) -> Result<auth::Session> {
        let session: auth::Session = auth::verify(&self.signing_key, "session", token)?;
        session.validate(&bot_key(ctx)?, role)?;
        crate::actor::check_access(ctx.client()?, &bot_key(ctx)?, token).await?;
        Ok(session)
    }

    /// A short agent session the conversation presents to the bot and memory actors.
    fn agent_token(&self, bot: &str) -> Result<String> {
        let session = auth::Session {
            bot_id: bot.to_owned(),
            role: "agent".into(),
            expires: auth::now() + AGENT_SESSION_SECONDS,
        };
        auth::sign(&self.signing_key, "session", &session)
    }

    async fn event(&self, ctx: &Ctx<Self>, turn: &str, kind: &str, payload: Value) -> Result<()> {
        let values = params(&[turn, kind, &payload.to_string(), turn]);
        let inserted = ctx.sql().execute(INSERT_EVENT_SQL, values).await?;
        let Some(row) = inserted.rows.first() else {
            return Ok(());
        };
        let event = OutputEvent {
            sequence: integer(row, 0)?,
            turn_id: turn.into(),
            kind: kind.into(),
            payload,
        };
        self.deliver(ctx, &event).await;
        Ok(())
    }

    fn start_drain(self: &Arc<Self>, ctx: &Ctx<Self>) {
        if self
            .draining
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let actor = self.clone();
        let task_ctx = ctx.clone();
        ctx.register_task(async move {
            // Submit also calls `start_drain` after its commit, which this drain may have
            // refused while it was finishing. The recheck closes that race.
            let recheck = || {
                if task_ctx.aborted() {
                    // `on_start` drains whatever stays queued on the next wake.
                    return;
                }
                let (actor, ctx) = (actor.clone(), task_ctx.clone());
                task_ctx.register_task(async move {
                    let queued = ctx.sql().query(QUEUED_SQL, None).await;
                    if queued.is_ok_and(|rows| !rows.rows.is_empty()) {
                        actor.start_drain(&ctx);
                    }
                });
            };
            let _guard = DrainGuard {
                draining: &actor.draining,
                recheck: Some(recheck),
            };
            let _awake = task_ctx.keep_awake_region();
            if let Err(e) = actor.drain(&task_ctx).await {
                tracing::error!(
                    transport_error = e.is::<reqwest::Error>(),
                    "agent drain failed"
                );
            }
        });
    }

    async fn drain(&self, ctx: &Ctx<Self>) -> Result<()> {
        loop {
            let rows = ctx.sql().query(CLAIM_SQL, None).await?.rows;
            let Some(row) = rows.first() else {
                return Ok(());
            };
            let id = string(row, 0)?;
            let text = string(row, 1)?;
            let stop = self.begin_turn_signal(&id);
            let result = self.run_turn(ctx, &id, &text, &stop).await;
            self.end_turn_signal(&id);
            let Some(result) = result else {
                return Ok(());
            };
            if let Err(error) = result {
                if stop.is_cancelled() {
                    // Already terminal: cancellation committed its own event.
                    continue;
                }
                tracing::warn!(
                    transport_error = error.is::<reqwest::Error>(),
                    "generation failed"
                );
                let payload = json!({
                    "message": "The assistant could not finish. Please try again.",
                });
                self.finish_turn(ctx, &id, "failed", "", payload).await?;
            }
        }
    }

    /// Runs one admitted turn under the execution deadline. `None` means the actor is
    /// stopping. A barge-in does not drop this future: it fires `stop`, which the model
    /// stream observes, so no `SQLite` transaction is abandoned mid-flight.
    async fn run_turn(
        &self,
        ctx: &Ctx<Self>,
        id: &str,
        text: &str,
        stop: &CancellationToken,
    ) -> Option<Result<()>> {
        let generation = async {
            self.event(ctx, id, "turn.started", json!({ "text": text }))
                .await?;
            self.generate(ctx, id, text, stop).await
        };
        let abort = ctx.abort_signal();
        tokio::select! {
            () = abort.cancelled() => None,
            result = tokio::time::timeout(TURN_DEADLINE, generation) => {
                Some(result.context("turn deadline exceeded").and_then(|v| v))
            }
        }
    }

    fn begin_turn_signal(&self, id: &str) -> CancellationToken {
        let token = CancellationToken::new();
        let mut running = self
            .running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *running = Some((id.into(), token.clone()));
        token
    }

    fn end_turn_signal(&self, id: &str) {
        let mut running = self
            .running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if running.as_ref().is_some_and(|(turn, _)| turn == id) {
            *running = None;
        }
    }

    /// Fires the stop signal of `id` if it is the running turn.
    fn stop_turn(&self, id: &str) {
        let running = self
            .running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((turn, token)) = running.as_ref()
            && turn == id
        {
            token.cancel();
        }
    }

    fn ensure_not_stopped(&self, id: &str) -> Result<()> {
        let running = self
            .running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let stopped = running
            .as_ref()
            .is_some_and(|(turn, token)| turn == id && token.is_cancelled());
        ensure!(!stopped, "turn cancelled");
        Ok(())
    }

    async fn check_live(&self, ctx: &Ctx<Self>) -> Result<()> {
        let mut checked = self.last_authorized.lock().await;
        if checked.is_some_and(|at| at.elapsed() < LIVE_CHECK_TTL) {
            return Ok(());
        }
        let bot = bot_key(ctx)?;
        let token = self.agent_token(&bot)?;
        crate::actor::check_access(ctx.client()?, &bot, &token).await?;
        *checked = Some(std::time::Instant::now());
        Ok(())
    }

    async fn active(&self, ctx: &Ctx<Self>, id: &str) -> Result<()> {
        self.ensure_not_stopped(id)?;
        self.check_live(ctx).await?;
        let rows = ctx.sql().query(RUNNING_TURN_SQL, params(&[id])).await?.rows;
        ensure!(!rows.is_empty(), "turn cancelled");
        Ok(())
    }

    async fn finish_turn(
        &self,
        ctx: &Ctx<Self>,
        id: &str,
        status: &str,
        answer: &str,
        payload: Value,
    ) -> Result<()> {
        let kind = format!("turn.{status}");
        let encoded = payload.to_string();
        let (kind_ref, encoded_ref) = (&kind, &encoded);
        let finish = move |tx: SqliteTransaction| async move {
            // A stopped turn keeps what it already streamed as its answer.
            let answer = if status == "completed" {
                answer.to_owned()
            } else {
                let rows = tx.execute(PARTIAL_ANSWER_SQL, params(&[id])).await?.rows;
                rows.iter()
                    .map(|r| string(r, 0))
                    .collect::<Result<Vec<_>>>()?
                    .join("")
            };
            let rows = tx
                .execute(FINISH_SQL, params(&[status, &answer, id]))
                .await?
                .rows;
            if rows.is_empty() {
                return Ok(None);
            }
            let inserted = tx
                .execute(TERMINAL_EVENT_SQL, params(&[id, kind_ref, encoded_ref]))
                .await?;
            tx.execute(DELETE_RUN_SQL, params(&[id])).await?;
            Ok(inserted.last_insert_row_id)
        };
        let changed = ctx
            .sql()
            .transaction(finish, named("conversation.finish_turn"))
            .await?;
        let Some(sequence) = changed else {
            return Ok(());
        };
        if status != "completed" {
            // Barge-in: drop the provider stream now instead of at the next storage check.
            self.stop_turn(id);
        }
        let event = OutputEvent {
            sequence,
            turn_id: id.into(),
            kind,
            payload,
        };
        self.deliver(ctx, &event).await;
        Ok(())
    }

    async fn memory(&self, ctx: &Ctx<Self>, command: Command) -> Result<Value> {
        let bot = bot_key(ctx)?;
        let token = self.agent_token(&bot)?;
        crate::memory::call(ctx.client()?, &bot, &token, command).await
    }

    async fn search_history(&self, ctx: &Ctx<Self>, query: &str) -> Result<Value> {
        ensure!(
            !query.is_empty() && query.len() <= 256,
            "invalid history query"
        );
        let escaped = query.replace('"', "\"\"");
        let phrase = format!("\"{escaped}\"");
        let rows = ctx
            .sql()
            .query(history::SEARCH_SQL, params(&[&phrase]))
            .await?
            .rows;
        let hits = rows
            .into_iter()
            .map(|r| {
                Ok(json!({
                    "turn_id": string(&r, 0)?,
                    "status": string(&r, 1)?,
                    "user": string(&r, 2)?,
                    "assistant": string(&r, 3)?,
                }))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(json!(hits))
    }
}

/// Roles that may change persona, voice and context settings.
fn ensure_settings_role(session: &auth::Session, what: &str) -> Result<()> {
    ensure!(
        ["device", "agent"].contains(&session.role.as_str()),
        "{what} update requires device or agent role"
    );
    Ok(())
}

fn turn_json(row: &[V]) -> Result<Value> {
    Ok(json!({
        "id": string(row, 0)?,
        "status": string(row, 1)?,
        "text": string(row, 2)?,
        "answer": string(row, 3)?,
    }))
}

impl Conversation {
    async fn submit(&self, ctx: &Ctx<Self>, request_id: String, text: String) -> Result<Value> {
        ensure!(
            !request_id.is_empty()
                && request_id.len() <= 128
                && !text.trim().is_empty()
                && text.len() <= 8192,
            "invalid turn"
        );
        let id = uuid::Uuid::now_v7().to_string();
        let created = auth::now();
        let submit = |tx: SqliteTransaction| async move {
            let prior = tx
                .execute(PRIOR_REQUEST_SQL, params(&[&request_id]))
                .await?
                .rows;
            if let Some(row) = prior.first() {
                ensure!(
                    string(row, 1)? == text,
                    "request id reused with different text"
                );
                return Ok(json!({ "turn_id": string(row, 0)?, "duplicate": true }));
            }
            let busy = tx.execute(BUSY_SQL, None).await?;
            ensure!(busy.rows.is_empty(), "busy");
            let turn = vec![
                P::Text(id.clone()),
                P::Text(request_id),
                P::Text(text),
                P::Integer(created),
            ];
            tx.execute(INSERT_TURN_SQL, Some(turn)).await?;
            Ok(json!({ "turn_id": id, "duplicate": false }))
        };
        ctx.sql()
            .transaction(submit, named("conversation.submit"))
            .await
    }

    async fn replay(&self, ctx: &Ctx<Self>, after: i64) -> Result<Value> {
        let cursor = Some(vec![P::Integer(after.max(0))]);
        let rows = ctx.sql().query(REPLAY_SQL, cursor).await?.rows;
        let events = rows
            .into_iter()
            .map(|r| {
                Ok(OutputEvent {
                    sequence: integer(&r, 0)?,
                    turn_id: string(&r, 1)?,
                    kind: string(&r, 2)?,
                    payload: serde_json::from_str(&string(&r, 3)?)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(serde_json::to_value(events)?)
    }

    async fn snapshot(&self, ctx: &Ctx<Self>) -> Result<Value> {
        let read = |tx: SqliteTransaction| async move {
            let rows = tx.execute(SNAPSHOT_SQL, None).await?.rows;
            let turns = rows
                .into_iter()
                .map(|r| {
                    let mut turn = turn_json(&r)?;
                    turn["speech_ready"] = json!(integer(&r, 4)? != 0);
                    Ok(turn)
                })
                .collect::<Result<Vec<_>>>()?;
            let rows = tx.execute(LAST_SEQUENCE_SQL, None).await?.rows;
            let last = integer(rows.first().context("sequence missing")?, 0)?;
            Ok(json!({ "turns": turns, "last_sequence": last }))
        };
        ctx.sql()
            .transaction(read, named("conversation.snapshot"))
            .await
    }

    async fn spoken_answer(&self, ctx: &Ctx<Self>, turn_id: String) -> Result<Value> {
        ensure!(turn_id.len() <= 128, "invalid turn id");
        let rows = ctx.sql().query(ANSWER_SQL, params(&[&turn_id])).await?.rows;
        let row = rows
            .first()
            .context("only completed answers can be spoken")?;
        let answer = string(row, 0)?;
        ensure!(
            !answer.trim().is_empty() && answer.len() <= MAX_SPOKEN_ANSWER_BYTES,
            "answer outside speech budget"
        );
        let voice = self.speech_voice(ctx, &turn_id).await?;
        Ok(json!({ "turn_id": turn_id, "text": answer, "voice": voice }))
    }

    async fn history_page(&self, ctx: &Ctx<Self>, before: String) -> Result<Value> {
        let cursor = params(&[&before, &before]);
        let rows = ctx.sql().query(HISTORY_PAGE_SQL, cursor).await?.rows;
        let turns = rows
            .iter()
            .map(|r| turn_json(r))
            .collect::<Result<Vec<_>>>()?;
        Ok(json!(turns))
    }
}

impl Handler<Call> for Conversation {
    async fn handle(self: Arc<Self>, ctx: Ctx<Self>, call: Call) -> Result<Value> {
        let session = self.authorize(&ctx, &call.session, None).await?;
        match call.command {
            Command::Submit { request_id, text } => {
                ensure!(session.role == "device", "only device may submit");
                let output = self.submit(&ctx, request_id, text).await?;
                self.start_drain(&ctx);
                Ok(output)
            }
            Command::Cancel { turn_id } => {
                self.finish_turn(&ctx, &turn_id, "cancelled", "", json!({}))
                    .await?;
                Ok(json!({ "ok": true }))
            }
            Command::Replay { after } => self.replay(&ctx, after).await,
            Command::Snapshot => self.snapshot(&ctx).await,
            Command::ListPersonas => self.list_personas(&ctx).await,
            Command::GetPersona { name, version } => self.get_persona(&ctx, &name, version).await,
            Command::SavePersona { persona } => {
                ensure_settings_role(&session, "persona")?;
                self.save_profile(&ctx, &persona, None).await
            }
            Command::ActivatePersona {
                name,
                version,
                expected_prompt_revision,
                expected_voice_revision,
            } => {
                ensure_settings_role(&session, "persona")?;
                let expected = (expected_prompt_revision, expected_voice_revision);
                self.activate_profile(&ctx, &name, version, expected, None)
                    .await
            }
            Command::GetContextConfig => self.context_config(&ctx).await,
            Command::SetContextConfig {
                expected_revision,
                config,
                reason,
            } => {
                ensure_settings_role(&session, "context")?;
                self.set_context(&ctx, expected_revision, &config, &reason, None)
                    .await
            }
            Command::GetVoiceConfig => self.voice_config(&ctx).await,
            Command::SetVoiceConfig {
                expected_revision,
                config,
                reason,
            } => {
                ensure_settings_role(&session, "voice")?;
                self.set_voice(&ctx, expected_revision, &config, &reason, None)
                    .await
            }
            Command::GetSystemPrompt => self.persona(&ctx).await,
            Command::SystemPromptHistory => self.persona_history(&ctx).await,
            Command::SetSystemPrompt {
                expected_revision,
                prompt,
                reason,
            } => {
                ensure_settings_role(&session, "prompt")?;
                self.set_persona(&ctx, expected_revision, &prompt, &reason, None)
                    .await
            }
            Command::SpeechChunk { turn_id, index } => {
                ensure!(session.role == "device", "speech requires device role");
                self.speech_chunk(&ctx, &turn_id, index).await
            }
            Command::SpokenAnswer { turn_id } => {
                ensure!(session.role == "device", "speech requires device role");
                self.spoken_answer(&ctx, turn_id).await
            }
            Command::History { before } => self.history_page(&ctx, before).await,
            Command::SearchHistory { query } => self.search_history(&ctx, &query).await,
            _ => bail!("command is not owned by conversation actor"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{AssertUnwindSafe, catch_unwind};

    #[test]
    fn drain_guard_clears_the_flag_and_rechecks_on_panic() {
        let draining = AtomicBool::new(true);
        let rechecked = AtomicBool::new(false);
        let unwound = catch_unwind(AssertUnwindSafe(|| {
            let _guard = DrainGuard {
                draining: &draining,
                recheck: Some(|| rechecked.store(true, Ordering::SeqCst)),
            };
            panic!("drain panicked");
        }));
        assert!(unwound.is_err());
        assert!(!draining.load(Ordering::Acquire));
        assert!(rechecked.load(Ordering::SeqCst));
    }

    #[test]
    fn factory_adoption_keeps_a_persona_saved_after_the_read() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch(include_str!("conversation.sql")).unwrap();
        db.execute(SEED_BUDDY_SQL, []).unwrap();
        let seed = rusqlite::params!["factory prompt", "factory voice", 0];
        db.execute(SEED_BUDDY_VERSION_SQL, seed).unwrap();
        let adopt = |old_prompt: &str| {
            let values =
                rusqlite::params!["companion", "will", "adopt", 1, old_prompt, "factory voice"];
            db.execute(ADOPT_BUDDY_SQL, values).unwrap()
        };
        // A save landed after the factory text was read: the stale adoption writes nothing.
        db.execute(
            "UPDATE persona_versions SET prompt='saved by owner' WHERE name='Buddy'",
            [],
        )
        .unwrap();
        assert_eq!(adopt("factory prompt"), 0);
        db.execute(
            "UPDATE persona_versions SET prompt='factory prompt' WHERE name='Buddy'",
            [],
        )
        .unwrap();
        // A newer version made the seed inactive: also untouched.
        db.execute("UPDATE personas SET latest=2 WHERE name='Buddy'", [])
            .unwrap();
        assert_eq!(adopt("factory prompt"), 0);
        db.execute("UPDATE personas SET latest=1 WHERE name='Buddy'", [])
            .unwrap();
        assert_eq!(adopt("factory prompt"), 1);
        let prompt: String = db.query_row(BUDDY_FIRST_SQL, [], |r| r.get(0)).unwrap();
        assert_eq!(prompt, "companion");
    }

    #[test]
    fn drain_guard_clears_the_flag_on_return() {
        let draining = AtomicBool::new(true);
        drop(DrainGuard {
            draining: &draining,
            recheck: None::<fn()>,
        });
        assert!(!draining.load(Ordering::Acquire));
    }
}

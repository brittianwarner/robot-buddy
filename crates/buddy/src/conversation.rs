//! Conversation transcript and Rig run ownership, independent of device identity and memory.
mod context;
mod delivery;
mod harness;
pub mod history;
mod persona;
mod profiles;
mod retention;
mod speech;
mod tools;
mod voice;
use crate::{
    auth,
    db::{BATCH_TX_TIMEOUT, Db, Tx, bot_key, integer, named, named_for, params, string},
    model::{Delta, Model, ToolCall},
    protocol::{BUSY_ERROR, Busy, Call, Command, Connection, OutputEvent},
};
use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;
use rivetkit::{
    BindParam as P, ColumnValue as V, SqliteDbExt, SqliteTransaction, action, prelude::*,
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

pub struct Conversation {
    signing_key: auth::Secret<Vec<u8>>,
    /// Batches of the history search rebuild remain; see [`history`].
    history_backfill: bool,
    draining: AtomicBool,
    /// Shared so a model stream can run on its own task.
    model: Arc<Model>,
    /// When the latest access check that passed began; see [`LIVE_CHECK_TTL`].
    last_authorized: std::sync::Mutex<Option<Instant>>,
    /// In-process stop signal for the running turn. A terminal `finish_turn` fires it so
    /// a provider stream drops at once; so do the turn deadline and actor shutdown. Durable
    /// turn status stays the source of truth.
    running: std::sync::Mutex<Option<(String, CancellationToken)>>,
    /// Verified session expiry per connection, so events do not re-verify each session.
    sessions: delivery::Sessions,
    /// A `start_drain` found a drain running. The drain claims again before it stops.
    wake: AtomicBool,
    /// Drains that failed in a row, for the bounded retry backoff.
    drain_failures: AtomicU32,
    /// A failed turn could not be marked failed. The next drain fails it first.
    orphaned: AtomicBool,
    /// Turns finished since the last retention pass.
    since_prune: AtomicU32,
    /// The schema, seeds and factory adoption are current; `on_start` skips adoption.
    schema_current: bool,
    /// `PRAGMA user_version` is below [`SCHEMA_VERSION`]; `on_start` raises it.
    mark_schema: bool,
}

/// The turn and tool call a settings write records its result for, when a tool made it.
type Receipt<'a> = Option<(&'a str, &'a str)>;

/// How long one turn may run, tool rounds and speech included.
const TURN_DEADLINE: Duration = Duration::from_mins(3);
/// After the deadline or shutdown fires a turn's stop signal, how long it may take to
/// reach its next stop check. Only a turn stuck past this is dropped.
const HALT_GRACE: Duration = Duration::from_secs(2);
/// How long a passed access check is trusted by in-process checks, speech polling and a
/// subscription right after its connect. Every other device action and every connect
/// checks the bot actor fresh, so a revoke fails them at once.
const LIVE_CHECK_TTL: Duration = Duration::from_secs(1);
/// Retries of a failed drain: 100 ms, then doubling, then the next submit or wake.
const DRAIN_RETRIES: u32 = 5;
const DRAIN_RETRY_BASE: Duration = Duration::from_millis(100);
/// Attempts to mark a failed turn failed before the next drain must reconcile it.
const FINISH_ATTEMPTS: u32 = 3;
/// `PRAGMA user_version` once this code's schema, seeds and factory adoption are applied.
const SCHEMA_VERSION: i64 = 1;
/// The schema version and whether a trigger of an older layout is present. Both readable
/// before any table exists.
const SCHEMA_STATE_SQL: &str = r"
    SELECT (SELECT user_version FROM pragma_user_version),
        EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'trigger' AND name IN (
            'turns_history_insert', 'turns_history_update',
            'turns_search_insert', 'turns_search_update'))";
const MARK_SCHEMA_SQL: &str = "PRAGMA user_version = 1";
/// Lifetime of the agent session the conversation signs for memory and access checks.
const AGENT_SESSION_SECONDS: i64 = 60;
/// Settings keep their newest versions; older ones are deleted on each write.
const KEPT_VERSIONS: i64 = 15;
const MAX_SPOKEN_ANSWER_BYTES: usize = 12_000;

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
/// At most 128 events and [`REPLAY_PAGE_BYTES`] of payload, counting 64 bytes of framing
/// per event, but always the first event: a client pages until a page is empty.
const REPLAY_SQL: &str = r"
    SELECT sequence, turn_id, kind, payload FROM (
        SELECT sequence, turn_id, kind, payload,
            row_number() OVER (ORDER BY sequence) AS n,
            sum(length(CAST(payload AS BLOB)) + 64) OVER (ORDER BY sequence) AS bytes
        FROM (SELECT * FROM events WHERE sequence > ? ORDER BY sequence LIMIT 128))
    WHERE n = 1 OR bytes <= ? ORDER BY sequence";
/// Half of rivetkit's 1 MiB outgoing message limit, leaving room for encoding.
const REPLAY_PAGE_BYTES: i64 = 512 * 1024;
const SNAPSHOT_SQL: &str = r"
    SELECT id, status, text, answer,
        EXISTS(SELECT 1 FROM speech_chunks c WHERE c.turn_id = turns.id), request_id
    FROM turns ORDER BY id DESC LIMIT 20";
const LAST_SEQUENCE_SQL: &str = "SELECT COALESCE(MAX(sequence), 0) FROM events";
const ANSWER_SQL: &str = "SELECT answer FROM turns WHERE id = ? AND status = 'completed'";
const HISTORY_PAGE_SQL: &str = r"
    SELECT id, status, text, answer FROM turns
    WHERE (? = '' OR id < ?) ORDER BY id DESC LIMIT 20";

/// Starts, resumes or restarts the history search rebuild, or marks it done. Returns
/// whether batches remain. Runs in `create`, before any action.
async fn begin_history_backfill(ctx: &Ctx<Conversation>) -> Result<bool> {
    let begin = |tx: Tx| async move {
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
    ctx.db().transaction(begin, options).await
}

/// One rebuild batch in one transaction. Returns whether batches remain.
async fn history_backfill_step(ctx: &Ctx<Conversation>) -> Result<bool> {
    let step = |tx: Tx| async move {
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
    ctx.db()
        .transaction(
            step,
            named_for("conversation.history_backfill", BATCH_TX_TIMEOUT),
        )
        .await
}

/// Seeds a singleton settings row and its first version; existing rows are kept.
async fn seed_settings(
    ctx: &Ctx<Conversation>,
    settings: &'static str,
    versions: &'static str,
    value: String,
) -> Result<()> {
    let seed = |tx: Tx| async move {
        let row = vec![P::Text(value), P::Integer(auth::now())];
        tx.execute(settings, Some(row)).await?;
        tx.execute(versions, None).await?;
        Ok(())
    };
    ctx.db()
        .transaction(seed, named("conversation.seed_settings"))
        .await
}

/// Seeds every settings singleton and the Buddy persona; existing rows are kept.
async fn seed_all(ctx: &Ctx<Conversation>) -> Result<()> {
    let prompt = include_str!("prompts/persona.txt").to_owned();
    seed_settings(ctx, SEED_PROMPT_SQL, SEED_PROMPT_VERSION_SQL, prompt).await?;
    let default_voice = serde_json::to_string(&crate::voice::VoiceConfig::default())?;
    let voice = default_voice.clone();
    seed_settings(ctx, SEED_VOICE_SQL, SEED_VOICE_VERSION_SQL, voice).await?;
    seed_buddy(ctx, default_voice).await?;
    let context = serde_json::to_string(&crate::context::ContextConfig::default())?;
    seed_settings(ctx, SEED_CONTEXT_SQL, SEED_CONTEXT_VERSION_SQL, context).await
}

/// Seeds the Buddy persona at version 1 with the default prompt and voice.
async fn seed_buddy(ctx: &Ctx<Conversation>, voice: String) -> Result<()> {
    let seed = |tx: Tx| async move {
        tx.execute(SEED_BUDDY_SQL, None).await?;
        let version = vec![
            P::Text(include_str!("prompts/persona.txt").into()),
            P::Text(voice),
            P::Integer(auth::now()),
        ];
        tx.execute(SEED_BUDDY_VERSION_SQL, Some(version)).await?;
        Ok(())
    };
    ctx.db()
        .transaction(seed, named("conversation.seed_buddy"))
        .await
}

/// Fails a settings write made by a tool once its turn has stopped running.
async fn ensure_receipt_running(tx: &Tx, receipt: Receipt<'_>) -> Result<()> {
    if let Some((turn, _)) = receipt {
        let rows = tx.execute(RUNNING_TURN_SQL, params(&[turn])).await?.rows;
        ensure!(!rows.is_empty(), "turn cancelled");
    }
    Ok(())
}

/// Records a tool's result in the same transaction as its write, so a recovered turn
/// cannot apply the write twice.
async fn write_receipt(tx: &Tx, receipt: Receipt<'_>, result: &Value) -> Result<()> {
    if let Some((turn, call)) = receipt {
        let values = params(&[&result.to_string(), turn, call]);
        tx.execute(RECEIPT_SQL, values).await?;
    }
    Ok(())
}

/// Deletes the settings versions older than the kept window below `revision`.
async fn trim_versions(tx: &Tx, sql: &str, revision: i64) -> Result<()> {
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

/// Why a turn was stopped by the runtime rather than by a user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Halt {
    Deadline,
    Abort,
}

/// Whether an action's access check may use the [`LIVE_CHECK_TTL`] cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Check {
    Fresh,
    Cached,
}

/// Clears the drain flag when dropped, including during a panic, then runs `recheck`.
struct DrainGuard<'a, F: FnOnce()> {
    draining: &'a AtomicBool,
    recheck: Option<F>,
}

impl<F: FnOnce()> Drop for DrainGuard<'_, F> {
    fn drop(&mut self) {
        self.draining.store(false, Ordering::SeqCst);
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
        // A wake of a current database skips the schema, the seeds and factory adoption.
        // Older code never sets the version, and any older trigger forces the full path,
        // which is idempotent, so a rolling deploy in either direction stays correct.
        let rows = ctx.db().query(SCHEMA_STATE_SQL, None).await?.rows;
        let row = rows.first().context("schema state missing")?;
        let version = integer(row, 0)?;
        let schema_current = version >= SCHEMA_VERSION && integer(row, 1)? == 0;
        if !schema_current {
            ctx.db().exec(include_str!("conversation.sql")).await?;
        }
        let history_backfill = begin_history_backfill(ctx).await?;
        if !schema_current {
            seed_all(ctx).await?;
        }
        Ok(Self {
            signing_key,
            history_backfill,
            draining: AtomicBool::new(false),
            model: Arc::new(Model::from_env()?),
            last_authorized: std::sync::Mutex::new(None),
            running: std::sync::Mutex::new(None),
            sessions: delivery::Sessions::default(),
            wake: AtomicBool::new(false),
            drain_failures: AtomicU32::new(0),
            orphaned: AtomicBool::new(false),
            since_prune: AtomicU32::new(0),
            schema_current,
            mark_schema: version < SCHEMA_VERSION,
        })
    }
    async fn on_start(self: Arc<Self>, ctx: Ctx<Self>) -> Result<()> {
        if !self.schema_current {
            self.adopt_factory_companion(&ctx).await?;
        }
        if self.mark_schema {
            // Never lowers a version newer code wrote.
            ctx.db().exec(MARK_SCHEMA_SQL).await?;
        }
        ctx.db().execute(REQUEUE_READY_SQL, None).await?;
        // A provider stream cannot resume after process loss. Preserve its output and make
        // the interruption explicit.
        let interrupted = ctx.db().query(RUNNING_TURNS_SQL, None).await?;
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
        self.authorize(&ctx, &p.session, None, Check::Fresh)
            .await
            .map(|_| ())
    }
    async fn on_subscribe(
        self: Arc<Self>,
        ctx: Ctx<Self>,
        conn: ConnCtx<Self>,
        event: String,
    ) -> Result<()> {
        // `on_before_connect` checked access fresh just before; see `LIVE_CHECK_TTL`.
        self.authorize(&ctx, &conn.params()?.session, None, Check::Cached)
            .await?;
        if event == OutputEvent::NAME {
            self.sessions.subscribed(conn.id());
        }
        Ok(())
    }
}

impl Conversation {
    /// Replace an untouched factory seed with the J.P. companion and Will.
    /// Customized prompts, voices, and later persona versions stay as saved.
    async fn adopt_factory_companion(&self, ctx: &Ctx<Self>) -> Result<()> {
        let factory_prompt = include_str!("prompts/persona.factory-v1.txt");
        let prompt_rows = ctx.db().query(FACTORY_PROMPT_SQL, None).await?.rows;
        if let Some(row) = prompt_rows.first()
            && integer(row, 0)? == 1
            && string(row, 1)? == factory_prompt
        {
            let prompt = include_str!("prompts/persona.txt");
            self.set_persona(ctx, 1, prompt, "Adopt J.P. companion wording", None)
                .await?;
        }
        let voice_rows = ctx.db().query(FACTORY_VOICE_SQL, None).await?.rows;
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
        let buddy = ctx.db().query(BUDDY_LATEST_SQL, None).await?.rows;
        if buddy
            .first()
            .is_none_or(|row| integer(row, 0).ok() != Some(1))
        {
            return Ok(());
        }
        let versions = ctx.db().query(BUDDY_FIRST_SQL, None).await?.rows;
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
            ctx.db().execute(ADOPT_BUDDY_SQL, Some(values)).await?;
        }
        Ok(())
    }

    async fn authorize(
        &self,
        ctx: &Ctx<Self>,
        token: &str,
        role: Option<&str>,
        check: Check,
    ) -> Result<auth::Session> {
        let session: auth::Session = auth::verify(&self.signing_key, "session", token)?;
        session.validate(&bot_key(ctx)?, role)?;
        if check == Check::Fresh || !self.recently_authorized() {
            self.check_access(ctx, token).await?;
        }
        Ok(session)
    }

    /// Whether an access check that passed began less than [`LIVE_CHECK_TTL`] ago.
    fn recently_authorized(&self) -> bool {
        self.last_authorized
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some_and(|at| at.elapsed() < LIVE_CHECK_TTL)
    }

    /// Asks the bot actor, then records the outcome: a pass dates the cache from when the
    /// check began, any failure clears it.
    async fn check_access(&self, ctx: &Ctx<Self>, token: &str) -> Result<()> {
        let began = Instant::now();
        let result = crate::actor::check_access(ctx.client()?, &bot_key(ctx)?, token).await;
        let mut last = self
            .last_authorized
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if result.is_ok() {
            if last.is_none_or(|at| at < began) {
                *last = Some(began);
            }
        } else {
            *last = None;
        }
        result
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
        let inserted = ctx.db().execute(INSERT_EVENT_SQL, values).await?;
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

    /// Starts the drain task, or tells the running one to claim again before it stops.
    fn start_drain(self: &Arc<Self>, ctx: &Ctx<Self>) {
        // `wake` is set before `draining` is read again, and the drain clears `draining`
        // before it reads `wake`, so a request is never lost between the two.
        loop {
            if self
                .draining
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                break;
            }
            self.wake.store(true, Ordering::SeqCst);
            if self.draining.load(Ordering::SeqCst) {
                return;
            }
        }
        // This drain's first claim covers every request made so far.
        self.wake.store(false, Ordering::SeqCst);
        let actor = self.clone();
        let task_ctx = ctx.clone();
        ctx.register_task(async move {
            let recheck = || actor.after_drain(&task_ctx);
            let _guard = DrainGuard {
                draining: &actor.draining,
                recheck: Some(recheck),
            };
            let _awake = task_ctx.keep_awake_region();
            match actor.drain(&task_ctx).await {
                Ok(()) => actor.drain_failures.store(0, Ordering::SeqCst),
                Err(e) => {
                    let failures = actor.drain_failures.fetch_add(1, Ordering::SeqCst) + 1;
                    tracing::error!(
                        failures,
                        transport_error = e.is::<reqwest::Error>(),
                        "agent drain failed"
                    );
                }
            }
        });
    }

    /// Runs after a drain clears its flag: retries a failed drain with a bounded backoff,
    /// or starts another drain for a request that arrived while this one finished.
    fn after_drain(self: &Arc<Self>, ctx: &Ctx<Self>) {
        if ctx.aborted() {
            // `on_start` drains whatever stays queued on the next wake.
            return;
        }
        let failures = self.drain_failures.load(Ordering::SeqCst);
        if failures == 0 {
            if self.wake.swap(false, Ordering::SeqCst) {
                self.start_drain(ctx);
            }
            return;
        }
        if failures > DRAIN_RETRIES {
            // A submit starts a drain even while busy, so a stuck turn is retried then.
            tracing::error!(failures, "agent drain retries exhausted");
            return;
        }
        let delay = DRAIN_RETRY_BASE * 2_u32.pow(failures - 1);
        let (actor, task_ctx) = (self.clone(), ctx.clone());
        ctx.register_task(async move {
            tokio::time::sleep(delay).await;
            if !task_ctx.aborted() {
                actor.start_drain(&task_ctx);
            }
        });
    }

    async fn drain(&self, ctx: &Ctx<Self>) -> Result<()> {
        if self.orphaned.swap(false, Ordering::SeqCst)
            && let Err(error) = self.fail_orphans(ctx).await
        {
            self.orphaned.store(true, Ordering::SeqCst);
            return Err(error);
        }
        loop {
            let Some(claimed) = self.claim(ctx).await? else {
                return Ok(());
            };
            let id = claimed.id.clone();
            if let Some(sequence) = claimed.started {
                let event = OutputEvent {
                    sequence,
                    turn_id: id.clone(),
                    kind: "turn.started".into(),
                    payload: json!({ "text": claimed.text }),
                };
                self.deliver(ctx, &event).await;
            }
            let stop = self.begin_turn_signal(&id);
            let (result, halt) = self.run_turn(ctx, &claimed, &stop).await;
            self.end_turn_signal(&id);
            match (result, halt) {
                // Stopping: `on_start` requeues or interrupts the turn on the next wake.
                (_, Some(Halt::Abort)) => return Ok(()),
                (Ok(()), _) => {}
                // A cancel already committed the terminal event.
                (Err(_), None) if stop.is_cancelled() => {}
                (Err(error), halt) => {
                    tracing::warn!(
                        transport_error = error.is::<reqwest::Error>(),
                        deadline = halt.is_some(),
                        "generation failed"
                    );
                    self.fail_turn(ctx, &id).await?;
                }
            }
            self.prune_after_turn(ctx);
            if !self.wake.swap(false, Ordering::SeqCst) {
                return Ok(());
            }
        }
    }

    /// Marks a turn failed, retrying briefly. If every attempt fails the turn would stay
    /// `running`, so the next drain fails it before claiming.
    async fn fail_turn(&self, ctx: &Ctx<Self>, id: &str) -> Result<()> {
        let payload = json!({ "message": "The assistant could not finish. Please try again." });
        let mut attempt = 1;
        loop {
            match self
                .finish_turn(ctx, id, "failed", "", payload.clone())
                .await
            {
                Ok(()) => return Ok(()),
                Err(error) if attempt >= FINISH_ATTEMPTS => {
                    self.orphaned.store(true, Ordering::SeqCst);
                    return Err(error);
                }
                Err(_) => {
                    tokio::time::sleep(DRAIN_RETRY_BASE * attempt).await;
                    attempt += 1;
                }
            }
        }
    }

    /// Fails turns left `running` by a failed `fail_turn`. No turn runs during a drain.
    async fn fail_orphans(&self, ctx: &Ctx<Self>) -> Result<()> {
        let rows = ctx.db().query(RUNNING_TURNS_SQL, None).await?.rows;
        for row in rows {
            let payload = json!({ "message": "The assistant could not finish. Please try again." });
            self.finish_turn(ctx, &string(&row, 0)?, "failed", "", payload)
                .await?;
        }
        Ok(())
    }

    /// Runs one claimed turn under the execution deadline. A barge-in, the deadline and
    /// actor shutdown all fire `stop`, which the turn observes at its next await point, so
    /// no `SQLite` transaction is abandoned mid-flight. Only a turn that does not stop
    /// within [`HALT_GRACE`] is dropped.
    async fn run_turn(
        &self,
        ctx: &Ctx<Self>,
        claimed: &harness::Claimed,
        stop: &CancellationToken,
    ) -> (Result<()>, Option<Halt>) {
        let generation = self.generate(ctx, claimed, stop);
        tokio::pin!(generation);
        let deadline = tokio::time::sleep(TURN_DEADLINE);
        tokio::pin!(deadline);
        let abort = ctx.abort_signal();
        let halt = tokio::select! {
            result = &mut generation => return (result, None),
            () = &mut deadline => Halt::Deadline,
            () = abort.cancelled() => Halt::Abort,
        };
        stop.cancel();
        let result = tokio::time::timeout(HALT_GRACE, generation)
            .await
            .unwrap_or_else(|_| Err(anyhow::anyhow!("turn did not stop in time")));
        (result, Some(halt))
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

    /// The access check of active generation and delivery, cached for [`LIVE_CHECK_TTL`].
    async fn check_live(&self, ctx: &Ctx<Self>) -> Result<()> {
        if self.recently_authorized() {
            return Ok(());
        }
        let token = self.agent_token(&bot_key(ctx)?)?;
        self.check_access(ctx, &token).await
    }

    /// Runs network work of the turn `id` until it finishes or the turn's stop signal
    /// fires. Only for work that holds no transaction: it is dropped when stopped.
    async fn interruptible<T>(&self, id: &str, work: impl Future<Output = Result<T>>) -> Result<T> {
        let stop = self
            .running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .filter(|(turn, _)| turn == id)
            .map(|(_, stop)| stop.clone());
        let Some(stop) = stop else {
            return work.await;
        };
        tokio::select! {
            biased;
            () = stop.cancelled() => bail!("turn cancelled"),
            result = work => result,
        }
    }

    async fn active(&self, ctx: &Ctx<Self>, id: &str) -> Result<()> {
        self.ensure_not_stopped(id)?;
        self.check_live(ctx).await?;
        let rows = ctx.db().query(RUNNING_TURN_SQL, params(&[id])).await?.rows;
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
        let finish = move |tx: Tx| async move {
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
            .db()
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
            .db()
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

/// A submit refused while `turn_id` is open: a public `user/busy` action error
/// carrying [`Busy`], so the device can cancel that turn and submit again.
fn busy_error(turn_id: String) -> anyhow::Error {
    let (group, code) = BUSY_ERROR;
    let message = "Another turn is still open.";
    anyhow::Error::new(rivet_error::RivetError {
        kind: rivet_error::RivetErrorKind::Dynamic {
            group: group.into(),
            code: code.into(),
            default_message: message.into(),
        },
        meta: serde_json::value::to_raw_value(&Busy { turn_id }).ok(),
        message: Some(message.into()),
        actor: None,
    })
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
        let submit = |tx: Tx| async move {
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
            let busy = tx.execute(BUSY_SQL, None).await?.rows;
            if let Some(row) = busy.first() {
                return Err(busy_error(string(row, 0)?));
            }
            let turn = vec![
                P::Text(id.clone()),
                P::Text(request_id),
                P::Text(text),
                P::Integer(created),
            ];
            tx.execute(INSERT_TURN_SQL, Some(turn)).await?;
            Ok(json!({ "turn_id": id, "duplicate": false }))
        };
        ctx.db()
            .transaction(submit, named("conversation.submit"))
            .await
    }

    async fn replay(&self, ctx: &Ctx<Self>, after: i64) -> Result<Value> {
        let cursor = Some(vec![
            P::Integer(after.max(0)),
            P::Integer(REPLAY_PAGE_BYTES),
        ]);
        let rows = ctx.db().query(REPLAY_SQL, cursor).await?.rows;
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
        let read = |tx: Tx| async move {
            let rows = tx.execute(SNAPSHOT_SQL, None).await?.rows;
            let turns = rows
                .into_iter()
                .map(|r| {
                    let mut turn = turn_json(&r)?;
                    turn["speech_ready"] = json!(integer(&r, 4)? != 0);
                    turn["request_id"] = json!(string(&r, 5)?);
                    Ok(turn)
                })
                .collect::<Result<Vec<_>>>()?;
            let rows = tx.execute(LAST_SEQUENCE_SQL, None).await?.rows;
            let last = integer(rows.first().context("sequence missing")?, 0)?;
            Ok(json!({ "turns": turns, "last_sequence": last }))
        };
        ctx.db()
            .transaction(read, named("conversation.snapshot"))
            .await
    }

    async fn spoken_answer(&self, ctx: &Ctx<Self>, turn_id: String) -> Result<Value> {
        ensure!(turn_id.len() <= 128, "invalid turn id");
        let rows = ctx.db().query(ANSWER_SQL, params(&[&turn_id])).await?.rows;
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
        let rows = ctx.db().query(HISTORY_PAGE_SQL, cursor).await?.rows;
        let turns = rows
            .iter()
            .map(|r| turn_json(r))
            .collect::<Result<Vec<_>>>()?;
        Ok(json!(turns))
    }
}

impl Handler<Call> for Conversation {
    async fn handle(self: Arc<Self>, ctx: Ctx<Self>, call: Call) -> Result<Value> {
        // Speech polling is the hot device path; see `LIVE_CHECK_TTL`.
        let check = if matches!(call.command, Command::SpeechChunk { .. }) {
            Check::Cached
        } else {
            Check::Fresh
        };
        let session = self.authorize(&ctx, &call.session, None, check).await?;
        match call.command {
            Command::Submit { request_id, text } => {
                ensure!(session.role == "device", "only device may submit");
                let output = self.submit(&ctx, request_id, text).await;
                // Also when busy: a turn left queued by a failed drain gets a new one.
                self.start_drain(&ctx);
                output
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
                let expected = Some((expected_prompt_revision, expected_voice_revision));
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

    /// `create` skips `conversation.sql` on a database at [`SCHEMA_VERSION`], so any change
    /// to the schema must raise the version. Update this hash only together with it.
    #[test]
    fn schema_changes_raise_the_schema_version() {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(include_str!("conversation.sql").as_bytes());
        let hex = digest.iter().fold(String::new(), |mut hex, byte| {
            use std::fmt::Write;
            let _ = write!(hex, "{byte:02x}");
            hex
        });
        assert_eq!(
            (SCHEMA_VERSION, hex.as_str()),
            (
                1,
                "5112e83bda725a9215a50d6a8b9a75d87aa6e2b2f9704055fd0b10adb28d2f3d"
            ),
            "conversation.sql changed: raise SCHEMA_VERSION and MARK_SCHEMA_SQL"
        );
        assert_eq!(
            MARK_SCHEMA_SQL,
            format!("PRAGMA user_version = {SCHEMA_VERSION}")
        );
    }

    fn replay_page(db: &rusqlite::Connection, after: i64) -> Vec<(i64, usize)> {
        let mut statement = db.prepare(REPLAY_SQL).unwrap();
        statement
            .query_map([after, REPLAY_PAGE_BYTES], |r| {
                Ok((r.get(0)?, r.get::<_, String>(3)?.len()))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    #[test]
    fn replay_pages_hold_the_byte_budget_and_never_stall() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch(include_str!("conversation.sql")).unwrap();
        let insert = |bytes: usize| {
            let payload = json!({ "text": "x".repeat(bytes) }).to_string();
            db.execute(
                "INSERT INTO events(turn_id, kind, payload) VALUES ('t', 'text.delta', ?)",
                [payload],
            )
            .unwrap();
        };
        // One event over the budget still pages, alone.
        insert(700_000);
        for _ in 0..3 {
            insert(200_000);
        }
        for _ in 0..200 {
            insert(10);
        }
        let first = replay_page(&db, 0);
        assert_eq!(first.len(), 1);
        let second = replay_page(&db, first[0].0);
        assert_eq!(
            second.len(),
            2,
            "two 200 kB events fit in 512 KiB, three do not"
        );
        let budget = usize::try_from(REPLAY_PAGE_BYTES).unwrap();
        assert!(second.iter().map(|(_, bytes)| bytes + 64).sum::<usize>() <= budget);
        let mut cursor = second[1].0;
        let mut pages = 0;
        loop {
            let page = replay_page(&db, cursor);
            let Some(last) = page.last() else { break };
            assert!(page.len() <= 128);
            cursor = last.0;
            pages += 1;
        }
        assert_eq!(cursor, 204);
        assert_eq!(
            pages, 2,
            "the last 200 kB event with 127 small ones, then the rest"
        );
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

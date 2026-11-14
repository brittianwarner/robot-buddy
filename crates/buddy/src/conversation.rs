//! Conversation transcript and Rig run ownership, independent of device identity and memory.
mod context;
mod harness;
mod persona;
mod profiles;
mod speech;
mod voice;
use crate::{
    auth,
    model::{Delta, Model, ToolCall},
    protocol::*,
};
use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;
use rivetkit::{BindParam as P, ColumnValue as V, SqliteDbExt, action, prelude::*};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

pub struct Conversation {
    signing_key: Vec<u8>,
    draining: AtomicBool,
    model: Model,
    last_authorized: tokio::sync::Mutex<Option<std::time::Instant>>,
}
fn string(row: &[V], index: usize) -> Result<String> {
    match row.get(index) {
        Some(V::Text(v)) => Ok(v.clone()),
        _ => bail!("invalid stored text"),
    }
}
fn integer(row: &[V], index: usize) -> Result<i64> {
    match row.get(index) {
        Some(V::Integer(v)) => Ok(*v),
        _ => bail!("invalid stored integer"),
    }
}
fn params(values: &[&str]) -> Option<Vec<P>> {
    Some(values.iter().map(|v| P::Text((*v).to_owned())).collect())
}
fn bot_key(ctx: &Ctx<Conversation>) -> Result<String> {
    match ctx.key().first() {
        Some(rivetkit::ActorKeySegment::String(s)) => Ok(s.clone()),
        _ => bail!("bot requires a string key"),
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
        let signing_key = std::env::var("BUDDY_SIGNING_KEY")
            .context("BUDDY_SIGNING_KEY required")?
            .into_bytes();
        ensure!(
            signing_key.len() >= 32,
            "BUDDY_SIGNING_KEY must be at least 32 bytes"
        );
        ctx.sql().exec(include_str!("conversation.sql")).await?;
        ctx.sql().transaction(|tx|async move {
            tx.execute("INSERT OR IGNORE INTO system_prompt(singleton,revision,prompt,updated) VALUES(1,1,?,?)",Some(vec![P::Text(include_str!("prompts/persona.txt").into()),P::Integer(auth::now())])).await?;
            tx.execute("INSERT OR IGNORE INTO prompt_versions(revision,prompt,reason,updated) SELECT revision,prompt,'Initial Buddy persona',updated FROM system_prompt WHERE singleton=1 AND revision=1",None).await?;
            Ok(())
        },Default::default()).await?;
        let voice_config = serde_json::to_string(&crate::voice::VoiceConfig::default())?;
        ctx.sql().transaction(|tx|async move {
            tx.execute("INSERT OR IGNORE INTO voice_settings(singleton,revision,config,updated) VALUES(1,1,?,?)",Some(vec![P::Text(voice_config),P::Integer(auth::now())])).await?;
            tx.execute("INSERT OR IGNORE INTO voice_versions(revision,config,reason,updated) SELECT revision,config,'Initial curated voice',updated FROM voice_settings WHERE singleton=1 AND revision=1",None).await?;
            Ok(())
        },Default::default()).await?;
        let default_voice = serde_json::to_string(&crate::voice::VoiceConfig::default())?;
        ctx.sql().transaction(|tx|async move {
            tx.execute("INSERT OR IGNORE INTO personas(name,latest) VALUES('Buddy',1)",None).await?;
            tx.execute("INSERT OR IGNORE INTO persona_versions(name,version,prompt,voice,reason,updated) SELECT 'Buddy',1,?,?,'Default playful robot',? WHERE EXISTS(SELECT 1 FROM personas WHERE name='Buddy' AND latest=1)",Some(vec![P::Text(include_str!("prompts/persona.txt").into()),P::Text(default_voice),P::Integer(auth::now())])).await?;
            Ok(())
        },Default::default()).await?;
        let context_config = serde_json::to_string(&crate::context::ContextConfig::default())?;
        ctx.sql().transaction(|tx|async move {
            tx.execute("INSERT OR IGNORE INTO context_settings(singleton,revision,config,updated) VALUES(1,1,?,?)",Some(vec![P::Text(context_config),P::Integer(auth::now())])).await?;
            tx.execute("INSERT OR IGNORE INTO context_versions(revision,config,reason,updated) SELECT revision,config,'Initial UTC fallback',updated FROM context_settings WHERE singleton=1 AND revision=1",None).await?;
            Ok(())
        },Default::default()).await?;
        Ok(Self {
            signing_key,
            draining: AtomicBool::new(false),
            model: Model::from_env()?,
            last_authorized: tokio::sync::Mutex::new(None),
        })
    }
    async fn on_start(self: Arc<Self>, ctx: Ctx<Self>) -> Result<()> {
        ctx.sql().execute("UPDATE turns SET status='queued' WHERE status='running' AND id IN (SELECT turn_id FROM runs WHERE phase='ready')",None).await?;
        // A provider stream cannot resume after process loss. Preserve its output and make interruption explicit.
        let interrupted = ctx
            .sql()
            .query("SELECT id FROM turns WHERE status='running'", None)
            .await?;
        for row in interrupted.rows {
            self.finish_turn(
                &ctx,
                &string(&row, 0)?,
                "interrupted",
                "",
                json!({"reason":"server_restart"}),
            )
            .await?;
        }
        self.start_drain(ctx);
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
    async fn event(&self, ctx: &Ctx<Self>, turn: &str, kind: &str, payload: Value) -> Result<()> {
        let inserted = ctx
            .sql()
            .execute(
                "INSERT INTO events(turn_id,kind,payload) SELECT ?,?,? WHERE EXISTS(SELECT 1 FROM turns WHERE id=? AND status='running') RETURNING sequence",
                params(&[turn, kind, &payload.to_string(),turn]),
            )
            .await?;
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
    async fn deliver(&self, ctx: &Ctx<Self>, event: &OutputEvent) {
        // A durable commit must not fail because a transient subscriber has gone away.
        if self.check_live(ctx).await.is_err() {
            return;
        }
        let Ok(bot) = bot_key(ctx) else {
            return;
        };
        for conn in ctx.conns_vec() {
            let valid = conn
                .params()
                .ok()
                .and_then(|p| {
                    auth::verify::<auth::Session>(&self.signing_key, "session", &p.session).ok()
                })
                .is_some_and(|s| s.validate(&bot, None).is_ok());
            if valid {
                let _ = conn.send(OutputEvent::NAME, event);
            } else {
                let _ = conn.disconnect(Some("authorization expired")).await;
            }
        }
    }
    fn start_drain(self: &Arc<Self>, ctx: Ctx<Self>) {
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
            let _awake = task_ctx.keep_awake_region();
            if let Err(e) = actor.drain(&task_ctx).await {
                tracing::error!(
                    transport_error = e.is::<reqwest::Error>(),
                    "agent drain failed"
                );
            }
            actor.draining.store(false, Ordering::Release);
            // Submit also invokes this after commit. Recheck closes the idle/submit race.
            if let Ok(rows) = task_ctx
                .sql()
                .query("SELECT id FROM turns WHERE status='queued' LIMIT 1", None)
                .await
                && !rows.rows.is_empty()
            {
                actor.start_drain(task_ctx);
            }
        });
    }
    async fn drain(&self, ctx: &Ctx<Self>) -> Result<()> {
        loop {
            let rows=ctx.sql().query("UPDATE turns SET status='running' WHERE id=(SELECT id FROM turns WHERE status='queued' ORDER BY created LIMIT 1) RETURNING id,text",None).await?.rows;
            let Some(row) = rows.first() else {
                return Ok(());
            };
            let id = string(row, 0)?;
            let text = string(row, 1)?;
            let generation = async {
                self.event(ctx, &id, "turn.started", json!({"text":text}))
                    .await?;
                self.generate(ctx, &id, &text).await
            };
            let abort = ctx.abort_signal();
            let result = tokio::select! {
                _=abort.cancelled()=>return Ok(()),
                result=tokio::time::timeout(Duration::from_secs(180),generation)=>result.context("turn deadline exceeded").and_then(|v|v),
            };
            if let Err(error) = result {
                tracing::warn!(
                    transport_error = error.is::<reqwest::Error>(),
                    "generation failed"
                );
                self.finish_turn(
                    ctx,
                    &id,
                    "failed",
                    "",
                    json!({"message":"The assistant could not finish. Please try again."}),
                )
                .await?;
            }
        }
    }
    async fn check_live(&self, ctx: &Ctx<Self>) -> Result<()> {
        let mut checked = self.last_authorized.lock().await;
        if checked.is_some_and(|at| at.elapsed() < Duration::from_secs(1)) {
            return Ok(());
        }
        let bot = bot_key(ctx)?;
        let token = auth::sign(
            &self.signing_key,
            "session",
            &auth::Session {
                bot_id: bot.clone(),
                role: "agent".into(),
                expires: auth::now() + 60,
            },
        )?;
        crate::actor::check_access(ctx.client()?, &bot, &token).await?;
        *checked = Some(std::time::Instant::now());
        Ok(())
    }
    async fn active(&self, ctx: &Ctx<Self>, id: &str) -> Result<()> {
        self.check_live(ctx).await?;
        let rows = ctx
            .sql()
            .query(
                "SELECT id FROM turns WHERE id=? AND status='running'",
                params(&[id]),
            )
            .await?
            .rows;
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
        let changed=ctx.sql().transaction(|tx|async move {
            let partial = if status != "completed" {
                tx.execute("SELECT json_extract(payload,'$.text') FROM events WHERE turn_id=? AND kind='text.delta' ORDER BY sequence", params(&[id])).await?.rows.iter().map(|r|string(r,0)).collect::<Result<Vec<_>>>()?.join("")
            } else { answer.to_owned() };
            let rows=tx.execute("UPDATE turns SET status=?,answer=? WHERE id=? AND status IN ('queued','running') RETURNING id",params(&[status,&partial,id])).await?.rows;
            if rows.is_empty(){return Ok(None);}
            let inserted=tx.execute("INSERT INTO events(turn_id,kind,payload) VALUES(?,?,?)",params(&[id,&kind,&encoded])).await?;
            tx.execute("DELETE FROM runs WHERE turn_id=?",params(&[id])).await?;
                Ok(inserted.last_insert_row_id)
        },Default::default()).await?;
        if let Some(sequence) = changed {
            self.deliver(
                ctx,
                &OutputEvent {
                    sequence,
                    turn_id: id.into(),
                    kind: format!("turn.{status}"),
                    payload,
                },
            )
            .await;
        }
        Ok(())
    }
    async fn tool(&self, ctx: &Ctx<Self>, turn: &str, call: &ToolCall) -> Result<Value> {
        ensure!(!call.id.is_empty(), "tool id required");
        ensure!(
            call.arguments.len() <= 16384 && call.id.len() <= 512 && call.name.len() <= 128,
            "tool call exceeds budget"
        );
        let args: Value = serde_json::from_str(&call.arguments)?;
        let prior = ctx
            .sql()
            .query(
                "SELECT result,name,args FROM tool_calls WHERE turn_id=? AND call_id=?",
                params(&[turn, &call.id]),
            )
            .await?
            .rows;
        if let Some(r) = prior.first() {
            ensure!(
                string(r, 1)? == call.name && string(r, 2)? == call.arguments,
                "tool receipt identity mismatch"
            );
            let value = string(r, 0)?;
            ensure!(!value.is_empty(), "tool outcome unresolved");
            return Ok(serde_json::from_str(&value)?);
        }
        ctx.sql()
            .execute(
                "INSERT INTO tool_calls(turn_id,call_id,name,args) SELECT ?,?,?,? WHERE EXISTS(SELECT 1 FROM turns WHERE id=? AND status='running')",
                params(&[turn, &call.id, &call.name, &call.arguments,turn]),
            )
            .await?;
        self.event(ctx, turn, "tool.started", json!({"name":call.name}))
            .await?;
        self.active(ctx, turn).await?;
        let result = match call.name.as_str() {
            "list_personas" => self.list_personas(ctx).await?,
            "get_persona" => {
                self.get_persona(
                    ctx,
                    args["name"].as_str().context("name required")?,
                    args["version"].as_i64().context("version required")?,
                )
                .await?
            }
            "save_persona" => {
                self.save_profile(
                    ctx,
                    &serde_json::from_value(args.clone())?,
                    Some((turn, &call.id)),
                )
                .await?
            }
            "activate_persona" => {
                // Revisions are concurrency tokens, not facts the model should invent.
                let prompt = self.persona(ctx).await?;
                let voice = self.voice_config(ctx).await?;
                self.activate_profile(
                    ctx,
                    args["name"].as_str().context("name required")?,
                    args["version"].as_i64().context("version required")?,
                    (
                        prompt["revision"]
                            .as_i64()
                            .context("prompt revision missing")?,
                        voice["revision"]
                            .as_i64()
                            .context("voice revision missing")?,
                    ),
                    Some((turn, &call.id)),
                )
                .await?
            }
            "get_context_config" => self.context_config(ctx).await?,
            "update_context_config" => {
                let config = serde_json::from_value::<crate::context::ContextConfig>(
                    args["config"].clone(),
                )?;
                self.set_context(
                    ctx,
                    args["expected_revision"]
                        .as_i64()
                        .context("expected revision required")?,
                    &config,
                    args["reason"].as_str().context("reason required")?,
                    Some((turn, &call.id)),
                )
                .await?
            }
            "get_voice_config" => self.voice_config(ctx).await?,
            "update_voice_config" => {
                let config =
                    serde_json::from_value::<crate::voice::VoiceConfig>(args["config"].clone())?;
                self.set_voice(
                    ctx,
                    args["expected_revision"]
                        .as_i64()
                        .context("revision required")?,
                    &config,
                    args["reason"].as_str().context("reason required")?,
                    Some((turn, &call.id)),
                )
                .await?
            }
            "read_system_prompt" => self.persona(ctx).await?,
            "update_system_prompt" => {
                self.set_persona(
                    ctx,
                    args["expected_revision"]
                        .as_i64()
                        .context("revision required")?,
                    args["prompt"].as_str().context("prompt required")?,
                    args["reason"].as_str().context("reason required")?,
                    Some((turn, &call.id)),
                )
                .await?
            }
            "history_search" => {
                self.search_history(ctx, args["query"].as_str().context("query required")?)
                    .await?
            }
            "web_search" => {
                crate::model::search(args["query"].as_str().context("query required")?).await?
            }
            "memory_remember" => {
                self.remember(
                    ctx,
                    args["key"].as_str().context("key required")?,
                    args["value"].as_str().context("value required")?,
                )
                .await?;
                json!({"saved":true})
            }
            "memory_forget" => {
                self.memory(
                    ctx,
                    Command::Forget {
                        key: args["key"].as_str().context("key required")?.into(),
                    },
                )
                .await?;
                json!({"forgotten":true})
            }
            _ => bail!("unknown tool"),
        };
        ensure!(
            result.to_string().len() <= 32768,
            "tool result exceeds budget"
        );
        ctx.sql()
            .execute(
                "UPDATE tool_calls SET result=? WHERE turn_id=? AND call_id=?",
                params(&[&result.to_string(), turn, &call.id]),
            )
            .await?;
        self.event(ctx, turn, "tool.completed", json!({"name":call.name}))
            .await?;
        Ok(result)
    }
    async fn memory(&self, ctx: &Ctx<Self>, command: Command) -> Result<Value> {
        let bot = bot_key(ctx)?;
        let token = auth::sign(
            &self.signing_key,
            "session",
            &auth::Session {
                bot_id: bot.clone(),
                role: "agent".into(),
                expires: auth::now() + 60,
            },
        )?;
        crate::memory::call(ctx.client()?, &bot, &token, command).await
    }
    async fn remember(&self, ctx: &Ctx<Self>, key: &str, value: &str) -> Result<()> {
        self.memory(
            ctx,
            Command::Remember {
                key: key.into(),
                value: value.into(),
            },
        )
        .await?;
        Ok(())
    }
    async fn search_history(&self, ctx: &Ctx<Self>, query: &str) -> Result<Value> {
        ensure!(
            !query.is_empty() && query.len() <= 256,
            "invalid history query"
        );
        let escaped = query.replace('"', "\"\"");
        let rows=ctx.sql().query("SELECT t.id,t.status,substr(t.text,1,1500),substr(t.answer,1,2500) FROM history_search h JOIN turns t ON t.id=h.turn_id WHERE history_search MATCH ? ORDER BY rank LIMIT 6",params(&[&format!("\"{escaped}\"")])).await?.rows;
        Ok(json!(rows.into_iter().map(|r|Ok(json!({"turn_id":string(&r,0)?,"status":string(&r,1)?,"user":string(&r,2)?,"assistant":string(&r,3)?}))).collect::<Result<Vec<_>>>()?))
    }
}
impl Handler<Call> for Conversation {
    async fn handle(self: Arc<Self>, ctx: Ctx<Self>, call: Call) -> Result<Value> {
        let session = self.authorize(&ctx, &call.session, None).await?;
        match call.command {
            Command::Submit { request_id, text } => {
                ensure!(session.role == "device", "only device may submit");
                ensure!(
                    !request_id.is_empty()
                        && request_id.len() <= 128
                        && !text.trim().is_empty()
                        && text.len() <= 8192,
                    "invalid turn"
                );
                let id = uuid::Uuid::now_v7().to_string();
                let created = auth::now();
                let output=ctx.sql().transaction(|tx|async move{
                    let prior=tx.execute("SELECT id,text FROM turns WHERE request_id=?",params(&[&request_id])).await?.rows;
                    if let Some(row)=prior.first(){ensure!(string(row,1)?==text,"request id reused with different text");return Ok(json!({"turn_id":string(row,0)?,"duplicate":true}));}
                    ensure!(tx.execute("SELECT id FROM turns WHERE status IN ('queued','running') LIMIT 1",None).await?.rows.is_empty(),"busy");
                    tx.execute("INSERT INTO turns(id,request_id,text,created) VALUES(?,?,?,?)",Some(vec![P::Text(id.clone()),P::Text(request_id),P::Text(text),P::Integer(created)])).await?;
                    Ok(json!({"turn_id":id,"duplicate":false}))
                },Default::default()).await?;
                self.start_drain(ctx);
                Ok(output)
            }
            Command::Cancel { turn_id } => {
                self.finish_turn(&ctx, &turn_id, "cancelled", "", json!({}))
                    .await?;
                Ok(json!({"ok":true}))
            }
            Command::Replay { after } => {
                let rows=ctx.sql().query("SELECT sequence,turn_id,kind,payload FROM events WHERE sequence>? ORDER BY sequence LIMIT 128",Some(vec![P::Integer(after.max(0))])).await?.rows;
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
            Command::Snapshot => {
                ctx.sql().transaction(|tx|async move {
                    let rows=tx.execute("SELECT id,status,text,answer,EXISTS(SELECT 1 FROM speech_chunks c WHERE c.turn_id=turns.id) FROM turns ORDER BY id DESC LIMIT 20",None).await?.rows;
                    let turns=rows.into_iter().map(|r|Ok(json!({"id":string(&r,0)?,"status":string(&r,1)?,"text":string(&r,2)?,"answer":string(&r,3)?,"speech_ready":integer(&r,4)?!=0}))).collect::<Result<Vec<_>>>()?;
                    let row=tx.execute("SELECT COALESCE(MAX(sequence),0) FROM events",None).await?.rows;
                    Ok(json!({"turns":turns,"last_sequence":integer(&row[0],0)?}))
                },Default::default()).await
            }
            Command::ListPersonas => self.list_personas(&ctx).await,
            Command::GetPersona{name,version} => self.get_persona(&ctx,&name,version).await,
            Command::SavePersona{persona} => {
                ensure!(["device","agent"].contains(&session.role.as_str()),"persona update requires device or agent role");
                self.save_profile(&ctx,&persona,None).await
            }
            Command::ActivatePersona{name,version,expected_prompt_revision,expected_voice_revision} => {
                ensure!(["device","agent"].contains(&session.role.as_str()),"persona update requires device or agent role");
                self.activate_profile(&ctx,&name,version,(expected_prompt_revision,expected_voice_revision),None).await
            }
            Command::GetContextConfig => self.context_config(&ctx).await,
            Command::SetContextConfig {expected_revision,config,reason} => {
                ensure!(["device","agent"].contains(&session.role.as_str()),"context update requires device or agent role");
                self.set_context(&ctx,expected_revision,&config,&reason,None).await
            }
            Command::GetVoiceConfig => self.voice_config(&ctx).await,
            Command::SetVoiceConfig { expected_revision,config,reason } => {
                ensure!(["device","agent"].contains(&session.role.as_str()),"voice update requires device or agent role");
                self.set_voice(&ctx,expected_revision,&config,&reason,None).await
            }
            Command::GetSystemPrompt => self.persona(&ctx).await,
            Command::SystemPromptHistory => self.persona_history(&ctx).await,
            Command::SetSystemPrompt { expected_revision,prompt,reason } => {
                ensure!(["device","agent"].contains(&session.role.as_str()),"prompt update requires device or agent role");
                self.set_persona(&ctx,expected_revision,&prompt,&reason,None).await
            }
            Command::SpeechChunk {turn_id,index} => {
                ensure!(session.role=="device","speech requires device role");
                self.speech_chunk(&ctx,&turn_id,index).await
            }
            Command::SpokenAnswer { turn_id } => {
                ensure!(session.role == "device", "speech requires device role");
                ensure!(turn_id.len() <= 128, "invalid turn id");
                let rows=ctx.sql().query("SELECT answer FROM turns WHERE id=? AND status='completed'",params(&[&turn_id])).await?.rows;
                let row=rows.first().context("only completed answers can be spoken")?;
                let answer=string(row,0)?;
                ensure!(!answer.trim().is_empty() && answer.len()<=12000,"answer outside speech budget");
                Ok(json!({"turn_id":turn_id,"text":answer,"voice":self.speech_voice(&ctx,&turn_id).await?}))
            }
            Command::History { before } => {
                let rows=ctx.sql().query("SELECT id,status,text,answer FROM turns WHERE (?='' OR id<?) ORDER BY id DESC LIMIT 20",params(&[&before,&before])).await?.rows;
                Ok(json!(rows.into_iter().map(|r|Ok(json!({"id":string(&r,0)?,"status":string(&r,1)?,"text":string(&r,2)?,"answer":string(&r,3)?}))).collect::<Result<Vec<_>>>()?))
            }
            Command::SearchHistory { query } => self.search_history(&ctx, &query).await,
            _ => bail!("command is not owned by conversation actor"),
        }
    }
}

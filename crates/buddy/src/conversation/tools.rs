//! Model tool calls. Each call commits its intent, runs one effect and records a receipt,
//! so a resumed turn replays the receipt instead of repeating the effect.
use super::{
    Command, Context, Conversation, Ctx, Db, RECEIPT_SQL, Receipt, Result, ToolCall, Value, bail,
    ensure, json, params, string,
};

const MAX_ARGUMENT_BYTES: usize = 16_384;
const MAX_CALL_ID_BYTES: usize = 512;
const MAX_NAME_BYTES: usize = 128;
const MAX_RESULT_BYTES: usize = 32_768;
const PRIOR_CALL_SQL: &str =
    "SELECT result, name, args FROM tool_calls WHERE turn_id = ? AND call_id = ?";
const INTENT_SQL: &str = r"
    INSERT INTO tool_calls(turn_id, call_id, name, args) SELECT ?, ?, ?, ?
    WHERE EXISTS(SELECT 1 FROM turns WHERE id = ? AND status = 'running')
    RETURNING call_id";
/// Settings writes record their receipt in their own transaction.
const SELF_RECEIPTED: [&str; 5] = [
    "save_persona",
    "activate_persona",
    "update_context_config",
    "update_voice_config",
    "update_system_prompt",
];

/// A required string argument.
fn text<'a>(args: &'a Value, key: &str, missing: &'static str) -> Result<&'a str> {
    args[key].as_str().context(missing)
}

/// A required integer argument.
fn number(args: &Value, key: &str, missing: &'static str) -> Result<i64> {
    args[key].as_i64().context(missing)
}

impl Conversation {
    pub(super) async fn tool(&self, ctx: &Ctx<Self>, turn: &str, call: &ToolCall) -> Result<Value> {
        ensure!(!call.id.is_empty(), "tool id required");
        ensure!(
            call.arguments.len() <= MAX_ARGUMENT_BYTES
                && call.id.len() <= MAX_CALL_ID_BYTES
                && call.name.len() <= MAX_NAME_BYTES,
            "tool call exceeds budget"
        );
        let args: Value = serde_json::from_str(&call.arguments)?;
        if let Some(result) = self.prior_result(ctx, turn, call).await? {
            return Ok(result);
        }
        // The intent commits before the effect. A cancelled turn admits no new effect.
        let intent = params(&[turn, &call.id, &call.name, &call.arguments, turn]);
        let admitted = ctx.db().execute(INTENT_SQL, intent).await?;
        ensure!(!admitted.rows.is_empty(), "turn cancelled");
        self.event(ctx, turn, "tool.started", json!({ "name": call.name }))
            .await?;
        self.active(ctx, turn).await?;
        let receipt = Some((turn, call.id.as_str()));
        let result = self.run_tool(ctx, &call.name, &args, receipt).await?;
        let encoded = result.to_string();
        ensure!(
            encoded.len() <= MAX_RESULT_BYTES,
            "tool result exceeds budget"
        );
        if !SELF_RECEIPTED.contains(&call.name.as_str()) {
            ctx.db()
                .execute(RECEIPT_SQL, params(&[&encoded, turn, &call.id]))
                .await?;
        }
        self.event(ctx, turn, "tool.completed", json!({ "name": call.name }))
            .await?;
        Ok(result)
    }

    /// The recorded result of a call this turn already made. An intent without a result
    /// means its effect may or may not have happened, so the turn cannot continue.
    async fn prior_result(
        &self,
        ctx: &Ctx<Self>,
        turn: &str,
        call: &ToolCall,
    ) -> Result<Option<Value>> {
        let prior = ctx
            .db()
            .query(PRIOR_CALL_SQL, params(&[turn, &call.id]))
            .await?
            .rows;
        let Some(row) = prior.first() else {
            return Ok(None);
        };
        ensure!(
            string(row, 1)? == call.name && string(row, 2)? == call.arguments,
            "tool receipt identity mismatch"
        );
        let value = string(row, 0)?;
        ensure!(!value.is_empty(), "tool outcome unresolved");
        Ok(Some(serde_json::from_str(&value)?))
    }

    /// Runs one tool. Writes record their result under `receipt` in their own transaction.
    /// Network calls stop with the turn; their intent then stays unresolved.
    async fn run_tool(
        &self,
        ctx: &Ctx<Self>,
        name: &str,
        args: &Value,
        receipt: Receipt<'_>,
    ) -> Result<Value> {
        let turn = receipt.map_or("", |(turn, _)| turn);
        match name {
            "list_personas" => self.list_personas(ctx).await,
            "get_persona" => {
                let name = text(args, "name", "name required")?;
                let version = number(args, "version", "version required")?;
                self.get_persona(ctx, name, version).await
            }
            "save_persona" => {
                let draft = crate::protocol::strict_value(args)?;
                self.save_profile(ctx, &draft, receipt).await
            }
            "activate_persona" => self.activate_persona_tool(ctx, args, receipt).await,
            "get_context_config" => self.context_config(ctx).await,
            "update_context_config" => self.update_context_tool(ctx, args, receipt).await,
            "get_voice_config" => self.voice_config(ctx).await,
            "update_voice_config" => self.update_voice_tool(ctx, args, receipt).await,
            "read_system_prompt" => self.persona(ctx).await,
            "update_system_prompt" => {
                let revision = number(args, "expected_revision", "revision required")?;
                let prompt = text(args, "prompt", "prompt required")?;
                let reason = text(args, "reason", "reason required")?;
                self.set_persona(ctx, revision, prompt, reason, receipt)
                    .await
            }
            "history_search" => {
                let query = text(args, "query", "query required")?;
                self.search_history(ctx, query).await
            }
            "web_search" => {
                let query = text(args, "query", "query required")?;
                self.interruptible(turn, crate::model::search(query)).await
            }
            "memory_remember" => {
                let key = text(args, "key", "key required")?.into();
                let value = text(args, "value", "value required")?.into();
                let remember = self.memory(ctx, Command::Remember { key, value });
                self.interruptible(turn, remember).await?;
                Ok(json!({ "saved": true }))
            }
            "memory_forget" => {
                let key = text(args, "key", "key required")?.into();
                let forget = self.memory(ctx, Command::Forget { key });
                self.interruptible(turn, forget).await?;
                Ok(json!({ "forgotten": true }))
            }
            _ => bail!("unknown tool"),
        }
    }

    async fn activate_persona_tool(
        &self,
        ctx: &Ctx<Self>,
        args: &Value,
        receipt: Receipt<'_>,
    ) -> Result<Value> {
        // Revisions are concurrency tokens, not facts the model should invent. They are
        // read in the activation's own transaction.
        let name = text(args, "name", "name required")?;
        let version = number(args, "version", "version required")?;
        self.activate_profile(ctx, name, version, None, receipt)
            .await
    }

    async fn update_context_tool(
        &self,
        ctx: &Ctx<Self>,
        args: &Value,
        receipt: Receipt<'_>,
    ) -> Result<Value> {
        let config: crate::context::ContextConfig = crate::protocol::strict_value(&args["config"])?;
        let revision = number(args, "expected_revision", "expected revision required")?;
        let reason = text(args, "reason", "reason required")?;
        self.set_context(ctx, revision, &config, reason, receipt)
            .await
    }

    async fn update_voice_tool(
        &self,
        ctx: &Ctx<Self>,
        args: &Value,
        receipt: Receipt<'_>,
    ) -> Result<Value> {
        let config: crate::voice::VoiceConfig = crate::protocol::strict_value(&args["config"])?;
        let revision = number(args, "expected_revision", "revision required")?;
        let reason = text(args, "reason", "reason required")?;
        self.set_voice(ctx, revision, &config, reason, receipt)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_name_what_is_missing() {
        let args = json!({ "name": "Buddy", "version": 2, "count": "3" });
        assert_eq!(text(&args, "name", "name required").unwrap(), "Buddy");
        assert_eq!(number(&args, "version", "version required").unwrap(), 2);
        let missing = text(&args, "reason", "reason required").unwrap_err();
        assert_eq!(missing.to_string(), "reason required");
        assert!(number(&args, "count", "count required").is_err());
    }
}

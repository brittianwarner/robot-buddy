//! The conversation owns its bounded, versioned harness configuration.
use super::*;
impl Conversation {
    pub(super) async fn persona(&self, ctx: &Ctx<Self>) -> Result<Value> {
        let rows = ctx
            .sql()
            .query(
                "SELECT revision,prompt,updated FROM system_prompt WHERE singleton=1",
                None,
            )
            .await?
            .rows;
        let row = rows.first().context("system prompt missing")?;
        Ok(json!({"revision":integer(row,0)?,"prompt":string(row,1)?,"updated":integer(row,2)?}))
    }
    pub(super) async fn set_persona(
        &self,
        ctx: &Ctx<Self>,
        expected_revision: i64,
        prompt: &str,
        reason: &str,
        receipt: Option<(&str, &str)>,
    ) -> Result<Value> {
        ensure!(
            expected_revision >= 1
                && !prompt.trim().is_empty()
                && prompt.len() <= 8192
                && !reason.trim().is_empty()
                && reason.len() <= 500,
            "invalid system prompt update"
        );
        ctx.sql().transaction(|tx|async move{
            if let Some((turn,_))=receipt {
                ensure!(!tx.execute("SELECT id FROM turns WHERE id=? AND status='running'",params(&[turn])).await?.rows.is_empty(),"turn cancelled");
            }
            let now=auth::now();
            let rows=tx.execute("UPDATE system_prompt SET revision=revision+1,prompt=?,updated=? WHERE singleton=1 AND revision=? RETURNING revision",Some(vec![P::Text(prompt.into()),P::Integer(now),P::Integer(expected_revision)])).await?.rows;
            let revision=integer(rows.first().context("system prompt revision conflict; read it again")?,0)?;
            tx.execute("INSERT INTO prompt_versions(revision,prompt,reason,updated) VALUES(?,?,?,?)",Some(vec![P::Integer(revision),P::Text(prompt.into()),P::Text(reason.into()),P::Integer(now)])).await?;
            tx.execute("DELETE FROM prompt_versions WHERE revision<?",Some(vec![P::Integer((revision-15).max(1))])).await?;
            let result=json!({"revision":revision,"saved":true,"applies":"next_turn"});
            // Prompt mutation and its tool receipt commit together; a recovery cannot apply it twice.
            if let Some((turn,call))=receipt {
                tx.execute("UPDATE tool_calls SET result=? WHERE turn_id=? AND call_id=?",params(&[&result.to_string(),turn,call])).await?;
            }
            Ok(result)
        },Default::default()).await
    }
    pub(super) async fn persona_history(&self, ctx: &Ctx<Self>) -> Result<Value> {
        let rows=ctx.sql().query("SELECT revision,prompt,reason,updated FROM prompt_versions ORDER BY revision DESC LIMIT 16",None).await?.rows;
        Ok(json!(rows.into_iter().map(|r|Ok(json!({"revision":integer(&r,0)?,"prompt":string(&r,1)?,"reason":string(&r,2)?,"updated":integer(&r,3)?}))).collect::<Result<Vec<_>>>()?))
    }
}

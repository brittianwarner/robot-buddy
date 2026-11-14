use super::*;
use crate::context::ContextConfig;
impl Conversation {
    pub(super) async fn context_config(&self, ctx: &Ctx<Self>) -> Result<Value> {
        let rows = ctx
            .sql()
            .query(
                "SELECT revision,config,updated FROM context_settings WHERE singleton=1",
                None,
            )
            .await?
            .rows;
        let row = rows.first().context("context configuration missing")?;
        let config: ContextConfig = serde_json::from_str(&string(row, 1)?)?;
        Ok(
            json!({"revision":integer(row,0)?,"config":config,"updated":integer(row,2)?,"current":config.at(auth::now())?}),
        )
    }
    pub(super) async fn set_context(
        &self,
        ctx: &Ctx<Self>,
        expected_revision: i64,
        config: &ContextConfig,
        reason: &str,
        receipt: Option<(&str, &str)>,
    ) -> Result<Value> {
        config.validate()?;
        ensure!(
            expected_revision >= 1 && !reason.trim().is_empty() && reason.len() <= 500,
            "invalid context update"
        );
        let encoded = serde_json::to_string(config)?;
        ctx.sql().transaction(|tx|async move {
            if let Some((turn,_))=receipt {ensure!(!tx.execute("SELECT id FROM turns WHERE id=? AND status='running'",params(&[turn])).await?.rows.is_empty(),"turn cancelled");}
            let now=auth::now();
            let rows=tx.execute("UPDATE context_settings SET revision=revision+1,config=?,updated=? WHERE singleton=1 AND revision=? RETURNING revision",Some(vec![P::Text(encoded.clone()),P::Integer(now),P::Integer(expected_revision)])).await?.rows;
            let revision=integer(rows.first().context("context revision conflict; read it again")?,0)?;
            tx.execute("INSERT INTO context_versions(revision,config,reason,updated) VALUES(?,?,?,?)",Some(vec![P::Integer(revision),P::Text(encoded),P::Text(reason.into()),P::Integer(now)])).await?;
            tx.execute("DELETE FROM context_versions WHERE revision<?",Some(vec![P::Integer((revision-15).max(1))])).await?;
            let result=json!({"revision":revision,"saved":true,"applies":"next_model_request"});
            if let Some((turn,call))=receipt {tx.execute("UPDATE tool_calls SET result=? WHERE turn_id=? AND call_id=?",params(&[&result.to_string(),turn,call])).await?;}
            Ok(result)
        },Default::default()).await
    }
}

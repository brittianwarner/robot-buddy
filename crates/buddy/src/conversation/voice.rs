use super::*;
use crate::voice::VoiceConfig;
impl Conversation {
    pub(super) async fn voice_config(&self, ctx: &Ctx<Self>) -> Result<Value> {
        let rows = ctx
            .sql()
            .query(
                "SELECT revision,config,updated FROM voice_settings WHERE singleton=1",
                None,
            )
            .await?
            .rows;
        let row = rows.first().context("voice configuration missing")?;
        let config: VoiceConfig = serde_json::from_str(&string(row, 1)?)?;
        Ok(
            json!({"revision":integer(row,0)?,"config":config,"voice_id":config.voice.id(),"name":config.voice.name(),"updated":integer(row,2)?,"choices":crate::voice::catalog()}),
        )
    }
    pub(super) async fn set_voice(
        &self,
        ctx: &Ctx<Self>,
        expected_revision: i64,
        config: &VoiceConfig,
        reason: &str,
        receipt: Option<(&str, &str)>,
    ) -> Result<Value> {
        config.validate()?;
        ensure!(
            expected_revision >= 1 && !reason.trim().is_empty() && reason.len() <= 500,
            "invalid voice update"
        );
        let encoded = serde_json::to_string(config)?;
        ctx.sql().transaction(|tx|async move {
            if let Some((turn,_))=receipt {ensure!(!tx.execute("SELECT id FROM turns WHERE id=? AND status='running'",params(&[turn])).await?.rows.is_empty(),"turn cancelled");}
            let now=auth::now();
            let rows=tx.execute("UPDATE voice_settings SET revision=revision+1,config=?,updated=? WHERE singleton=1 AND revision=? RETURNING revision",Some(vec![P::Text(encoded.clone()),P::Integer(now),P::Integer(expected_revision)])).await?.rows;
            let revision=integer(rows.first().context("voice revision conflict; read it again")?,0)?;
            tx.execute("INSERT INTO voice_versions(revision,config,reason,updated) VALUES(?,?,?,?)",Some(vec![P::Integer(revision),P::Text(encoded),P::Text(reason.into()),P::Integer(now)])).await?;
            tx.execute("DELETE FROM voice_versions WHERE revision<?",Some(vec![P::Integer((revision-15).max(1))])).await?;
            let result=json!({"revision":revision,"saved":true,"applies":"next_speech_request","voice":config.voice,"voice_id":config.voice.id()});
            if let Some((turn,call))=receipt {tx.execute("UPDATE tool_calls SET result=? WHERE turn_id=? AND call_id=?",params(&[&result.to_string(),turn,call])).await?;}
            Ok(result)
        },Default::default()).await
    }
}

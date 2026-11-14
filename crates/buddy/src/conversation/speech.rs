//! Durable speech admission: only the tool-disabled final phase can commit chunks.
use super::*;
impl Conversation {
    pub(super) async fn begin_speech(&self, ctx: &Ctx<Self>, id: &str) -> Result<String> {
        ctx.sql().transaction(|tx|async move {
            let rows=tx.execute("SELECT p.prompt,v.config,v.revision,v.updated FROM system_prompt p CROSS JOIN voice_settings v WHERE p.singleton=1 AND v.singleton=1",None).await?.rows;
            let row=rows.first().context("speech settings missing")?;
            let config:crate::voice::VoiceConfig=serde_json::from_str(&string(row,1)?)?;
            let voice=json!({"config":config,"revision":integer(row,2)?,"updated":integer(row,3)?,"voice_id":config.voice.id(),"name":config.voice.name(),"choices":crate::voice::catalog()});
            let inserted=tx.execute("INSERT INTO speech_turns(turn_id,voice) SELECT ?,? WHERE EXISTS(SELECT 1 FROM turns WHERE id=? AND status='running') RETURNING turn_id",params(&[id,&voice.to_string(),id])).await?.rows;
            ensure!(!inserted.is_empty(),"turn cancelled");
            string(row,0)
        },Default::default()).await
    }

    pub(super) async fn speech_voice(&self, ctx: &Ctx<Self>, id: &str) -> Result<Value> {
        let rows = ctx
            .sql()
            .query(
                "SELECT voice FROM speech_turns WHERE turn_id=?",
                params(&[id]),
            )
            .await?
            .rows;
        match rows.first() {
            Some(row) => Ok(serde_json::from_str(&string(row, 0)?)?),
            None => self.voice_config(ctx).await,
        }
    }
    pub(super) async fn commit_speech(
        &self,
        ctx: &Ctx<Self>,
        id: &str,
        index: u32,
        text: &str,
    ) -> Result<()> {
        ensure!(
            index < 64 && text.len() <= 604,
            "speech chunk budget exceeded"
        );
        ctx.sql().transaction(|tx|async move {
    let rows=tx.execute("INSERT INTO speech_chunks(turn_id,chunk_index,text) SELECT ?,?,? WHERE EXISTS(SELECT 1 FROM turns WHERE id=? AND status='running') RETURNING chunk_index",Some(vec![P::Text(id.into()),P::Integer(i64::from(index)),P::Text(text.into()),P::Text(id.into())])).await?.rows;
    ensure!(!rows.is_empty(),"turn cancelled");
    Ok(())
  },Default::default()).await?;
        self.event(ctx, id, "speech.ready", json!({"index":index}))
            .await
    }
    pub(super) async fn speech_chunk(
        &self,
        ctx: &Ctx<Self>,
        id: &str,
        index: u32,
    ) -> Result<Value> {
        ensure!(
            !id.is_empty() && id.len() <= 128 && index <= 64,
            "invalid speech cursor"
        );
        ctx.sql().transaction(|tx|async move {
   let rows=tx.execute("SELECT status FROM turns WHERE id=?",params(&[id])).await?.rows;
   let status=string(rows.first().context("turn missing")?,0)?;
   ensure!(["running","completed"].contains(&status.as_str()),"speech turn stopped");
   let rows=tx.execute("SELECT c.text,s.voice FROM speech_chunks c JOIN speech_turns s ON s.turn_id=c.turn_id WHERE c.turn_id=? AND c.chunk_index=?",Some(vec![P::Text(id.into()),P::Integer(i64::from(index))])).await?.rows;
   if let Some(row)=rows.first(){return Ok(json!({"state":"ready","index":index,"text":string(row,0)?,"voice":serde_json::from_str::<Value>(&string(row,1)?)?}));}
   Ok(json!({"state":if status=="completed"{"done"}else{"pending"}}))
  },Default::default()).await
    }
}

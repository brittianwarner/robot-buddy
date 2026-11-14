//! Bounded persona library; selecting a profile updates prompt and voice atomically.
use super::*;
impl Conversation {
    pub(super) async fn list_personas(&self, ctx: &Ctx<Self>) -> Result<Value> {
        let rows=ctx.sql().query("SELECT p.name,p.latest,substr(v.reason,1,160),v.updated FROM personas p JOIN persona_versions v ON v.name=p.name AND v.version=p.latest ORDER BY p.name LIMIT 50",None).await?.rows;
        Ok(json!(rows.iter().map(|r|Ok(json!({"name":string(r,0)?,"latest_version":integer(r,1)?,"reason":string(r,2)?,"updated":integer(r,3)?}))).collect::<Result<Vec<_>>>()?))
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
        ctx.sql().transaction(|tx|async move {
            let rows=tx.execute("SELECT v.name,v.version,v.prompt,v.voice,v.reason,v.updated FROM persona_versions v JOIN personas p ON p.name=v.name WHERE v.name=? AND v.version=CASE WHEN ?=0 THEN p.latest ELSE ? END",Some(vec![P::Text(name.into()),P::Integer(version),P::Integer(version)])).await?.rows;
            let r=rows.first().context("persona version not found")?;
            let versions=tx.execute("SELECT version,reason,updated FROM persona_versions WHERE name=? ORDER BY version DESC LIMIT 16",params(&[name])).await?.rows;
            Ok(json!({"name":string(r,0)?,"version":integer(r,1)?,"prompt":string(r,2)?,"voice":serde_json::from_str::<crate::voice::VoiceConfig>(&string(r,3)?)?,"reason":string(r,4)?,"updated":integer(r,5)?,"versions":versions.iter().map(|v|Ok(json!({"version":integer(v,0)?,"reason":string(v,1)?,"updated":integer(v,2)?}))).collect::<Result<Vec<_>>>()?}))
        },Default::default()).await
    }
    pub(super) async fn save_profile(
        &self,
        ctx: &Ctx<Self>,
        draft: &crate::persona::PersonaDraft,
        receipt: Option<(&str, &str)>,
    ) -> Result<Value> {
        draft.validate()?;
        let voice = serde_json::to_string(&draft.voice)?;
        ctx.sql().transaction(|tx|async move {
            if let Some((turn,_))=receipt {ensure!(!tx.execute("SELECT id FROM turns WHERE id=? AND status='running'",params(&[turn])).await?.rows.is_empty(),"turn cancelled");}
            if draft.expected_version==0 {
                let count=tx.execute("SELECT COUNT(*) FROM personas",None).await?.rows;
                ensure!(integer(count.first().context("count missing")?,0)?<50,"persona library is full (50 names)");
                tx.execute("INSERT INTO personas(name,latest) VALUES(?,1)",params(&[&draft.name])).await?;
            } else {
                let changed=tx.execute("UPDATE personas SET latest=latest+1 WHERE name=? AND latest=? RETURNING latest",Some(vec![P::Text(draft.name.clone()),P::Integer(draft.expected_version)])).await?.rows;
                ensure!(!changed.is_empty(),"persona version conflict; read it again");
            }
            let version=draft.expected_version+1;
            tx.execute("INSERT INTO persona_versions(name,version,prompt,voice,reason,updated) VALUES(?,?,?,?,?,?)",Some(vec![P::Text(draft.name.clone()),P::Integer(version),P::Text(draft.prompt.clone()),P::Text(voice),P::Text(draft.reason.clone()),P::Integer(auth::now())])).await?;
            tx.execute("DELETE FROM persona_versions WHERE name=? AND version<?",Some(vec![P::Text(draft.name.clone()),P::Integer((version-15).max(1))])).await?;
            let result=json!({"name":draft.name,"version":version,"saved":true,"active":false});
            if let Some((turn,call))=receipt {tx.execute("UPDATE tool_calls SET result=? WHERE turn_id=? AND call_id=?",params(&[&result.to_string(),turn,call])).await?;}
            Ok(result)
        },Default::default()).await
    }
    pub(super) async fn activate_profile(
        &self,
        ctx: &Ctx<Self>,
        name: &str,
        version: i64,
        expected: (i64, i64),
        receipt: Option<(&str, &str)>,
    ) -> Result<Value> {
        crate::persona::validate_name(name)?;
        ensure!(
            version > 0 && expected.0 > 0 && expected.1 > 0,
            "activation requires explicit profile version and current prompt/voice revisions"
        );
        ctx.sql().transaction(|tx|async move {
            if let Some((turn,_))=receipt {ensure!(!tx.execute("SELECT id FROM turns WHERE id=? AND status='running'",params(&[turn])).await?.rows.is_empty(),"turn cancelled");}
            let rows=tx.execute("SELECT prompt,voice FROM persona_versions WHERE name=? AND version=?",Some(vec![P::Text(name.into()),P::Integer(version)])).await?.rows;
            let row=rows.first().context("persona version not found")?;
            let prompt=string(row,0)?;
            let config=serde_json::from_str::<crate::voice::VoiceConfig>(&string(row,1)?)?;
            config.validate()?;
            let voice=serde_json::to_string(&config)?;
            let now=auth::now();
            let reason=format!("Activate {name} version {version}");
            let pr=tx.execute("UPDATE system_prompt SET revision=revision+1,prompt=?,updated=? WHERE singleton=1 AND revision=? RETURNING revision",Some(vec![P::Text(prompt.clone()),P::Integer(now),P::Integer(expected.0)])).await?.rows;
            let prompt_revision=integer(pr.first().context("prompt revision conflict")?,0)?;
            let vr=tx.execute("UPDATE voice_settings SET revision=revision+1,config=?,updated=? WHERE singleton=1 AND revision=? RETURNING revision",Some(vec![P::Text(voice.clone()),P::Integer(now),P::Integer(expected.1)])).await?.rows;
            let voice_revision=integer(vr.first().context("voice revision conflict")?,0)?;
            tx.execute("INSERT INTO prompt_versions(revision,prompt,reason,updated) VALUES(?,?,?,?)",Some(vec![P::Integer(prompt_revision),P::Text(prompt),P::Text(reason.clone()),P::Integer(now)])).await?;
            tx.execute("INSERT INTO voice_versions(revision,config,reason,updated) VALUES(?,?,?,?)",Some(vec![P::Integer(voice_revision),P::Text(voice),P::Text(reason),P::Integer(now)])).await?;
            tx.execute("DELETE FROM prompt_versions WHERE revision<?",Some(vec![P::Integer((prompt_revision-15).max(1))])).await?;
            tx.execute("DELETE FROM voice_versions WHERE revision<?",Some(vec![P::Integer((voice_revision-15).max(1))])).await?;
            let result=json!({"name":name,"version":version,"activated":true,"prompt_revision":prompt_revision,"voice_revision":voice_revision,"prompt_applies":"next_turn","voice_applies":"next_speech_request"});
            if let Some((turn,call))=receipt {tx.execute("UPDATE tool_calls SET result=? WHERE turn_id=? AND call_id=?",params(&[&result.to_string(),turn,call])).await?;}
            Ok(result)
        },Default::default()).await
    }
}

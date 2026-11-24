//! A face cue is a one-shot offer, not durable desired hardware state.
use super::{
    Context, Conversation, Ctx, Db, OutputEvent, Result, Tx, Value, integer, json, named, params,
    write_receipt,
};
use crate::protocol::{ExpressionCue, FACE_EXPRESS};

// Only a marker is retained, so neither reconnect nor restart can replay motion. Even
// another call ID cannot offer a second cue on this turn. events_turn_kind covers the check.
const RESERVE_CUE: &str = r"
    INSERT INTO events(turn_id, kind, payload) SELECT ?1, ?2, '{}'
    WHERE EXISTS(SELECT 1 FROM turns WHERE id = ?1 AND status = 'running')
      AND NOT EXISTS(SELECT 1 FROM events WHERE turn_id = ?1 AND kind = ?2)
    RETURNING sequence";

impl Conversation {
    pub(super) async fn express(
        &self,
        ctx: &Ctx<Self>,
        intent: (&str, &str),
        cue: ExpressionCue,
    ) -> Result<Value> {
        let turn = intent.0;
        let reserve = |tx: Tx| async move {
            let inserted = tx
                .execute(RESERVE_CUE, params(&[turn, FACE_EXPRESS]))
                .await?;
            let row = inserted
                .rows
                .first()
                .context("turn closed or expression already offered")?;
            let sequence = integer(row, 0)?;
            let result = json!({"offered": true});
            write_receipt(&tx, Some(intent), &result).await?;
            Ok((sequence, result))
        };
        let (sequence, result) = ctx
            .db()
            .transaction(reserve, named("conversation.express"))
            .await?;
        // A crash anywhere after commit deliberately loses the animation. The receipt
        // means "offered", never "played". The regular session/revocation checks apply.
        let live = OutputEvent {
            sequence,
            turn_id: turn.into(),
            kind: FACE_EXPRESS.into(),
            payload: serde_json::to_value(cue)?,
        };
        self.deliver(ctx, &live).await;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_running_turn_can_offer_one_cue_and_the_log_is_inert() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch(include_str!("../conversation.sql"))
            .unwrap();
        let reserve = |id| {
            db.prepare(RESERVE_CUE)
                .unwrap()
                .query_map([id, FACE_EXPRESS], |row| row.get::<_, i64>(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        db.execute(
            "INSERT INTO turns(id,request_id,text,status,created) VALUES('t','r','q','queued',0)",
            [],
        )
        .unwrap();
        assert!(reserve("t").is_empty());
        db.execute("UPDATE turns SET status='running' WHERE id='t'", [])
            .unwrap();
        assert_eq!(reserve("t"), [1]);
        assert!(reserve("t").is_empty());
        let payload: String = db
            .query_row("SELECT payload FROM events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(payload, "{}");
    }
}

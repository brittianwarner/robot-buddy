//! Output events reach connections subscribed to `output` whose session is still valid.
use super::{ConnCtx, Conversation, Ctx, Event, OutputEvent, auth, bot_key};
use std::collections::HashMap;

/// Session expiry per connection ID. Each session is verified once, when its connection
/// is first seen, instead of on every event. `None` marks a session that did not verify.
#[derive(Default)]
pub(super) struct Sessions(std::sync::Mutex<HashMap<String, Option<i64>>>);

impl Sessions {
    /// Indexes of the connections to close: their session has expired or never verified.
    /// Connections no longer open are forgotten.
    fn sweep<C>(
        &self,
        conns: &[C],
        id: impl Fn(&C) -> &str,
        verify: impl Fn(&C) -> Option<i64>,
        now: i64,
    ) -> Vec<usize> {
        let mut known = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        known.retain(|known_id, _| conns.iter().any(|conn| id(conn) == known_id));
        conns
            .iter()
            .enumerate()
            .filter_map(|(index, conn)| {
                let expires = *known
                    .entry(id(conn).to_owned())
                    .or_insert_with(|| verify(conn));
                expires.is_none_or(|at| at <= now).then_some(index)
            })
            .collect()
    }

    fn clear(&self) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }
}

impl Conversation {
    /// The expiry of a connection's session, if it verifies for this bot.
    fn session_expiry(&self, conn: &ConnCtx<Self>, bot: &str) -> Option<i64> {
        let params = conn.params().ok()?;
        let session: auth::Session =
            auth::verify(&self.signing_key, "session", &params.session).ok()?;
        (session.bot_id == bot).then_some(session.expires)
    }

    /// Sends a committed event to subscribers. A durable commit must not fail because a
    /// transient subscriber has gone away, so delivery problems are not errors.
    pub(super) async fn deliver(&self, ctx: &Ctx<Self>, event: &OutputEvent) {
        if self.check_live(ctx).await.is_err() {
            // Access may be revoked. Subscribers reconnect through the full check.
            tracing::warn!("live access check failed; closing subscriptions");
            self.sessions.clear();
            for conn in ctx.conns_vec() {
                let _ = conn.disconnect(Some("authorization check failed")).await;
            }
            return;
        }
        let Ok(bot) = bot_key(ctx) else {
            return;
        };
        let conns = ctx.conns_vec();
        let expired = self.sessions.sweep(
            &conns,
            ConnCtx::id,
            |conn| self.session_expiry(conn, &bot),
            auth::now(),
        );
        for index in expired {
            let _ = conns[index].disconnect(Some("authorization expired")).await;
        }
        // Only connections subscribed to `output` receive it.
        if ctx.broadcast(OutputEvent::NAME, event).is_err() {
            tracing::warn!("output event could not be encoded");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn sessions_are_verified_once_per_connection() {
        let sessions = Sessions::default();
        let verified = Cell::new(0);
        let conns = [("a", Some(100)), ("b", Some(50)), ("c", None)];
        let verify = |conn: &(&str, Option<i64>)| {
            verified.set(verified.get() + 1);
            conn.1
        };
        assert_eq!(sessions.sweep(&conns, |c| c.0, verify, 10), [2]);
        assert_eq!(sessions.sweep(&conns, |c| c.0, verify, 60), [1, 2]);
        assert_eq!(verified.get(), 3);
    }

    #[test]
    fn closed_connections_are_forgotten() {
        let sessions = Sessions::default();
        sessions.sweep(&[("a", Some(100))], |c| c.0, |c| c.1, 10);
        // The same ID reused by a new connection is verified again.
        let expired = sessions.sweep(&[("b", Some(100))], |c| c.0, |c| c.1, 10);
        assert!(expired.is_empty());
        assert_eq!(sessions.sweep(&[("a", None)], |c| c.0, |c| c.1, 10), [0]);
    }
}

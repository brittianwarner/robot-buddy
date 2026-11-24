//! Output events reach connections subscribed to `output` whose session is still valid.
//!
//! An event is broadcast only when every open connection has a valid session. If an
//! expired connection could not be closed, the event goes one by one to the valid
//! connections this process saw subscribe; the rest catch up through `Replay`.
use super::{ConnCtx, Conversation, Ctx, Event, OutputEvent, auth, bot_key};
use std::collections::{HashMap, HashSet};

/// Session expiry per connection ID. Each session is verified once, when its connection
/// is first seen, instead of on every event. `None` marks a session that did not verify.
#[derive(Default)]
pub(super) struct Sessions {
    expiry: std::sync::Mutex<HashMap<String, Option<i64>>>,
    /// Connections seen subscribing to `output` in this process.
    subscribed: std::sync::Mutex<HashSet<String>>,
}

/// Who receives one event.
#[derive(Debug, PartialEq, Eq)]
enum Route {
    /// Every open connection is valid: rivetkit sends to its subscribers.
    Broadcast,
    /// An expired connection is still open: send only to these indexes.
    Only(Vec<usize>),
}

/// Decides delivery from the connections still expired after the close attempts.
/// `subscribed` says whether a connection was seen subscribing.
fn route(count: usize, expired: &[usize], subscribed: impl Fn(usize) -> bool) -> Route {
    if expired.is_empty() {
        return Route::Broadcast;
    }
    let valid = (0..count)
        .filter(|index| !expired.contains(index) && subscribed(*index))
        .collect();
    Route::Only(valid)
}

impl Sessions {
    /// Records a connection that subscribed after its session was checked.
    pub(super) fn subscribed(&self, id: &str) {
        self.subscribed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.to_owned());
    }

    fn is_subscribed(&self, id: &str) -> bool {
        self.subscribed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(id)
    }

    /// Indexes of the connections to close: their session has expired or never verified.
    /// Connections no longer open are forgotten.
    fn sweep<C>(
        &self,
        conns: &[C],
        id: impl Fn(&C) -> &str,
        verify: impl Fn(&C) -> Option<i64>,
        now: i64,
    ) -> Vec<usize> {
        let open = |known_id: &String| conns.iter().any(|conn| id(conn) == known_id);
        self.subscribed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(open);
        let mut known = self
            .expiry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        known.retain(|known_id, _| open(known_id));
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
        self.expiry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        self.subscribed
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
        let sweep = |conns: &[ConnCtx<Self>]| {
            let verify = |conn: &ConnCtx<Self>| self.session_expiry(conn, &bot);
            self.sessions.sweep(conns, ConnCtx::id, verify, auth::now())
        };
        let mut conns = ctx.conns_vec();
        let mut expired = sweep(&conns);
        if !expired.is_empty() {
            for index in &expired {
                let _ = conns[*index]
                    .disconnect(Some("authorization expired"))
                    .await;
            }
            // A close can fail. Whatever is still open and expired must not receive this.
            conns = ctx.conns_vec();
            expired = sweep(&conns);
        }
        let subscribed = |index: usize| self.sessions.is_subscribed(conns[index].id());
        match route(conns.len(), &expired, subscribed) {
            // Only connections subscribed to `output` receive it.
            Route::Broadcast => {
                if ctx.broadcast(OutputEvent::NAME, event).is_err() {
                    tracing::warn!("output event could not be encoded");
                }
            }
            Route::Only(valid) => {
                tracing::warn!(
                    skipped = expired.len(),
                    "expired connection still open; sending to valid subscribers only"
                );
                for index in valid {
                    let _ = conns[index].send(OutputEvent::NAME, event);
                }
            }
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

    /// Security regression: an expired subscriber whose close failed is still open on the
    /// next sweep, so the event goes only to the valid subscriber, never as a broadcast.
    #[test]
    fn an_expired_connection_that_failed_to_close_gets_nothing() {
        let sessions = Sessions::default();
        let conns = [("expired", Some(5)), ("valid", Some(100))];
        sessions.subscribed("expired");
        sessions.subscribed("valid");
        let first = sessions.sweep(&conns, |c| c.0, |c| c.1, 10);
        assert_eq!(first, [0]);
        // The disconnect failed: the same connections are open on the re-sweep.
        let still = sessions.sweep(&conns, |c| c.0, |c| c.1, 10);
        assert_eq!(still, [0]);
        let subscribed = |index: usize| sessions.is_subscribed(conns[index].0);
        assert_eq!(route(conns.len(), &still, subscribed), Route::Only(vec![1]));
    }

    #[test]
    fn fallback_skips_connections_never_seen_subscribing() {
        let sessions = Sessions::default();
        let conns = [
            ("expired", Some(5)),
            ("valid", Some(100)),
            ("idle", Some(100)),
        ];
        sessions.subscribed("valid");
        let expired = sessions.sweep(&conns, |c| c.0, |c| c.1, 10);
        let subscribed = |index: usize| sessions.is_subscribed(conns[index].0);
        assert_eq!(
            route(conns.len(), &expired, subscribed),
            Route::Only(vec![1])
        );
    }

    #[test]
    fn all_valid_connections_broadcast() {
        let sessions = Sessions::default();
        let conns = [("a", Some(100)), ("b", Some(100))];
        let expired = sessions.sweep(&conns, |c| c.0, |c| c.1, 10);
        assert_eq!(route(conns.len(), &expired, |_| true), Route::Broadcast);
    }

    #[test]
    fn closed_connections_lose_their_subscription() {
        let sessions = Sessions::default();
        sessions.subscribed("a");
        sessions.sweep(&[("b", Some(100))], |c| c.0, |c| c.1, 10);
        assert!(!sessions.is_subscribed("a"));
    }
}

//! Committed speech read from the Conversation actor over the server's Rivet client.
use super::{
    error,
    pipeline::{Chunk, ChunkSource, Spoken},
};
use anyhow::{Context, Result};
use rivetkit::{
    TypedClientExt,
    client::{Client, GetOptions, connection::ConnectionStatus},
    typed_client::{TypedActorConnection, TypedActorHandle},
};
use robot_buddy::{
    conversation::Conversation,
    protocol::{Call, Command, OutputEvent, SpeechChunkReply, SpokenAnswerReply},
};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::sync::Notify;

/// `SpeechChunk` at this index is a status probe. A turn commits chunks 0..64, so the
/// actor answers 64 from turn status alone, without reading text or the pinned voice.
/// An actor from before the probe reads an absent chunk and gives the same answer.
const STATUS_PROBE: u32 = 64;

/// One typed actor handle per stream. Streaming requests also hold an event
/// connection that wakes the chunk wait on `speech.ready` and `turn.*` for this turn.
pub struct ActorSource {
    handle: TypedActorHandle<Conversation>,
    session: String,
    turn_id: String,
    replay: bool,
    wake: Arc<Notify>,
    events: Option<TypedActorConnection<Conversation>>,
}

impl ActorSource {
    /// `stream: false` is completed-answer replay: chunk 0 is the whole answer.
    pub async fn open(
        client: &Client,
        bot: &str,
        session: &str,
        turn_id: &str,
        stream: bool,
    ) -> Result<Self> {
        let handle = client.get_typed::<Conversation>(
            "conversation",
            [bot],
            GetOptions {
                params: Some(json!({ "session": session })),
            },
        )?;
        let wake = Arc::new(Notify::new());
        let events = if stream {
            Some(subscribe(&handle, turn_id, &wake).await)
        } else {
            None
        };
        Ok(Self {
            handle,
            session: session.into(),
            turn_id: turn_id.into(),
            replay: !stream,
            wake,
            events,
        })
    }

    /// One actor call. Uses the open event WebSocket when connected so `SpeechChunk`
    /// hits the actor's cached authorization instead of `on_before_connect`, falling
    /// back to a stateless handle call if disconnected or on a transport failure.
    /// Its error carries the [`error::SpeechError`] kind.
    async fn call(&self, command: Command) -> Result<Value> {
        if let Some(connection) = &self.events
            && connection.inner().conn_status() == ConnectionStatus::Connected
        {
            let request = Call {
                session: self.session.clone(),
                command: command.clone(),
            };
            match connection.send(request).await {
                Ok(value) => return Ok(value),
                Err(err) if !is_transport_error(&err) => return Err(error::engine(err)),
                Err(_) => {}
            }
        }
        self.handle
            .send(Call {
                session: self.session.clone(),
                command,
            })
            .await
            .map_err(error::engine)
    }

    async fn answer(&self) -> Result<Chunk> {
        let value = self
            .call(Command::SpokenAnswer {
                turn_id: self.turn_id.clone(),
            })
            .await?;
        let SpokenAnswerReply { text, voice, .. } =
            serde_json::from_value(value).context("invalid spoken answer")?;
        Ok(Chunk::Ready(Spoken { text, voice }))
    }
}

/// Events are only a wake-up hint. If the connection never opens, or an event is
/// missed, the pipeline's fallback poll still finds the chunk.
async fn subscribe(
    handle: &TypedActorHandle<Conversation>,
    turn_id: &str,
    wake: &Arc<Notify>,
) -> TypedActorConnection<Conversation> {
    let connection = handle.connect();
    let opened = wake.clone();
    connection
        .inner()
        .on_open(move || opened.notify_one())
        .await;
    let turn_id = turn_id.to_owned();
    let wake = wake.clone();
    // The connection is disconnected on drop, which also ends this subscription.
    let _subscription = connection
        .on::<OutputEvent>(move |event| {
            let relevant = event.kind == "speech.ready" || event.kind.starts_with("turn.");
            if relevant && event.turn_id == turn_id {
                wake.notify_one();
            }
        })
        .await;
    connection
}

impl ChunkSource for ActorSource {
    async fn chunk(&self, index: u32) -> Result<Chunk> {
        if self.replay {
            return if index == 0 {
                self.answer().await
            } else {
                Ok(Chunk::Done)
            };
        }
        let value = self
            .call(Command::SpeechChunk {
                turn_id: self.turn_id.clone(),
                index,
            })
            .await?;
        let reply: SpeechChunkReply =
            serde_json::from_value(value).context("invalid speech state")?;
        Ok(match reply {
            SpeechChunkReply::Ready { text, voice, .. } => Chunk::Ready(Spoken { text, voice }),
            SpeechChunkReply::Pending => Chunk::Pending,
            SpeechChunkReply::Done => Chunk::Done,
        })
    }

    /// `SpeechChunk` re-verifies the session against Conversation and Bot and fails
    /// once the turn is cancelled, failed or interrupted. It works for replay too.
    async fn check(&self) -> Result<()> {
        self.call(Command::SpeechChunk {
            turn_id: self.turn_id.clone(),
            index: STATUS_PROBE,
        })
        .await
        .map(drop)
    }

    async fn changed(&self) {
        self.wake.notified().await;
    }
}

/// Whether a connection RPC error reflects a broken or closed transport rather than
/// an actor/engine response to the action.
fn is_transport_error(error: &anyhow::Error) -> bool {
    let mut saw_rpc = false;
    for cause in error.chain() {
        let message = cause.to_string();
        if message.starts_with("RPC Error(") {
            saw_rpc = true;
            if message.contains(": Connection closed:") {
                return true;
            }
        }
    }
    !saw_rpc
}

impl Drop for ActorSource {
    fn drop(&mut self) {
        let Some(connection) = self.events.take() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        runtime.spawn(async move {
            let _ = tokio::time::timeout(Duration::from_secs(2), connection.disconnect()).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    #[test]
    fn connection_transport_errors_are_distinguished_from_actor_rpc_errors() {
        assert!(is_transport_error(&anyhow!("Socket closed during rpc")));
        assert!(is_transport_error(&anyhow!(
            "RPC Error(ws/closed): Connection closed: ws.closed"
        )));
        assert!(!is_transport_error(&anyhow!(
            "RPC Error(core/internal_error): speech turn stopped, null"
        )));
        assert!(!is_transport_error(&anyhow!(
            "RPC Error(user/busy): busy, null"
        )));
        assert!(!is_transport_error(&anyhow!(
            "RPC Error(actor/not_found): not found, null"
        )));
    }
}

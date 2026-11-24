//! One authenticated connection: bootstrap, the Rivet actor handles, owner
//! registration, and the conversation calls the controller uses.
//!
//! Conversation calls travel over the open actor connection (one WebSocket),
//! each with a deadline. The bot actor is only asked about ownership, over
//! HTTP. When the connection closes or reports an error the device loop is
//! told at once and replaces the session.
use crate::{
    audio::{Media, Playback},
    controller::{CallError, Claim, Registration, Remote, Submitted},
    device::{Connector, Session},
    screen::Screen,
    wifi,
};
use anyhow::{Context, Result};
use rivetkit_client::{
    Client, ClientConfig, GetOptions, SubscriptionHandle, connection::ActorConnection,
    handle::ActorHandle,
};
use robot_buddy::{
    auth::{self, Identity},
    protocol::{
        Bundle, CALL_ACTION, Call, Command, EnrollmentRequest, OUTPUT_EVENT, OutputEvent, busy_turn,
    },
};
use serde_json::{Value, json};
use std::{
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{Notify, mpsc},
    time::{Instant, timeout},
};

const CALL_DEADLINE: Duration = Duration::from_secs(15);
/// A replay this far ahead reads no rows: the cheapest authenticated call.
const PROBE_AFTER: i64 = i64::MAX;
const PUSHED_EVENTS: usize = 256;
/// The lifetime the robot plans a session with is the server's, clamped to
/// this range, so a skewed or odd answer cannot cause a rotation storm or
/// a session that outlives its credentials by much.
const MIN_LIFETIME: i64 = 120;
const MAX_LIFETIME: i64 = 3600;

/// Opens sessions for the device loop and reaches Wi-Fi and the panel.
#[derive(Clone)]
pub struct Link {
    pub http: reqwest::Client,
    pub server: String,
    pub identity: Arc<Identity>,
    pub screen: Screen,
    pub media: Arc<Media>,
    /// Hardware mode: check and set up Wi-Fi through `NetworkManager`.
    pub manage_wifi: bool,
    /// `--simulate`: the engine endpoint may be plain HTTP on loopback.
    pub insecure_local: bool,
}

impl Connector for Link {
    type Remote = Conversation;
    type Speaker = Playback;

    fn connect(&self) -> impl Future<Output = Result<Session<Conversation, Playback>>> + 'static {
        self.clone().open()
    }

    fn link_up(&self) -> impl Future<Output = Option<bool>> + 'static {
        let manage = self.manage_wifi;
        async move {
            if !manage {
                return None;
            }
            match wifi::connected().await {
                Ok(up) => Some(up),
                Err(error) => {
                    tracing::debug!(%error, "Wi-Fi state unavailable");
                    None
                }
            }
        }
    }

    fn setup(&self, window: Duration) -> impl Future<Output = ()> + 'static {
        let (screen, manage) = (self.screen.clone(), self.manage_wifi);
        async move {
            if manage && let Err(error) = wifi::setup(&screen, Some(window)).await {
                tracing::warn!(%error, "Wi-Fi setup failed");
            }
        }
    }

    fn status(&self, message: &'static str) -> impl Future<Output = ()> + 'static {
        let screen = self.screen.clone();
        async move {
            if let Err(error) = screen.show(message, "").await {
                tracing::debug!(%error, "panel update failed");
            }
        }
    }
}

impl Link {
    async fn open(self) -> Result<Session<Conversation, Playback>> {
        let asked = Instant::now();
        let (bundle, server_now) = bootstrap(&self.http, &self.server, &self.identity).await?;
        // Measured on the local monotonic clock from before the request, so
        // neither clock's offset matters.
        let expires = asked + lifetime(bundle.expires, server_now.unwrap_or_else(auth::now));
        // The gateway token and session travel to this endpoint.
        crate::config::safe_url(&bundle.endpoint, self.insecure_local)
            .context("engine endpoint from /v1/enroll refused")?;
        let client = Client::new(
            ClientConfig::new(&bundle.endpoint)
                .namespace(&bundle.namespace)
                .token(&bundle.gateway_token)
                .disable_metadata_lookup(true),
        );
        let options = || GetOptions {
            params: Some(json!({ "session": bundle.session })),
        };
        let bot = client.get_for_id("bot", &bundle.actor_id, options())?;
        let conversation =
            client.get_for_id("conversation", &bundle.conversation_actor_id, options())?;
        let connection = conversation.connect();
        let closed = Arc::new(Notify::new());
        let on_close = closed.clone();
        connection.on_close(move || on_close.notify_one()).await;
        let on_error = closed.clone();
        connection.on_error(move |_| on_error.notify_one()).await;
        let (push, events) = mpsc::channel(PUSHED_EVENTS);
        let overflow = Arc::new(AtomicBool::new(false));
        let subscription = subscribe(&connection, push, overflow.clone()).await;

        let media = self.media.clone();
        tokio::spawn(async move { media.prewarm().await });
        let (playback_events, playback) = mpsc::channel(8);
        let speaker = Playback::new(self.media.clone(), bundle.session.clone(), playback_events);
        let remote = Conversation(Arc::new(Inner {
            connection,
            bot,
            _conversation: conversation,
            _client: client,
            _subscription: subscription,
            session: bundle.session.clone(),
            bot_id: bundle.bot_id.clone(),
            server: self.server,
            screen: self.screen,
            shown: Mutex::new(Shown::Nothing),
        }));
        Ok(Session {
            bot_id: bundle.bot_id,
            remote,
            speaker,
            playback,
            events,
            overflow,
            closed,
            expires,
        })
    }
}

fn lifetime(expires: i64, server_now: i64) -> Duration {
    let seconds = expires
        .saturating_sub(server_now)
        .clamp(MIN_LIFETIME, MAX_LIFETIME);
    Duration::from_secs(u64::try_from(seconds).unwrap_or(0))
}

/// Prove the device identity and receive a connection bundle, with the
/// server's clock from its `Date` header when it sent one.
pub async fn bootstrap(
    http: &reqwest::Client,
    server: &str,
    identity: &Identity,
) -> Result<(Bundle, Option<i64>)> {
    let server = server.trim_end_matches('/');
    let response: Value = http
        .post(format!("{server}/v1/challenge"))
        .json(&identity.certificate)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let challenge = response["challenge"]
        .as_str()
        .context("challenge missing")?
        .to_owned();
    let request = EnrollmentRequest {
        certificate: identity.certificate.clone(),
        proof: identity.proof(&challenge)?,
        challenge,
    };
    let response = http
        .post(format!("{server}/v1/enroll"))
        .json(&request)
        .send()
        .await?
        .error_for_status()?;
    let server_now = response
        .headers()
        .get(reqwest::header::DATE)
        .and_then(|value| value.to_str().ok())
        .and_then(http_date);
    Ok((response.json().await?, server_now))
}

/// Unix seconds of an HTTP `Date` (IMF-fixdate, `Sun, 06 Nov 1994 08:49:37 GMT`).
fn http_date(value: &str) -> Option<i64> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let mut parts = value.split_ascii_whitespace();
    parts.next()?.strip_suffix(',')?;
    let day: i64 = parts.next()?.parse().ok()?;
    let month_name = parts.next()?;
    let month = i64::try_from(MONTHS.iter().position(|m| *m == month_name)? + 1).ok()?;
    let year: i64 = parts.next()?.parse().ok()?;
    let mut clock = parts.next()?.split(':').map(str::parse::<i64>);
    let (hour, minute, second) = (
        clock.next()?.ok()?,
        clock.next()?.ok()?,
        clock.next()?.ok()?,
    );
    if parts.next()? != "GMT" || parts.next().is_some() || clock.next().is_some() {
        return None;
    }
    let valid = (1..=31).contains(&day)
        && (0..24).contains(&hour)
        && (0..60).contains(&minute)
        && (0..=60).contains(&second);
    valid.then(|| days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second)
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// An `output` event carries one [`OutputEvent`] argument.
fn decode_output(args: &[Value]) -> Option<OutputEvent> {
    match args {
        [event] => serde_json::from_value(event.clone()).ok(),
        _ => None,
    }
}

/// Pushes decoded `output` events into `push`. Never blocks the client's event task:
/// a dropped event sets `overflowed`, shows up as a gap and is replayed.
async fn subscribe(
    connection: &ActorConnection,
    push: mpsc::Sender<OutputEvent>,
    overflowed: Arc<AtomicBool>,
) -> SubscriptionHandle {
    connection
        .on_event(OUTPUT_EVENT, move |args| {
            let Some(event) = decode_output(args) else {
                tracing::debug!("undecodable output event dropped");
                return;
            };
            if push.try_send(event).is_err() {
                overflowed.store(true, Ordering::Relaxed);
            }
        })
        .await
}

/// A failed action: the server answered with an error, or nothing came back.
fn classify(message: &str) -> CallError {
    if let Some(turn) = busy_turn(message) {
        return CallError::Busy(turn);
    }
    let answered = (message.starts_with("RPC Error(") || message.starts_with("action failed ("))
        && !message.contains("Connection closed");
    if answered {
        CallError::Rejected(message.to_owned())
    } else {
        CallError::Uncertain(message.to_owned())
    }
}

#[derive(PartialEq, Eq)]
enum Shown {
    Nothing,
    Ready,
    Claim(String),
}

struct Inner {
    connection: ActorConnection,
    bot: ActorHandle,
    _conversation: ActorHandle,
    _client: Client,
    _subscription: SubscriptionHandle,
    session: String,
    bot_id: String,
    server: String,
    screen: Screen,
    /// What this session last put on the panel, so it is drawn once.
    shown: Mutex<Shown>,
}

/// The Conversation actor over its open connection, plus the bot actor.
#[derive(Clone)]
pub struct Conversation(Arc<Inner>);

impl Conversation {
    fn call_for(&self, command: Command) -> Call {
        Call {
            session: self.0.session.clone(),
            command,
        }
    }

    async fn call(&self, command: Command) -> Result<Value, CallError> {
        let call = serde_json::to_value(self.call_for(command))
            .map_err(|e| CallError::Rejected(e.to_string()))?;
        let action = self.0.connection.action(CALL_ACTION, vec![call]);
        match timeout(CALL_DEADLINE, action).await {
            Err(_) => Err(CallError::Uncertain("timed out".into())),
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(classify(&error.to_string())),
        }
    }

    /// One call to the bot actor (ownership), over HTTP.
    async fn bot_call(&self, command: Command) -> Result<Value> {
        let call = serde_json::to_value(self.call_for(command))?;
        timeout(CALL_DEADLINE, self.0.bot.action(CALL_ACTION, vec![call]))
            .await
            .context("bot call timed out")?
    }

    async fn show(&self, wanted: Shown) -> Result<()> {
        if *self
            .0
            .shown
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            == wanted
        {
            return Ok(());
        }
        match &wanted {
            Shown::Nothing => {}
            Shown::Ready => self.0.screen.ready().await,
            Shown::Claim(url) => {
                self.0
                    .screen
                    .show("Scan to register your robot", url)
                    .await?;
            }
        }
        *self
            .0
            .shown
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = wanted;
        Ok(())
    }
}

fn parse_submitted(result: &Value) -> Result<Submitted, CallError> {
    // An answer without a turn ID proves nothing either way.
    let turn_id = result["turn_id"]
        .as_str()
        .ok_or_else(|| CallError::Uncertain("turn id missing".into()))?;
    Ok(Submitted {
        turn_id: turn_id.into(),
        // Anything but an explicit `false` is treated as a replay: silent.
        duplicate: result["duplicate"] != false,
        cancelled: result["cancelled"] == true,
    })
}

impl Remote for Conversation {
    async fn submit(&self, request_id: &str, text: &str) -> Result<Submitted, CallError> {
        let command = Command::Submit {
            request_id: request_id.into(),
            text: text.into(),
        };
        let result = self.call(command).await?;
        parse_submitted(&result)
    }

    async fn cancel(&self, turn_id: &str) -> Result<(), CallError> {
        let command = Command::Cancel {
            turn_id: turn_id.into(),
        };
        self.call(command).await.map(drop)
    }

    async fn cancel_request(&self, request_id: &str) -> Result<(), CallError> {
        let command = Command::CancelRequest {
            request_id: request_id.into(),
        };
        self.call(command).await.map(drop)
    }

    async fn replay(&self, after: i64) -> Result<Vec<OutputEvent>, CallError> {
        let page = self.call(Command::Replay { after }).await?;
        serde_json::from_value(page).map_err(|e| CallError::Rejected(e.to_string()))
    }

    async fn probe(&self) -> Result<(), CallError> {
        self.replay(PROBE_AFTER).await.map(drop)
    }

    async fn confirm(&self, code: &str) -> Result<()> {
        let confirm = Command::ConfirmClaim { code: code.into() };
        self.bot_call(confirm).await.context("confirm")?;
        Ok(())
    }

    /// Keep the panel in step with ownership: the ready message once owned,
    /// otherwise a live claim QR and the owner's confirmation code.
    async fn register(&self, shown: Claim) -> Result<Registration> {
        let snapshot = self
            .bot_call(Command::Snapshot)
            .await
            .context("bot snapshot")?;
        let owner = snapshot["owner"].as_str();
        if owner != Some("") {
            self.show(Shown::Ready).await?;
            return Ok(Registration {
                owned: owner.is_some_and(|o| !o.is_empty()),
                claim: None,
                confirming: false,
            });
        }
        let now = auth::now();
        let actor_expires = snapshot["claim_expires"].as_i64().unwrap_or(0);
        let claim_lapsed = snapshot["pending_owner"].as_str() == Some("") || actor_expires <= now;
        let mut claim = None;
        let mut url = shown.url;
        if now >= shown_claim_expiry(shown.expires, actor_expires) && claim_lapsed {
            let started = self.bot_call(Command::BeginClaim).await.context("claim")?;
            let expires = started["expires"]
                .as_i64()
                .context("claim expiry missing")?;
            let token = started["token"].as_str().context("claim token missing")?;
            let server = self.0.server.trim_end_matches('/');
            let mut link = url::Url::parse(&format!("{server}/claim"))?;
            link.query_pairs_mut()
                .append_pair("bot", &self.0.bot_id)
                .append_pair("token", token);
            url = link.to_string();
            claim = Some(Claim {
                url: url.clone(),
                expires,
            });
        }
        if !url.is_empty() {
            self.show(Shown::Claim(url)).await?;
        }
        let code = snapshot["confirm_code"].as_str().unwrap_or("");
        // The optional loopback panel's button; the spoken code is the other way.
        if self.0.screen.confirmation(code).await && !code.is_empty() {
            self.confirm(code).await?;
        }
        let confirming = !code.is_empty()
            && snapshot["pending_owner"]
                .as_str()
                .is_some_and(|o| !o.is_empty())
            && actor_expires > now;
        Ok(Registration {
            owned: false,
            claim,
            confirming,
        })
    }

    async fn disconnect(&self) {
        self.0.connection.disconnect().await;
    }
}

/// The expiry of the claim the screen shows, as far as the bot still knows it. The bot
/// clears a claim after five wrong codes (expiry 0), and a different expiry means the
/// cached QR is not the bot's claim: both read as expired, so a new claim starts at once.
fn shown_claim_expiry(cached: i64, actor: i64) -> i64 {
    if cached == actor { cached } else { 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use robot_buddy::protocol::Busy;

    #[test]
    fn a_cleared_claim_is_replaced_at_once() {
        // The claim on screen is still live in the bot.
        assert_eq!(shown_claim_expiry(1_000, 1_000), 1_000);
        // Five wrong codes cleared it; the cached QR is dead.
        assert_eq!(shown_claim_expiry(1_000, 0), 0);
        // The bot holds a claim this cache never saw.
        assert_eq!(shown_claim_expiry(1_000, 2_000), 0);
        assert_eq!(shown_claim_expiry(0, 0), 0);
    }

    #[test]
    fn request_cancels_and_probes_use_the_shared_wire_shape() {
        let call = |command| {
            serde_json::to_value(Call {
                session: "s".into(),
                command,
            })
            .unwrap()
        };
        assert_eq!(
            call(Command::CancelRequest {
                request_id: "r".into()
            }),
            json!({ "session": "s", "command": { "type": "cancel_request", "request_id": "r" } })
        );
        assert_eq!(
            call(Command::Replay { after: PROBE_AFTER })["command"]["after"],
            i64::MAX
        );
    }

    #[test]
    fn calls_keep_the_typed_wire_shape() {
        let call = Call {
            session: "s".into(),
            command: Command::Replay { after: 7 },
        };
        let encoded = serde_json::to_value(&call).unwrap();
        assert_eq!(
            encoded,
            json!({ "session": "s", "command": { "type": "replay", "after": 7 } })
        );
        assert_eq!((CALL_ACTION, OUTPUT_EVENT), ("call", "output"));
    }

    #[test]
    fn output_events_decode_from_one_argument() {
        let event = json!({ "sequence": 3, "turn_id": "t", "kind": "text.delta", "payload": {} });
        let decoded = decode_output(std::slice::from_ref(&event)).unwrap();
        assert_eq!((decoded.sequence, decoded.kind.as_str()), (3, "text.delta"));
        assert!(decode_output(&[]).is_none());
        assert!(decode_output(&[event.clone(), event]).is_none());
    }

    #[test]
    fn http_dates_parse_to_unix_seconds() {
        assert_eq!(
            http_date("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(784_111_777)
        );
        assert_eq!(http_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(
            http_date("Thu, 29 Feb 2024 23:59:59 GMT"),
            Some(1_709_251_199)
        );
        for bad in [
            "",
            "Sun, 06 Nov 1994 08:49:37 UTC",
            "Sunday, 06-Nov-94 08:49:37 GMT",
            "Sun Nov  6 08:49:37 1994",
            "Sun, 06 Nov 1994 25:49:37 GMT",
            "Sun, 06 Foo 1994 08:49:37 GMT",
        ] {
            assert_eq!(http_date(bad), None, "{bad}");
        }
    }

    #[test]
    fn session_lifetime_uses_the_server_clock_within_bounds() {
        assert_eq!(lifetime(1_900, 1_000), Duration::from_mins(15));
        // A robot clock hours off does not matter: only the server's is used.
        assert_eq!(lifetime(1_000, 2_000), Duration::from_mins(2));
        assert_eq!(lifetime(100_000, 0), Duration::from_hours(1));
    }

    #[test]
    fn a_busy_refusal_names_the_open_turn() {
        // The rivetkit-client WebSocket format: `{:#}` of the metadata value.
        let metadata = serde_json::to_value(Busy {
            turn_id: "turn-1".into(),
        })
        .unwrap();
        let message = format!("RPC Error(user/busy): Another turn is still open., {metadata:#}");
        assert!(matches!(classify(&message), CallError::Busy(t) if t == "turn-1"));
        // Unknown metadata fields are ignored; missing metadata is a plain refusal.
        let newer = r#"RPC Error(user/busy): Busy., {"turn_id": "t", "since": 1}"#;
        assert!(matches!(classify(newer), CallError::Busy(t) if t == "t"));
        // A brace in the message text does not hide the metadata.
        let braced = r#"RPC Error(user/busy): Busy {still open}., {"turn_id": "t2"}"#;
        assert!(matches!(classify(braced), CallError::Busy(t) if t == "t2"));
        for refusal in [
            "RPC Error(user/busy): Busy., null",
            "RPC Error(internal/error): busy, null",
        ] {
            assert!(
                matches!(classify(refusal), CallError::Rejected(_)),
                "{refusal}"
            );
        }
    }

    #[test]
    fn only_server_answers_count_as_rejections() {
        let rejected = [
            "RPC Error(actor/internal_error): Internal error., null",
            "action failed (actor/internal_error): Internal error.",
        ];
        for message in rejected {
            assert!(
                matches!(classify(message), CallError::Rejected(_)),
                "{message}"
            );
        }
        let uncertain = [
            "RPC Error(ws/closed): Connection closed: going away",
            "Socket closed during rpc",
            "timed out",
        ];
        for message in uncertain {
            assert!(
                matches!(classify(message), CallError::Uncertain(_)),
                "{message}"
            );
        }
    }

    #[test]
    fn submit_reply_honors_cancelled_and_duplicate_flags() {
        let normal = parse_submitted(&json!({ "turn_id": "t1", "duplicate": false })).unwrap();
        assert_eq!(normal.turn_id, "t1");
        assert!(!normal.duplicate);
        assert!(!normal.cancelled);

        let cancelled = parse_submitted(&json!({
            "turn_id": "t2",
            "duplicate": false,
            "cancelled": true
        }))
        .unwrap();
        assert_eq!(cancelled.turn_id, "t2");
        assert!(!cancelled.duplicate);
        assert!(cancelled.cancelled);

        let missing_dup = parse_submitted(&json!({ "turn_id": "t3" })).unwrap();
        assert!(missing_dup.duplicate);
        assert!(!missing_dup.cancelled);

        assert!(matches!(
            parse_submitted(&json!({ "duplicate": false })),
            Err(CallError::Uncertain(_))
        ));
    }
}

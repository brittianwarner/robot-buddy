//! Owner claim: `GET /claim` (the robot's QR link), `POST /login` (start OIDC) and
//! `GET /auth/callback` (finish it and propose the claim to the robot).
//!
//! A sign-in in flight lives here, server-side, keyed by an opaque random id. The
//! `buddy_login` cookie carries only that id, never the claim token, PKCE verifier or
//! nonce. Each entry is single-use and expires after [`LOGIN_TTL`]; each client holds
//! at most [`PER_CLIENT`], so no client can crowd out another's sign-in.
use super::{App, Command, Secret, config::origin_of, limit::Client, oidc, web};
use axum::{
    Extension,
    body::Body,
    extract::{Query, State},
    http::{HeaderMap, HeaderValue, header},
    response::{IntoResponse, Redirect, Response},
};
use robot_buddy::auth;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashMap, sync::Arc, sync::Mutex};
use web::{ApiError, CookieSpec, Kind, Reject, Web, cookie, escape, page, require, same_origin};

const CLAIM_TTL: i64 = 600;
/// Lifetime of one sign-in transaction (login cookie, OAuth `state`), spent once.
pub const LOGIN_TTL: i64 = 300;
/// Sign-ins in flight per client (an IPv4 address or IPv6 /64). A new one replaces that
/// client's oldest.
const PER_CLIENT: usize = 8;
/// Sign-ins in flight in total. When full, the oldest is dropped: it can only fail at
/// its callback, never be replayed.
const MAX_LOGINS: usize = 16_384;

#[derive(Deserialize)]
pub struct ClaimQuery {
    bot: String,
    token: String,
}
#[derive(Deserialize)]
struct LoginForm {
    bot: String,
    token: String,
    csrf: String,
}

/// Binds `GET /claim` to `POST /login`: the signed value names the claim it was issued
/// for, and its CSRF value must come back in the form.
#[derive(Serialize, Deserialize)]
struct ClaimForm {
    bot: String,
    /// Digest only: the cookie never carries the QR claim token itself.
    token_digest: String,
    csrf: String,
    expires: i64,
}

/// One sign-in in flight.
pub struct Login {
    bot: String,
    token: Secret<String>,
    /// OAuth `state`, compared with the callback query.
    state: String,
    /// OIDC `nonce`, compared with the ID token. Independent of `state`.
    nonce: String,
    verifier: Secret<String>,
    started: i64,
    client: Client,
}

/// Sign-ins in flight, keyed by the digest of their cookie id.
pub struct Logins {
    pending: Mutex<HashMap<String, Login>>,
    max: usize,
    per_client: usize,
}

impl Default for Logins {
    fn default() -> Self {
        Self {
            pending: Mutex::default(),
            max: MAX_LOGINS,
            per_client: PER_CLIENT,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Take {
    Found,
    /// Unknown, expired or already spent.
    Missing,
    /// The state does not match. The entry stays, so a forged callback cannot cancel the
    /// owner's real sign-in.
    Mismatch,
}

impl Logins {
    /// Stores `login` and returns the cookie id that names it.
    fn begin(&self, login: Login, now: i64) -> String {
        let mut pending = lock(&self.pending);
        pending.retain(|_, l| l.started + LOGIN_TTL > now);
        let client = login.client;
        let mine = pending.values().filter(|l| l.client == client).count();
        if mine >= self.per_client {
            evict_oldest(&mut pending, |l| l.client == client);
        }
        if pending.len() >= self.max {
            evict_oldest(&mut pending, |_| true);
        }
        let id = format!("{}{}", auth::random(), auth::random()).replace('-', "");
        pending.insert(auth::digest(&id), login);
        id
    }

    /// Spends the sign-in named by `id` if `state` matches.
    fn take(&self, id: &str, state: &str, now: i64) -> (Take, Option<Login>) {
        let mut pending = lock(&self.pending);
        let key = auth::digest(id);
        let Some(login) = pending.get(&key) else {
            return (Take::Missing, None);
        };
        if login.started + LOGIN_TTL <= now {
            pending.remove(&key);
            return (Take::Missing, None);
        }
        if !auth::ct_eq(login.state.as_bytes(), state.as_bytes()) {
            return (Take::Mismatch, None);
        }
        (Take::Found, pending.remove(&key))
    }
}

fn evict_oldest(pending: &mut HashMap<String, Login>, which: impl Fn(&Login) -> bool) {
    let oldest = pending
        .iter()
        .filter(|(_, l)| which(l))
        .min_by_key(|(_, l)| l.started)
        .map(|(k, _)| k.clone());
    if let Some(key) = oldest {
        pending.remove(&key);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn claim_cookie(app: &App) -> CookieSpec {
    CookieSpec {
        name: "buddy_claim",
        path: "/login",
        same_site: "Strict",
        max_age: CLAIM_TTL,
        secure: app.secure_cookies(),
    }
}
fn login_cookie(app: &App) -> CookieSpec {
    // Lax: the identity provider returns with a cross-site top-level GET.
    CookieSpec {
        name: "buddy_login",
        path: "/auth/callback",
        same_site: "Lax",
        max_age: LOGIN_TTL,
        secure: app.secure_cookies(),
    }
}
fn valid_claim(bot: &str, token: &str) -> bool {
    (1..=128).contains(&bot.len()) && (1..=128).contains(&token.len())
}
/// The page CSP with nothing loadable; `form_action` names where its form may post.
fn page_csp(form_action: &str) -> String {
    format!(
        "default-src 'none'; form-action {form_action}; frame-ancestors 'none'; base-uri 'none'"
    )
}

pub async fn claim(State(app): State<Arc<App>>, Query(q): Query<ClaimQuery>) -> Web<Response> {
    require(valid_claim(&q.bot, &q.token), Kind::BadRequest)?;
    let Some(oidc) = app.oidc.as_ref() else {
        return Ok(page(
            "<p>The server administrator must configure sign-in before registration can finish. \
             Your robot will retain its identity.</p>",
        ));
    };
    let form = ClaimForm {
        bot: q.bot,
        token_digest: auth::digest(&q.token),
        csrf: auth::random(),
        expires: auth::now() + CLAIM_TTL,
    };
    let mut response = page(&format!(
        "<p>Connect this robot to your account. Keep the robot nearby to confirm.</p>\
         <form method=post action=/login>\
         <input type=hidden name=bot value=\"{}\">\
         <input type=hidden name=token value=\"{}\">\
         <input type=hidden name=csrf value=\"{}\">\
         <button>Sign in to register</button></form>",
        escape(&form.bot),
        escape(&q.token),
        escape(&form.csrf)
    ));
    // Browsers apply form-action to the login POST's redirect chain too.
    let authorize_origin = origin_of(&oidc.authorize).reject(Kind::Internal)?;
    let csp = page_csp(&format!("'self' {authorize_origin}"));
    let signed = auth::sign(app.key.expose(), "claim-form", &form).reject(Kind::Internal)?;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_str(&csp).reject(Kind::Internal)?,
    );
    // `no-referrer` would make the form POST send `Origin: null`. `same-origin` keeps the
    // claim URL (with its token) away from every other origin.
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(
        header::SET_COOKIE,
        claim_cookie(&app).set(&signed).reject(Kind::Internal)?,
    );
    Ok(response)
}

/// Origin and the signed claim cookie are checked from headers before the form body is
/// read.
pub async fn login(
    State(app): State<Arc<App>>,
    Extension(client): Extension<Client>,
    headers: HeaderMap,
    body: Body,
) -> Web<Response> {
    // A cross-site auto-submitted form would otherwise bind the victim's IdP session to
    // the attacker's robot.
    require(same_origin(&app.public_origin, &headers), Kind::Forbidden)?;
    let oidc = app.oidc.as_ref().reject(Kind::Unavailable)?;
    let raw = cookie(&headers, "buddy_claim").reject(Kind::Forbidden)?;
    let form: ClaimForm =
        auth::verify(app.key.expose(), "claim-form", raw).reject(Kind::Forbidden)?;
    require(form.expires > auth::now(), Kind::Forbidden)?;
    let q: LoginForm = web::form(body).await?;
    require(valid_claim(&q.bot, &q.token), Kind::BadRequest)?;
    require(
        auth::ct_eq(form.csrf.as_bytes(), q.csrf.as_bytes())
            && form.bot == q.bot
            && auth::ct_eq(
                form.token_digest.as_bytes(),
                auth::digest(&q.token).as_bytes(),
            ),
        Kind::Forbidden,
    )?;
    let login = Login {
        bot: q.bot,
        token: Secret::new(q.token),
        state: auth::random(),
        nonce: auth::random(),
        verifier: Secret::new(format!("{}{}", auth::random(), auth::random())),
        started: auth::now(),
        client,
    };
    let mut url = url::Url::parse(&oidc.authorize).reject(Kind::Internal)?;
    url.query_pairs_mut().extend_pairs([
        ("client_id", oidc.client_id.as_str()),
        ("response_type", "code"),
        ("scope", "openid profile"),
        ("redirect_uri", &app.callback_url()),
        ("state", &login.state),
        ("nonce", &login.nonce),
        ("code_challenge", &auth::digest(login.verifier.expose())),
        ("code_challenge_method", "S256"),
    ]);
    let id = app.logins.begin(login, auth::now());
    let mut response = Redirect::to(url.as_str()).into_response();
    let headers = response.headers_mut();
    headers.append(
        header::SET_COOKIE,
        login_cookie(&app).set(&id).reject(Kind::Internal)?,
    );
    headers.append(
        header::SET_COOKIE,
        claim_cookie(&app).clear().reject(Kind::Internal)?,
    );
    Ok(response)
}

#[derive(Deserialize)]
pub struct Callback {
    code: String,
    state: String,
}

pub async fn callback(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(q): Query<Callback>,
) -> Web<Response> {
    let id = cookie(&headers, "buddy_login").reject(Kind::Unauthorized)?;
    let oidc = app.oidc.as_ref().reject(Kind::Unavailable)?;
    // Spent before the exchange: whatever happens next, this sign-in cannot be replayed.
    let login = match app.logins.take(id, &q.state, auth::now()) {
        (Take::Found, Some(login)) => login,
        (Take::Mismatch, _) => return Err(ApiError(Kind::Forbidden)),
        _ => return Err(ApiError(Kind::Unauthorized)),
    };
    let redirect = app.callback_url();
    let token: Value = async {
        let form = oidc.token_form(&q.code, login.verifier.expose(), &redirect);
        oidc::json(app.http.post(&oidc.token).form(&form).send().await?).await
    }
    .await
    .reject(Kind::BadGateway)?;
    let access = token["access_token"].as_str().reject(Kind::BadGateway)?;
    let user: Value = async {
        oidc::json(
            app.http
                .get(&oidc.userinfo)
                .bearer_auth(access)
                .send()
                .await?,
        )
        .await
    }
    .await
    .reject(Kind::BadGateway)?;
    let subject = user["sub"].as_str().reject(Kind::BadGateway)?;
    let id_token = token["id_token"].as_str().reject(Kind::BadGateway)?;
    let expected = oidc::Expected {
        nonce: &login.nonce,
        started: login.started,
    };
    oidc::validate(&app.http, oidc, expected, id_token, subject)
        .await
        .reject(Kind::Unauthorized)?;
    let owner = format!("{}|{subject}", oidc.issuer);
    let proposed = app
        .call(
            &login.bot,
            Command::ProposeClaim {
                token: login.token.expose().clone(),
                owner,
            },
        )
        .await
        .reject(Kind::BadGateway)?;
    let code = proposed["code"].as_str().reject(Kind::BadGateway)?;
    let mut response = page(&format!(
        "<p>Match this code on your robot and press its confirm control:</p><h2>{}</h2>\
         <p>The robot will welcome you when registration is complete.</p>",
        escape(code)
    ));
    response.headers_mut().insert(
        header::SET_COOKIE,
        login_cookie(&app).clear().reject(Kind::Internal)?,
    );
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn login(client: Client, started: i64, state: &str) -> Login {
        Login {
            bot: "bot".into(),
            token: Secret::new("t".into()),
            state: state.into(),
            nonce: "n".into(),
            verifier: Secret::new("v".into()),
            started,
            client,
        }
    }
    fn client(ip: &str) -> Client {
        Client::of(ip.parse().unwrap())
    }

    #[test]
    fn a_sign_in_is_spent_once_and_a_forged_state_leaves_it() {
        let logins = Logins::default();
        let id = logins.begin(login(client("192.0.2.1"), 100, "s"), 100);
        assert!(id.len() >= 64 && !id.contains('-'), "opaque id");
        assert_eq!(logins.take(&id, "forged", 101).0, Take::Mismatch);
        assert_eq!(logins.take("unknown", "s", 101).0, Take::Missing);
        let (found, spent) = logins.take(&id, "s", 102);
        assert_eq!(found, Take::Found);
        assert_eq!(spent.unwrap().bot, "bot");
        assert_eq!(logins.take(&id, "s", 103).0, Take::Missing, "single use");
        let late = logins.begin(login(client("192.0.2.1"), 100, "s"), 100);
        assert_eq!(
            logins.take(&late, "s", 100 + LOGIN_TTL).0,
            Take::Missing,
            "expired"
        );
    }

    /// One subnet churning sign-ins displaces only its own; a full table drops the oldest
    /// instead of refusing everyone.
    #[test]
    fn sign_ins_are_bounded_per_client_and_never_refused() {
        let logins = Logins {
            max: 12,
            ..Logins::default()
        };
        let victim = logins.begin(login(client("198.51.100.7"), 1, "v"), 1);
        let mut attacker = Vec::new();
        for i in 0..50u16 {
            // Every address in one /64 is one client.
            let (ip, at) = (format!("2001:db8:1:2::{i:x}"), 2 + i64::from(i));
            attacker.push(logins.begin(login(client(&ip), at, "a"), at));
        }
        assert_eq!(lock(&logins.pending).len(), 1 + PER_CLIENT);
        assert_eq!(logins.take(&victim, "v", 60).0, Take::Found, "victim kept");
        let newest = attacker.last().unwrap();
        assert_eq!(logins.take(newest, "a", 60).0, Take::Found);
        assert_eq!(
            logins.take(&attacker[0], "a", 60).0,
            Take::Missing,
            "oldest gone"
        );
        // Many clients: the table stays at its bound, the oldest go first and the newest
        // sign-in works.
        for i in 0..40u8 {
            let at = 100 + i64::from(i);
            let _ = logins.begin(login(client(&format!("203.0.113.{i}")), at, "x"), at);
        }
        let last = logins.begin(login(client("192.0.2.200"), 200, "y"), 200);
        assert_eq!(lock(&logins.pending).len(), 12);
        assert_eq!(logins.take(&last, "y", 201).0, Take::Found);
        assert_eq!(logins.take(&attacker[49 - 1], "a", 201).0, Take::Missing);
    }
}

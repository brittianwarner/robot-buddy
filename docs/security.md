# Security

Threat model and controls for the server, the actors and the robot's local pages. Each control names the test that guards it.

## Assets

- **Server secrets.** `BUDDY_SIGNING_KEY` signs sessions, challenges and cookies. `RIVET_TOKEN` administers the engine. Provider keys pay for inference and speech. S3 keys reach object storage.
- **Device identity.** Each robot's Ed25519 private key and its issuer-signed certificate.
- **Ownership.** Which account owns which robot. An owner sees the robot's conversation and memory.
- **Conversation and memory.** Transcripts and saved facts.
- **Home Wi-Fi credentials.** Entered on the robot's setup page; they never leave the robot.

## Trust boundaries

| Boundary | Who is on the far side | What crosses it |
|---|---|---|
| Public HTTPS (`/v1/*`, `/claim`, `/login`, `/auth/callback`, `/lab`) | Anyone on the internet | Certificates, proofs, claim tokens, OIDC codes |
| Rivet gateway | Robots with a scoped gateway token | Actor actions with a signed session |
| Identity provider | The configured OIDC issuer | Authorization codes, ID tokens, userinfo |
| Robot loopback panel (`127.0.0.1:8090`) | The robot's kiosk browser, and any web page that browser opens | Owner confirmation |
| Robot `SoftAP` page (`http://10.42.0.1`) | Phones on the setup hotspot | Home Wi-Fi credentials |
| Data volume (`/data`) | Backups and operators | Engine logs and objects |

## Controls

### Owner registration

| Control | Defends against | Test |
|---|---|---|
| `POST /login` requires `Origin` equal to the public origin. | Login CSRF: a cross-site form that binds the victim's IdP session to the attacker's robot. | `http_tests::login_rejects_cross_site_and_unbound_posts` |
| `GET /claim` sets a signed, `HttpOnly`, `SameSite=Strict` cookie bound to the bot, a digest of the claim token and a random CSRF value. The form returns the CSRF value. `/login` checks all three and clears the cookie. | The same attack from a page that can forge `Origin` (none today), and replays of an old form. | same |
| The claim page uses `Referrer-Policy: same-origin`. | `no-referrer` makes browsers send `Origin: null` on the form POST. `same-origin` keeps the claim URL from other origins. | `http_tests::open_claim` helper |
| OAuth `state` and OIDC `nonce` are separate random values. Both compare in constant time. | Using one leaked value for both checks. | `http_tests::login_redirects_with_independent_state_and_nonce`, `oidc::tests::validates_signature_and_login_binding` |
| ID tokens: RS256/ES256 only, `kid` required; when present, JWK `use` must be `sig`, JWK `key_ops` must include `verify` and JWK `alg` must match. `nbf` checked, 30 s leeway. | Encryption keys, keys restricted to other operations, mismatched algorithms; tokens used early. | `oidc::tests::rejects_encryption_keys_and_mismatched_alg` |
| A login transaction (cookie, `state`) lives 5 minutes and is spent once. The callback spends it after the `state` check and before the token exchange, so a failed exchange needs a fresh sign-in. The spent set is process-local and capped at 4096 live entries; when full, the callback answers 429 rather than forget an entry. `/auth/callback` is rate-limited like `/login`. | Replaying one login cookie to make the server call the IdP repeatedly. | `http_tests::login_redirects_with_independent_state_and_nonce`, `http_tests::the_callback_is_rate_limited_per_client`, `oidc::tests::a_login_transaction_is_spent_once_and_the_ledger_is_bounded` |
| Issuer keys are cached for 10 minutes. An unknown `kid` refetches once, at most every 30 s. | Fetching JWKS per login; forged `kid`s turning the server into a request amplifier. | `oidc::tests::caches_keys_and_refetches_once_for_unknown_kid` |
| The token request omits `client_secret` when none is configured. | Sending an empty secret, which some providers reject. | `oidc::tests::token_form_omits_an_empty_secret` |
| The six-digit code is uniform from the OS generator. Five wrong codes clear the pending claim, its code and its QR token. | Guessing the code by a direct Rivet caller. The code is also shown to the device session, so this is defense in depth. | `actor::tests::five_wrong_codes_invalidate_the_pending_claim` |

Residual risk: a person who opens an attacker's claim link and chooses to sign in still proposes themselves as owner of the attacker's robot, which the attacker can confirm. The flow trusts the person who scans the code. The page asks them to keep the robot nearby and match the code on it.

### Device enrollment and sessions

| Control | Defends against | Test |
|---|---|---|
| Certificates are issuer-signed; proofs sign a short-lived challenge; the Bot actor consumes each nonce once. | Forged devices, replayed proofs. | `auth::tests::certificate_and_proof_bind_device_key`, integration suite |
| Signing keys must be at least 32 bytes, in `sign`, `verify`, `Bot::create` and `Memory::create`. | A short key that makes every session forgeable. | `auth::tests::short_keys_are_rejected_for_sign_and_verify` |
| Signing keys, the engine token, the lab token, the OIDC client secret and the device private key are `Secret<T>`: `Debug` prints `[redacted]` and the buffer is zeroized on drop (`zeroize`). Copies a library makes, such as Rig's API key, are outside this guarantee. | Secrets in logs, panics and debug output. | `auth::tests::secrets_are_redacted_and_round_trip_identity_files` |

### HTTP surface

| Control | Defends against | Test |
|---|---|---|
| Token bucket per client IP and route on `/v1/challenge`, `/v1/enroll`, `/login`, `/auth/callback`, `/lab/api/login`: a burst of 10, then one request per 6 s. At most 4096 clients are tracked; idle buckets go first. | Credential guessing and sign-in floods. | `limit::tests::*`, `http_tests::credential_endpoints_are_rate_limited_per_client` |
| The client IP is the socket peer. `X-Forwarded-For` is read only when the peer is listed in `BUDDY_TRUSTED_PROXY`, and then only its rightmost untrusted entry. | Spoofed forwarding headers. | `limit::tests::forwarded_for_is_used_only_from_trusted_proxies` |
| 128 requests in flight, 30 s to the response head, for every route. A permit is held until the head is sent; a streaming PCM body does not hold one. | Unbounded work queues; slow handlers. | `http_tests::only_an_extended_request_outlives_the_header_deadline` |
| Speech authenticates from headers (device bearer; lab cookie, `Origin` and CSRF) before reading the body, then reads at most 1 KiB within 5 s. Only after it takes one of the four speech stream slots does it return its shared permit and get 330 s for the head, above the pipeline's own 300 s deadline. | Slow, incomplete speech bodies holding the shared pool and starving enrollment, sign-in and health checks. | `http_tests::slow_unauthenticated_speech_bodies_cannot_delay_enrollment_or_health`, `http_tests::an_authenticated_speech_body_gets_a_short_read_deadline` |
| `/health` is liveness and touches nothing else; `/ready` checks the engine and answers 503 without detail when it is down. The container health check uses `/ready`. | Liveness probes and connection warm-ups loading the engine. | `http_tests::missing_configuration_is_503_and_disabled_lab_is_404` |
| Typed errors: 400 invalid input, 401 bad or missing credentials, 403 origin/CSRF/allowlist, 404 disabled lab, 429 limits, 502 engine or identity provider, 503 not configured. Bodies are fixed text; causes are dropped unlogged. | Leaking internal error text, URLs or tokens. | `web::tests::server_errors_hide_details`, `http_tests::missing_configuration_is_503_and_disabled_lab_is_404` |
| Cookies read across every `Cookie` header. Cookie clears keep `Secure` on HTTPS. | Missed cookies on HTTP/2; insecure clearing headers. | `web::tests::*` |

### Lab

| Control | Defends against | Test |
|---|---|---|
| The operator token and the guess are each MACed under the signing key, with the token as the message, and compared in constant time. | `token` and `token\0` colliding when the token was the HMAC key. | `lab::tests::lab_token_rejects_nul_padding`, `http_tests::lab_login_and_csrf_statuses` |
| Mutations need `Origin` and the session's CSRF header, compared in constant time. Actions pass an allowlist. | Cross-site requests; admin actions from the browser. | `http_tests::lab_login_and_csrf_statuses` |
| A failed poll or session check clears the watched turn. Catch-up events after reconnect apply silently; Replay still speaks on request. | Speaking a reply that completed during an outage. | `bun apps/server/tests/lab-reconnect.mjs` (browser, not in CI) |

### Actor bounds

Direct Rivet callers skip the server's body limit, so actors check their own inputs.

| Input | Bound | Test |
|---|---|---|
| `Enroll` | device ID 1–128 bytes, public key 1–64 bytes | `actor::tests::codes_are_six_digits_and_inputs_are_bounded` |
| `ProposeClaim` | token 1–128, owner 1–512 | same |
| `ConfirmClaim` | exactly six ASCII digits | same |
| `Remember` | key 1–128, value 1–2048, at most 500 facts (updates always allowed) | `memory::tests::fact_cap_rejects_new_keys_but_allows_updates` |
| `Forget` | key 1–128 | `memory::tests::inputs_are_bounded` |
| `SearchMemory` | 8192 bytes (the `Submit` cap), 16 terms, 128 bytes per term | same |

### Engine and container

| Control | Defends against | Test |
|---|---|---|
| The engine config (admin token, S3 keys) is written to `$XDG_RUNTIME_DIR` or the temp directory, never `/data`. The file is new, `0600`, uniquely named and removed on shutdown. A config left in `/data` by an older release is deleted at start. | Secrets in volume backups. | `http_tests::engine_config_is_private_ephemeral_and_removed` |
| The engine stops with a direct `SIGTERM`; the image no longer ships `procps`. | Spawning `kill(1)`; a smaller image. | build only |
| `RIVET_ENDPOINT`, which carries the engine admin token, must be HTTPS. Plain HTTP is accepted only to loopback (`127.0.0.0/8`, `::1`, `localhost`) or to the single host named by `RIVET_ENDPOINT_PLAINTEXT_HOST`, for a private Docker-network service. No credentials or fragment. | The admin token crossing a network in plaintext. | `http_tests::the_engine_endpoint_needs_tls_off_loopback` |

`compose.yaml` and `compose.local.yaml` mount a `tmpfs` on `/tmp`, so the engine config never touches disk. Do the same in any other production runtime.

### Robot local pages

| Control | Defends against | Test |
|---|---|---|
| Face panel and setup page send `Cache-Control: no-store`, a `default-src 'none'` CSP, `X-Frame-Options: DENY` and `Referrer-Policy: same-origin`. | Framing, script injection, cached codes. | `screen::tests::page_is_locked_down`, `wifi::tests::setup_form_requires_host_origin_and_csrf` |
| Face panel answers only `Host` `127.0.0.1:8090` or `localhost:8090`. Confirm also needs a matching `Origin`. | DNS rebinding and cross-site POSTs from pages the kiosk opens. | `screen::tests::confirm_checks_origin_host_and_rotating_csrf` |
| The face CSRF token rotates with each new confirmation code, and a press for an old code is dropped. | Confirming a later claim with an earlier press or token. | same |
| The setup form POST needs `Host` `10.42.0.1` and `Origin` `http://10.42.0.1`. A missing `Origin` is refused: phone browsers send it on form POSTs. | Cross-site writes of Wi-Fi credentials. | `wifi::tests::setup_form_requires_host_origin_and_csrf` |
| `BUDDY_WIFI_INTERFACE` must match `^[A-Za-z0-9][A-Za-z0-9_-]{0,14}$`, checked before every use. nmcli takes no `--`; every positional value is a constant, our own path, or this name. | An interface name parsed as an nmcli option or breaking a keyfile line. | `wifi::tests::interface_names_cannot_be_options` |
| Keyfile values escape `\\`, spaces, `\n`, `\t` and `\r`. An SSID with `;` or `\` is written as a byte list. | Keyfile injection and SSIDs NetworkManager would misread. | `wifi::tests::escapes_network_values`, `wifi::tests::profile_text_is_unchanged` |

## Not covered

- No live browser, IdP, hardware or container run validates these controls. The tests above are unit and router tests.
- The rate limiter and the spent-login set are per process. They do not share state across replicas.
- A cloned device credential cannot be told apart from the original.

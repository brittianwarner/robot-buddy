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
| Robot loopback panel (`127.0.0.1:8090` by default, `BUDDY_SCREEN_ADDR`) | The robot's kiosk browser, and any web page that browser opens | Owner confirmation |
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
| A login transaction (server-side state, cookie, `state`) lives 5 minutes and is spent once. Server-side login state is bounded to 8 per client and 16,384 total (oldest evicted). The callback spends it after the `state` check and before the token exchange, so a failed exchange needs a fresh sign-in. `/auth/callback` is rate-limited like `/login`. | Replaying one login cookie to make the server call the IdP repeatedly; unbounded state growth. | `http_tests::login_redirects_with_independent_state_and_nonce`, `http_tests::the_callback_is_rate_limited_per_client` |
| Issuer keys are cached for 10 minutes. An unknown `kid` refetches once, at most every 30 s. | Fetching JWKS per login; forged `kid`s turning the server into a request amplifier. | `oidc::tests::caches_keys_and_refetches_once_for_unknown_kid` |
| The token request omits `client_secret` when none is configured. | Sending an empty secret, which some providers reject. | `oidc::tests::token_form_omits_an_empty_secret` |
| The six-digit code is uniform from the OS generator. Five wrong codes clear the pending claim, its code and its QR token. | Guessing the code by a direct Rivet caller. The code is also shown to the device session, so this is defense in depth. | `actor::tests::five_wrong_codes_invalidate_the_pending_claim` |

Residual risk: a person who opens an attacker's claim link and chooses to sign in still proposes themselves as owner of the attacker's robot, which the attacker can confirm. The flow trusts the person who scans the code. The page asks them to keep the robot nearby and match the code on it.

### Device enrollment and sessions

| Control | Defends against | Test |
|---|---|---|
| Certificates are issuer-signed; proofs sign a short-lived challenge; the Bot actor consumes each nonce once. | Forged devices, replayed proofs. | `auth::tests::certificate_and_proof_bind_device_key`, integration suite |
| Signing keys must be at least 32 bytes, in `sign`, `verify`, `Bot::create` and `Memory::create`. | A short key that makes every session forgeable. | `auth::tests::short_keys_are_rejected_for_sign_and_verify` |
| Signing keys, the engine token, the lab token, the OIDC client secret and the device private key are `Secret<T>`: `Debug` prints `[redacted]` and the buffer is zeroized on drop (`zeroize`). `Cache` and `Pending` also redact sensitive fields in `Debug`. Copies a library makes, such as Rig's API key, are outside this guarantee. | Secrets in logs, panics and debug output. | `auth::tests::secrets_are_redacted_and_round_trip_identity_files` |
| Actor errors reach a client masked as internal, except the one public refusal: a submit while another turn is open answers `user/busy` with that turn's ID as metadata, to the authenticated session of the same conversation, which can already list its turns. Once a turn receives a `web_search` result, subsequent `web_search`, write, and history tools are all refused for the rest of that turn (taint). | Internal error text reaching devices; prompt injection via web results mutating memory/settings or exfiltrating history. | `actors::running_turn_pins_voice_rejects_busy_and_cancels_cleanly` (integration suite) |

Residual: rivetkit 3.1.1 has no `on_unsubscribe` hook, so subscriber cleanup relies on disconnect and session-expiry pruning.

### HTTP surface

| Control | Defends against | Test |
|---|---|---|
| Token bucket per client IP and route on `/v1/challenge`, `/v1/enroll`, `/claim`, `/login`, `/auth/callback`, `/lab/api/login`, `/ready`: a burst of 10, then one request per 6 s. IPv6 clients are keyed by `/64`. At most 4096 clients are tracked; idle buckets go first. | Credential guessing, sign-in floods, and `/ready` amplification. | `limit::tests::*`, `http_tests::credential_endpoints_are_rate_limited_per_client` |
| The client IP is the socket peer. `X-Forwarded-For` is read only when the peer matches a CIDR or IP in `BUDDY_TRUSTED_PROXY`, and then only its rightmost untrusted entry. | Spoofed forwarding headers. | `limit::tests::forwarded_for_is_used_only_from_trusted_proxies` |
| Listener caps: 512 total / 64 per client connections, 10 s header deadline, 6 min keep-alive idle. Request pool: 128 in flight, 32-per-client share, 30 s to the response head, 5 s body deadline (408) on every route. A permit is held until the head is sent; a streaming PCM body does not hold one. | Unbounded work queues; slowloris and slow-body starvation. | `serve::tests::*`, `http_tests::one_client_holds_at_most_its_share_of_the_pool`, `http_tests::only_an_extended_request_outlives_the_header_deadline`, `web::tests::bodies_are_read_within_the_deadline` |
| Speech authenticates from headers (device bearer; lab cookie, `Origin` and CSRF) before reading the body, then reads at most 1 KiB within 5 s (408). Only after it takes one of the four speech stream slots does it return its shared permit and get 330 s for the head, above the pipeline's own 300 s deadline. | Slow, incomplete speech bodies holding the shared pool and starving enrollment, sign-in and health checks. | `http_tests::slow_unauthenticated_speech_bodies_are_refused_from_headers`, `http_tests::slow_bodies_cannot_delay_health_enrollment_or_speech`, `http_tests::an_authenticated_speech_body_gets_a_short_read_deadline` |
| `/health` runs outside the request pool, is liveness-only, and touches nothing else; `/ready` is coalesced and rate-limited, checks the engine, and answers 503 without detail when it is down. The container health check uses `/ready`. | Liveness probes and connection warm-ups loading the engine. | `http_tests::health_answers_while_the_pool_is_full`, `http_tests::missing_configuration_is_503_and_disabled_lab_is_404` |
| Typed errors: 400 invalid input, 401 bad or missing credentials, 403 origin/CSRF/allowlist/revoked, 404 disabled lab or not found, 408 body deadline, 409 conflict, 429 limits, 502 engine/upstream/IdP, 503 not configured/unavailable, 504 upstream timeout. Bodies are fixed text; causes are dropped unlogged. | Leaking internal error text, URLs or tokens. | `web::tests::server_errors_hide_details`, `http_tests::missing_configuration_is_503_and_disabled_lab_is_404`, `http_tests::speech_statuses_name_the_failure` |
| Default security headers include `Strict-Transport-Security` (HSTS on HTTPS) and `X-Frame-Options: DENY`. Cookies read across every `Cookie` header. Cookie clears keep `Secure` on HTTPS. | Clickjacking, downgrade, missed cookies on HTTP/2, insecure clearing headers. | `web::tests::*` |

### Lab

| Control | Defends against | Test |
|---|---|---|
| The operator token and the guess are each MACed under the signing key, with the token as the message, and compared in constant time. Lab sessions live server-side (opaque cookie ID) and `/lab/api/logout` revokes the session immediately. | `token` and `token\0` colliding; stolen cookies surviving logout. | `lab::tests::lab_token_rejects_nul_padding`, `http_tests::lab_login_and_csrf_statuses`, `http_tests::the_lab_cookie_is_an_opaque_session_id` |
| Mutations need `Origin` and the session's CSRF header, compared in constant time. Actions pass an allowlist. | Cross-site requests; admin actions from the browser. | `http_tests::lab_login_and_csrf_statuses` |
| A failed poll or session check clears the watched turn. Catch-up events after reconnect apply silently; Replay still speaks on request. | Speaking a reply that completed during an outage. | `bun run test:lab-reconnect` (`apps/server/tests/lab-reconnect.mjs`, headless Chromium in CI) |

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
| The engine config (admin token, S3 keys) is written to `$XDG_RUNTIME_DIR` or the temp directory, never `/data`. The file is new, `0600`, uniquely named and removed on shutdown. Stale engine configs left behind by earlier runs or in `/data` are swept at start. | Secrets in volume backups or leftover temp files. | `engine::tests::engine_config_is_private_ephemeral_and_removed` |
| The engine stops with a direct `SIGTERM`; the image no longer ships `procps`. | Spawning `kill(1)`; a smaller image. | build only |
| An `openai_compatible` `AI_BASE_URL` must be HTTPS unless its host is loopback. `AI_ALLOW_PLAINTEXT_MODEL=1` allows plain HTTP to another host, for a model on a trusted private network; without it startup fails. | Prompts and the model API key crossing a network in plaintext by accident. | `model::tests::only_loopback_hosts_may_skip_tls` |
| `RIVET_ENDPOINT`, which carries the engine admin token, must be HTTPS. Plain HTTP is accepted only to loopback (`127.0.0.0/8`, `::1`, `localhost`) or to the single host named by `RIVET_ENDPOINT_PLAINTEXT_HOST`, for a private Docker-network service. No credentials or fragment. `BUDDY_S3_ENDPOINT` likewise requires HTTPS unless its host is explicitly named by `BUDDY_S3_ENDPOINT_PLAINTEXT_HOST`. | Admin tokens or S3 credentials crossing a network in plaintext. | `config::tests::secret_endpoints_need_tls_off_loopback`, `engine::tests::s3_endpoints_carry_keys_only_over_tls` |

`compose.yaml` and `compose.local.yaml` mount a `tmpfs` on `/tmp`, drop all Linux capabilities (`cap_drop: [ALL]`), set `read_only: true`, `no-new-privileges:true`, and cap memory/PIDs, so the engine config never touches disk. Do the same in any other production runtime.

### Robot local pages

| Control | Defends against | Test |
|---|---|---|
| Face panel and setup page send `Cache-Control: no-store`, a `default-src 'none'` CSP, `X-Frame-Options: DENY` and `Referrer-Policy: same-origin`, and share a connection guard (`guard.rs`: 16 concurrent connections, 10 s idle, 1 min total). | Framing, script injection, cached codes, local connection exhaustion. | `screen::tests::page_is_locked_down`, `wifi::tests::setup_form_requires_host_origin_and_csrf`, `guard::tests::*` |
| Face panel answers only `Host` equal to its own address (`127.0.0.1:8090` by default) or `localhost:<port>`. Confirm also needs a matching `Origin`. | DNS rebinding and cross-site POSTs from pages the kiosk opens. | `screen::tests::confirm_checks_origin_host_and_rotating_csrf`, `screen::tests::a_moved_panel_answers_only_its_own_host_and_origin` |
| `BUDDY_SCREEN_ADDR` accepts only a loopback `ip:port`, or `off`; anything else fails at start instead of binding. | Exposing the login-free owner confirmation to the network by a typo. | `screen::tests::the_address_is_loopback_configurable_or_off`, `screen::tests::an_off_or_invalid_panel_binds_nothing` |
| The face CSRF token rotates with each new confirmation code, and a press for an old code is dropped. The claim URL is cleared from cache once the robot is owned. | Confirming a later claim with an earlier press or token; stale claim tokens on disk. | same |
| The setup form POST needs `Host` `10.42.0.1` and `Origin` `http://10.42.0.1`. A missing `Origin` is refused: phone browsers send it on form POSTs. | Cross-site writes of Wi-Fi credentials. | `wifi::tests::setup_form_requires_host_origin_and_csrf` |
| `BUDDY_WIFI_INTERFACE` must match `^[A-Za-z0-9][A-Za-z0-9_-]{0,14}$`, checked before every use. nmcli runs without a shell, one argument per value, and secret-bearing `nmcli` calls (`nm_secret`) never log stderr (non-secret `nm()` calls still log bounded stderr for diagnostics). | An interface name parsed as an nmcli option; secrets in stderr. | `wifi::tests::interface_names_cannot_be_options` |
| SSIDs are 1–32 bytes without control characters and do not start with `-`; passwords are 8–63 printable ASCII characters and are zeroized in memory after use. | Values nmcli would read as options, lines that break its terse output, and lingering Wi-Fi secrets in memory. | `wifi::tests::rejects_keyfile_injection` |
| The home password never reaches argv: nmcli reads it from a 0600 file in `/run/robot-buddy`, escaped for the passwd-file format, removed when activation ends, also on failure. Home join verifies the profile readback as a hard failure; a failed join keeps the previous working home profile. Documented exception: the per-robot setup hotspot password is still passed as an nmcli argument (local-only, per-device random). | The home password in `ps` output, in the journal, or left on disk; stranding a robot on a mistyped password. | `wifi::tests::passwords_are_escaped_for_passwd_file_and_terse_output`, `wifi::tests::joining_home_proves_the_new_profile_before_retiring_the_old`, `wifi::tests::a_mistyped_password_keeps_the_working_home_network`, `wifi::tests::an_unsaved_password_fails_the_join_and_keeps_the_old_profile` |
| The identity file must be mode `0600`, opened with `O_NOFOLLOW`, and loose group/world modes are refused at start. `bundle.endpoint` must use TLS off loopback. | Symlink attacks, group-readable private keys, plaintext session downgrade. | `config::tests::the_identity_file_must_be_private`, `config::tests::engine_endpoints_need_tls_unless_local_and_opted_in` |
| The service runs as the `robot-buddy` account, not root, with `UMask=0077`. A polkit rule grants that account only the NetworkManager actions setup needs (`wifi.share.open` and `enable-disable-wifi` are dropped); its one capability is `CAP_NET_BIND_SERVICE` for the setup page; the unit is sandboxed (`ProtectSystem=strict`, `NoNewPrivileges`, `ProtectProc=invisible`, `MemoryDenyWriteExecute=yes`, `SystemCallFilter=@system-service ~@privileged @chown`). | A compromised client owning the robot. | unit file review (`infra/robot-buddy.service`, `infra/robot-buddy-nm.rules`) |
| The control socket is mode `0660`, group `pip-control` (`BUDDY_CONTROL_GROUP`), in a `0711` runtime directory (`UMask=0077`); only the service account and `pip-control` (never `robot-buddy`) can send wake, utterance and confirm lines. A stale socket is replaced; other files at the path are kept. | Other local accounts injecting utterances or reading `/etc/robot-buddy`. | `input::tests::the_control_socket_survives_its_writer_restarting`, `input::tests::a_stale_socket_is_replaced_but_other_files_are_kept` |

## Not covered

- No live browser, IdP, hardware or container run validates these controls. The tests above are unit and router tests.
- The rate limiter, `Logins`, and `lab::Sessions` are per process. They do not share state across replicas.
- A cloned device credential cannot be told apart from the original.

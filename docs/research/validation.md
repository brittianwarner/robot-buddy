# Local validation

Validated on November 13, 2026 using the code in this repository. This is local acceptance, not a deployed production service.

- Formatting and workspace Clippy (`--all-targets --all-features --locked -D warnings`) passed.
- Five unit tests passed, including OIDC token validation; the other cases cover: signature purpose/key binding, device certificate/proof binding, NetworkManager keyfile injection rejection, and escaping.
- The real Rivet integration scenario passed against engine 3.1.1 with Rig 0.46.0 and a local OpenAI-compatible streaming fixture. Assertions cover text, memory tool execution, history lookup, request deduplication, isolation, consumed enrollment nonces, claim confirmation, cancellation idempotency, revoked access, completed state across engine restart, and interruption of an active model stream.
- The single-container release image built successfully on Linux ARM64 as `robot-buddy:local`.
- The HTTP bootstrap/container scenario passed: certificate enrollment, rejection of replayed proof, stable actor IDs across re-enrollment, official Rust client access with the issued gateway grants, and reading previously stored memory after replacing the container while retaining its volume.

The unit-test command intentionally skips external integration cases; these were run separately. Initial development failures included an omitted test namespace, an incomplete SSE fixture, and a collision between concurrent test runs on the same fixture port. The final isolated runs passed after those issues were corrected.

Not validated: a local inference server's tool support, a real-provider OIDC browser login, NetworkManager/AP hardware, kiosk/speech hardware, Linux AMD64 image execution, production load or a backup restoration drill. Those require the operator's selected services and robot. No remote repository, public deployment or provider credentials were created.

The review added automated Chromium acceptance with a controlled OIDC provider and streaming model fixture. Registration, reconnect, owner confirmation, Rig memory tools/chat, durable reload, rejected unauthorized/CSRF/admin commands, and mobile layout passed. See [review findings](implementation-review.md) and [lab instructions](../lab.md).

Live OpenRouter acceptance also passed with GLM 5.3 Flash Nitro (low reasoning) and Fish Drama 3: memory-tool execution, conversational answer, streaming PCM playback in Chromium, Stop, explicit replay and silent reload. The operator supplied the key; it is stored only in ignored local configuration. This validates browser playback mechanics, not subjective voice quality or physical speaker output.

## November 23, 2026: test suite rework (wave2-robot, 120c195)

Local runs on a Mac mini (Apple M4 Pro, macOS arm64) shared with other build jobs. What each suite covers is in [testing](../testing.md).

- `cargo test --workspace --locked`: 102 passed, 20 ignored. Of those: 24 buddy unit, 3 history SQL, 10 protocol golden, 4 sentence, 7 voice rule, 42 robot and 12 server tests. The ignored ones are the 17 engine scenarios, 2 voice-rule bug tests and the container test. Before this rework the same command ran 85 tests, and the engine suite was a single ignored scenario.
- `bun run test:integration`: 17 of 17 passed against engine 3.1.1, in two consecutive runs (49 s and 44 s). Nothing was left listening on 16420, 16421, 16430 or 19091, and no run directory remained. The first run reaped an orphaned engine (parent PID 1) left by an earlier failed run of the old harness. Killing the test binary with SIGKILL mid-run left no engine and no run directory. A foreign listener on the model port stopped the run with its PID and command.
- Restart scenario flake: in 2 of about 12 further full runs at load average 33 to 43, `restart_interrupts_streams_and_fails_closed_on_unresolved_tools` failed with one `turn.started` where two were expected. Two test-side causes, both fixed in the harness and test (no source change):
  - Replay skipped events lost in a reconnect. `Events::pull` replayed after the newest sequence held, so when the reconnected socket pushed `turn.failed` first, the resumed turn's `turn.started` was never fetched. It now replays from the first hole.
  - The search lost its race with the stop. The web search client allows 5 s for the proxy tunnel (`connect_timeout`). In one run the graceful stop took longer than that to reach the actor, so the search failed by itself (`transport_error=true`) before the actor stopped, and the checkpoint was never exercised. Pushed events now carry the time the push arrived. A `turn.failed` received before the restart finished counts as a lost race, and the tool leg is retried once with a new bot and a second restart.
  After both fixes, 9 consecutive full runs passed at load average 32 to 41 (restart 17.7 to 21.7 s). None of them needed the retry, so the retry path itself has not run.
- Clippy pedantic (`bun run lint:pedantic`): no warnings in `crates/buddy/tests`. Source files still have 113 warnings, so CI keeps `-D warnings` without pedantic.
- `cargo deny --locked check` (cargo-deny 0.22.0): licenses, sources and bans pass. Advisories fail on RUSTSEC-2026-0592: `url` 2.5.8 overflows the stack in `Url::origin()` on nested `blob:` URLs. Our two `origin()` calls parse operator configuration, not request input. The fix is `cargo update -p url` to 2.5.9. Three advisories are ignored in `deny.toml`, each with its reason: bundled SQLite in a dev-dependency, and chrono/chrono-tz unmaintained.
- `bun tests/early-player.mjs`: 3 of 3 passed (offline).
- `bun run bench:e2e`: one baseline (983e90f) run and four current runs, all turns completed, no leaks. Text-path latency is unchanged from baseline within noise; first audio p50 is 833 ms with fixed 250 ms model and 300 ms speech latency. See [e2e latency](e2e-latency.md).
- `bun tests/robot-shutdown.mjs`: fails. The robot ignores SIGTERM while stdin is open (a regression from baseline).

- Robot on aarch64 Linux: `cargo check --locked -p robot-buddy-robot` passed in a native `linux/arm64` `rust:1.97.1` container. CI instead cross-checks from x86_64 with `gcc-aarch64-linux-gnu`, and that path was not run locally.

Not run locally: the container test and the live-provider lab scripts.

## November 23, 2026: bug fixes from the test pass (wave2-robot, f712ceb + working tree)

Same Mac mini, shared with other jobs.

- Robot shutdown: stdin is read on a plain OS thread, runtime shutdown is bounded at 100 ms, and the in-memory cache is flushed before exit. `bun run test:shutdown` (new, in CI after the release build): 5 of 5 stops exited 0 in 12 to 33 ms with the cache written (3 SIGTERM and 1 SIGINT with stdin open, 1 SIGTERM with stdin closed). The bench's own check reported 52 ms. New unit tests cover an open input not delaying runtime shutdown, and runtime shutdown SIGKILLing an abandoned player; the second fails with `kill_on_drop(false)`.
- Speech rules: overlaps now resolve longest first across start positions (ties leftmost), and model-authored markup is removed before rules apply. Both formerly ignored voice-rule tests pass, plus two new ones. A sentence that is only markup sends no provider request.
- Protocol: settings types ignore additive fields on read; settings writes (commands and model tool arguments) reject unknown fields at any depth through `protocol::strict` (`serde_ignored`). The golden suite records both halves.
- `cargo update -p url`: 2.5.8 to 2.5.9 (and `form_urlencoded` 1.2.2 to 1.2.3). `bun run deny` (new root script, `cargo deny --locked --all-features check`, cargo-deny 0.22.0): advisories, bans, licenses and sources ok; only duplicate-crate warnings.
- `cargo fmt --all --check` and `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`: clean. `bun run lint:pedantic`: no warnings. Pedantic is already enabled in `[workspace.lints.clippy]` at this commit, so the 113 source warnings recorded above were already gone; nothing needed fixing.
- `cargo test --workspace --locked`: 170 passed, 18 ignored (54 buddy unit, 8 history SQL, 11 protocol golden, 4 sentence, 11 voice rule, 52 robot, 30 server; ignored are the 17 engine scenarios and the container test).
- `bun run test:integration`: 17 of 17 passed in two consecutive runs; nothing left listening on 16420, 16421, 16430 or 19091 and no run directory left. The one `rivet-engine` process still running belonged to another checkout and predated these runs.
- `bun run bench:e2e` (short runs, 2 to 3 turns): the 4 s warm-up first audio reproduced in 1 of 5 runs and was traced to the fake player's first launch, not to backend or robot code. See [e2e latency](e2e-latency.md).

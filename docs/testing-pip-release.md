# Pip release qualification

The hardware bring-up checklist is in [DEPLOYMENT.md](../DEPLOYMENT.md). Passing a crate test or the workstation latency benchmark does not qualify a release. Attach the following evidence to the release PR for the **same candidate**. If only docs or measurement artifacts changed after a run, name the tested SHA and show the intervening diff.

| Gate | Evidence required |
|---|---|
| Compiler and platform | For both pinned ARM64 Bookworm and Trixie builders: compiler version, image digest, workspace `cargo test --locked`, workspace Clippy with `-D warnings`, `cargo fmt --all --check`, default and full Pip features. List ignored tests and their prerequisites. |
| Virtual Pip | Real robot, server and Rivet engine; PTY face/servo bus, fake ALSA and camera devices. Show separately which trials used a real recording, actual Whisper and actual neural wake. Show an interrupted slow decode, playback echo, model gesture on a live turn, QR restoration after a board reset, reconnect and claim. |
| Continuous load | At least 30 uninterrupted minutes of conversation through the virtual Pip path. Attach wall duration, completed and failed trials, p95 timestamps with explicit endpoints, cgroup memory start/peak/end and growth, motion ticks/missed deadlines, restarts and panics. Do not count setup time or idle waiting. |
| Deployment rehearsal | Fresh ARM64 systemd guest; exercise the documented install with the 1 GiB profile, OIDC claim, upgrade and rollback retaining identity and state. Unit-file syntax validation alone is insufficient. |
| Independent verification | Final OpenCode Neon review on the candidate; record decision, residuals, and CI status for the PR head. |

Hardware checks remain separate even after these gates pass. A virtual servo register map cannot prove neck clearance or torque release. Fake PCM consumption cannot prove acoustic wake accuracy, echo suppression or audible latency. A fixture image cannot qualify the real CSI sensor, lighting, optics or face accuracy.

## Asset-dependent checks on candidate `5351762`

Both normally ignored robot tests below were explicitly run using the **Linux ARM64 full-feature Rust test binary**, not just its mock engine. Each passed. The models and recordings were mounted read-only; none are included in the repository.

| Check | Input and result | What it does not establish |
|---|---|---|
| `listen::wake::onnx::tests::compare_reference_scores` | Upstream Python `openwakeword==0.6.0` reference, ONNX Runtime 1.20.1, 195 × 80 ms hops. After the 50-hop warmup, idle and busy decisions matched on all 145 evaluated hops. The positive was [two recorded words from one speaker](../tests/pip-e2e/speech/human-wake-recipe/README.md), edited together and surrounded by recorded human negatives. | Natural contiguous wake-word recall across people, rooms, and playback conditions. |
| `vision::native::tests::enrollment_matches_another_view_but_not_an_unknown_person` | Pinned [OpenCV fixtures](../tests/pip-e2e/peripherals/fetch-vision-models.sh), YuNet and SFace. Enrolled `face_a_1.i420`, matched the distinct `face_a_2.i420` view to the same entry, and rejected `face_b_1.i420` as unknown. | Accuracy on Pip's camera or on a representative population. A recognition match does not authorize access. |

The [systemd rehearsal](../tests/pip-deploy-qa/EVIDENCE.md) separately exercised the candidate executable with the effective 1 GiB cap, OIDC enrollment, and actual cross-version upgrade/rollback. The [workstation benchmark](research/e2e-latency.md) is a submit-to-output regression check, not a substitute for either test or the loaded soak.

### Parallel-test corrections (`eb639ad`)

- **Face serial stall:** In a Linux ARM64 PTY experiment, a 20-byte write succeeded after a 1,024-byte write returned `EAGAIN` in **80/80 trials**. The kernel can move buffered bytes into the unread master's queue in between. The old test inferred two imminent timeouts from that first `EAGAIN` and counted opens against a real stopwatch; neither assumption tested the write deadline. The replacement drives an oversized write on a real nonblocking raw PTY and advances virtual time. A short write must complete on the reactor while its sole blocking worker is occupied. A separate virtual-clock test verifies automatic reopen ordering over two forced stalls. Neither test depends on a scheduler delivering a heartbeat within four or six wall-clock seconds.
- **Listen drop logging:** The `tracing-core` single-dispatcher fast path caches interest using the thread that first registers the callsite. In a minimal reproduction, registering it first on a thread without a subscriber suppressed all three events in a scoped subscriber; rebuilding the cache before the third event reproduced the reported **one captured out of three**. The control captured all three. The drop test still checks every admitted/suppressed result, exact event count, safe field names and values. It runs that subscriber assertion once in a dedicated test process so other parallel producers cannot change its callsite interest. The matrix itself remains parallel; it does not retry either test until it passes.

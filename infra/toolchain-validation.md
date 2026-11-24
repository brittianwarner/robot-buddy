# ARM64 toolchain validation: Rust 1.100.0 on Debian Bookworm and Trixie

This matrix checks that the workspace builds, lints, and tests cleanly on Linux
ARM64 with the pinned Rust 1.100.0 toolchain (`rust-toolchain.toml`). It runs
on both supported Pi OS bases, with default features and with the full Pip
features (`pi-vision` + `onnx-wake`).

## Verdict (qualifying source eb639ad)

| OS | Result | First failure |
| --- | --- | --- |
| Trixie (Debian 13, packaged OpenCV 4.10.0) | **PASSED** (every phase, including STRESS 20/20) | none |
| Bookworm (Debian 12, private OpenCV 4.10.0) | **FAILED** at `[FULL] cargo test --workspace --all-features` | `controller::tests::offline_announcement_participates_in_busy_and_reaps_before_wake_or_reconnect` |

So the matrix is **not fully green yet**. The Bookworm failure was not retried.
The gate stops at its first failure, so Bookworm's full-feature release build,
ARTIFACT, and STRESS phases did not run in this qualifying run.

## Tooling

| File | Role |
| --- | --- |
| `infra/run-arm64-matrix.sh` | Host runner. Refuses dirty workspace sources in gate mode. Computes the workspace tree hash and runs the OS gates one at a time (Trixie, then Bookworm). |
| `infra/arm64-matrix.Dockerfile` | Digest-pinned `rust:1.100.0-slim-{bookworm,trixie}` toolchains. Bookworm builds OpenCV 4.10.0 (commit `71d3237a`) into `/opt/robot-buddy/opencv-4.10`. Trixie uses `libopencv-dev`. `*-deps` warms caches. `*-gate` runs the check with `--no-cache-filter`. |
| `infra/arm64-matrix.Dockerfile.dockerignore` | Limits the build context to the workspace sources and the check script. Excludes model and key files. |
| `infra/arm64-matrix-check.sh` | The in-container gate (phases listed below). |

Run it with `CARGO_JOBS=2 infra/run-arm64-matrix.sh both`. That is two Cargo
jobs per container, sized for a 16 GiB Docker budget.

Gate phases, in order. The gate stops at the first failure.

1. Verify the tree hash: the hash of the in-container sources must equal the host hash.
2. `[FMT]` `cargo fmt --all -- --check`
3. `[DEFAULT]` clippy `--workspace --all-targets -D warnings`; `cargo test --workspace`; `cargo test -p robot-buddy --no-default-features`
4. `[FULL]` clippy `--all-features -D warnings`; `cargo test --workspace --all-features`; release build `robot-buddy-robot --features pi-vision,onnx-wake`
5. `[ARTIFACT]` `file` must report AArch64 ELF. `ldd` must show no `not found` entries and must link OpenCV. On Bookworm every OpenCV library must resolve under `/opt/robot-buddy/opencv-4.10/lib/`. Then `setup-card` runs as `nobody` through `setpriv`, writing `BUDDY_SETUP_WIFI_FILE` into a 0700 directory, and the file must come out `65534:65534:600`.
6. `[STRESS]` 20 runs at `--test-threads=8` of the three corrected tests, with no retries. Each run must report `3 passed; 0 failed`.
7. `[IGNORED]` Lists the ignored tests.

### Checker change after the qualifying run

The qualifying run's checker printed the release ELF's GNU BuildID only through
`file`. It did not print a SHA-256. BuildKit had already evicted the Trixie
target cache, so that binary cannot be hashed after the fact. Rebuilding it
would hash a different build, so that was not done. The committed checker now
prints `sha256sum` and the `readelf -n` BuildID inside `[ARTIFACT]`, and it
puts both in the final `PASSED` line. That change applies to future runs only.
Nothing else in the gate changed.

## Qualifying source

- Commit `eb639ad2e97bfed2ab25ed4ad25ab21fdfa71d4b`. The runner checked the workspace sources as CLEAN.
- Workspace tree hash `5e357f0abcda65f7d5730ce73e5657f0225bf7c6e527e5b92494571be0bdcacc`. The host hash and both container hashes match.
- Compared with soak candidate `5351762` (tree `dcbcb885…`), `git diff 5351762 eb639ad -- Cargo.toml Cargo.lock rust-toolchain.toml .cargo crates apps` touches only two files:
  - `apps/robot/src/face.rs`: every hunk is at line ≥ 1286, inside `#[cfg(test)] mod tests` (lines 748–1373, which run to the end of the file).
  - `apps/robot/src/listen/mod.rs`: every hunk is at line ≥ 1686, inside `#[cfg(test)] mod tests` (lines 959–1779, which run to the end of the file).

  Production and release code is therefore identical between source trees `5351762` and `eb639ad`.
  This is a source-level argument, not an ELF comparison.
- Later commits (`aad1a85`, `a70271c`, `6500385`) change only `docs/` and `tests/pip-e2e/`. They do not touch the workspace tree.

## Qualifying run results

Log: [`evidence/arm64-matrix/qualifying-eb639ad.log`](evidence/arm64-matrix/qualifying-eb639ad.log)
(sha256 `6db8fb66ef2e4f84754bfc8834f536e275a3bdd7e4839911bc77d40cd881fa86`).
Started 2026-11-24 at about 14:08 ET with `CARGO_JOBS=2 infra/run-arm64-matrix.sh both`.

### Trixie: PASSED (648.7 s)

- `Debian GNU/Linux 13 (trixie)`, `rustc 1.100.0 (0ef1bc386 2026-11-09)`, opencv4 4.10.0 with prefix `/usr`.
- FMT ok. DEFAULT: clippy ok, workspace tests ok (robot bin 251 passed), `--no-default-features` ok.
- FULL: clippy ok, workspace tests ok (robot bin 253 passed, 2 ignored), release build ok.
- ARTIFACT: `ELF 64-bit LSB pie executable, ARM aarch64 … stripped`.
  - **BuildID[sha1] `4bbbcb32d665941bbf66c20e250ebbb7b29c0f26`**
  - **SHA-256: not recorded** (see "Checker change after the qualifying run").
  - `ldd` has no `not found` entries. OpenCV `*.so.410` resolves from `/lib/aarch64-linux-gnu/`.
  - `setup-card` smoke ok (`uid=65534 gid=65534 mode=600`).
- STRESS: **20/20 passed** (`3 passed; 0 failed` each run, `--test-threads=8`, no retries).
- `=== ARM64 Matrix Gate PASSED: trixie (SOURCE_SHA=eb639ad… TREE_HASH=5e357f0a…) ===`

### Bookworm: FAILED in FULL tests (609.7 s)

- `Debian GNU/Linux 12 (bookworm)`, `rustc 1.100.0`, opencv4 4.10.0 with prefix `/opt/robot-buddy/opencv-4.10`.
- FMT ok. DEFAULT: clippy ok, workspace tests ok (robot bin 251 passed), `--no-default-features` ok. FULL clippy ok.
- FULL `cargo test --workspace --all-features --locked -j 2`: robot bin `252 passed; 1 failed; 2 ignored`.
  - `controller::tests::offline_announcement_participates_in_busy_and_reaps_before_wake_or_reconnect`
  - Panicked at `apps/robot/src/controller.rs:2642:17` with `online: offline player pid missing`.
- Never ran: release build, ARTIFACT (no Bookworm BuildID or SHA-256), STRESS, IGNORED.

**Unconfirmed theory about the failure:** this test is not one of the three
corrected tests. It passed in every earlier diagnostic run that reached it (11 runs of the test suite, 0 failures). The
assertion is a 5 s wall-clock deadline for a spawned `sh` player to write its
pid file. While the Bookworm gate ran, a separate 30-minute Pip E2E soak
container (`soak-30m-v2-5351762`, started about 14:17 ET) was running on the
same Docker host. The Trixie gate had finished before the soak started. That
points to resource contention against a real-time deadline, but nobody has
reproduced it. The failure must be diagnosed and fixed in the test (as with the
two earlier flakes) before a Bookworm rerun can count. A plain rerun until it
passes would not count.

## Diagnostic runs (not qualifying)

These are kept apart from the qualifying run in
[`evidence/arm64-matrix/diagnostic/`](evidence/arm64-matrix/diagnostic/). They
ran before `eb639ad`, and they found the two flakes that `eb639ad` fixes.

| Log | Source SHA | Tree | OS | Outcome |
| --- | --- | --- | --- | --- |
| `arm64-matrix-gate.log` | `bb8753d` | `e90ae9f5…` | Trixie | FAILED: `face::…reopens_raw` (face.rs:1340, no reopen) |
| `arm64-matrix-gate-2.log` | `738db59` | `e90ae9f5…` | Trixie | FAILED: `face::…reopens_raw` |
| `arm64-matrix-bookworm.log` | `5351762` | `dcbcb885…` | Bookworm | FAILED: `listen::…drop_log_is_rate_limited…` (listen/mod.rs:1734) |
| `arm64-matrix-5351762.log` | `5dc8a34` | `dcbcb885…` | Trixie | FAILED: `face::…reopens_raw` |
| `arm64-matrix-5351762-final.log` | `d65e75a` | `dcbcb885…` | Trixie | FAILED: `face::…reopens_raw` |
| `arm64-matrix-5351762-bookworm.log` | `b960f1c` | `dcbcb885…` | Bookworm | PASSED (an earlier checker revision) |
| `arm64-matrix-5351762-trixie.log` | `b960f1c` | `dcbcb885…` | Trixie | FAILED: `face::…reopens_raw` |

The first diagnostic failures were `face::tests::a_stalled_port_times_out_without_leaking_threads_and_reopens_raw`
(Trixie, from the first gate run) and `listen::tests::drop_log_is_rate_limited_and_emits_only_safe_metadata`
(Bookworm on `5351762`). `eb639ad` fixed both by making the tests deterministic
(see `docs/testing-pip-release.md`), not by retrying them. In the qualifying
run, both tests and the new `face::tests::writer_reopens_only_after_the_stall_deadline_and_reopen_delay`
passed in the FULL suites on both OSes. On Trixie they also passed the 20×
stress battery.

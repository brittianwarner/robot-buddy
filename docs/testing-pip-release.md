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

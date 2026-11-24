# Documentation index

Start with the [README](../README.md). To bring up hardware, go to [DEPLOYMENT.md](../DEPLOYMENT.md). Before you change code, read [AGENTS.md](../AGENTS.md). Co-located docs describe the code as built. Research notes and plans are dated records and may describe older states.

## Bring-up and release

- [DEPLOYMENT.md](../DEPLOYMENT.md): physical Pi 5 bring-up, from hardware and firmware to the go/no-go checklist, troubleshooting and rollback. §0 lists what comes from butty, what you create, the one decision to make with Brittian (own backend or his), OIDC owner sign-in (needed before Pip talks), and the "Confirm on your Pip" checks
- [README: butty and robot-buddy](../README.md#butty-and-robot-buddy): how this Rust port differs from ToschLogic's butty (Python brain), and the Bookworm/Trixie binary choice
- [testing-pip-release.md](testing-pip-release.md): evidence a Pip release candidate needs, and the asset-dependent checks run so far
- [performance-pip.md](performance-pip.md): on-device acoustic and hardware-budget benchmark (`scripts/pip-perf.py`) and its provisional thresholds
- [../CHANGELOG.md](../CHANGELOG.md): what the Pip port changed, and its validation status

## Testing

- [testing.md](testing.md): every suite, what it proves, ports, the e2e bench and its publish rules, and what nothing proves
- [../tests/pip-e2e/README.md](../tests/pip-e2e/README.md): ARM64 full-stack E2E, fault suite and 30-minute soak harness
- [../tests/pip-e2e/peripherals/README.md](../tests/pip-e2e/peripherals/README.md): virtual face, servo, camera and ALSA peripherals for the E2E harness
- [../tests/pip-e2e/speech/README.md](../tests/pip-e2e/speech/README.md): ARM64 speech qualification (real whisper-cli, ONNX Runtime and openWakeWord) and its asset lock
- [../tests/pip-e2e/speech/human-wake-recipe/README.md](../tests/pip-e2e/speech/human-wake-recipe/README.md): provenance recipe for the recorded human wake positive
- [../infra/toolchain-validation.md](../infra/toolchain-validation.md): evidence that the ARM64 Bookworm/Trixie Rust 1.100.0 matrix passed at `82e8d5b`
- [../tests/pip-deploy-qa/EVIDENCE.md](../tests/pip-deploy-qa/EVIDENCE.md): evidence from the fresh ARM64 systemd deployment rehearsal (install, 1 GiB cgroup, OIDC claim, upgrade, rollback)
- [lab.md](lab.md): optional `/lab` browser workbench and its acceptance scripts

## Architecture

- [architecture/backend.md](architecture/backend.md): server, Rivet actors (Bot, Conversation, Memory), Rig execution and recovery, storage
- [architecture/robot-client.md](architecture/robot-client.md): `apps/robot` modules, control input, events, face states, environment, plus the broader design-target protocol
- [architecture/voice.md](architecture/voice.md): inference and persona, the PCM speech contract, Eleven v4 Turbo over OpenRouter
- [architecture/boot-and-registration.md](architecture/boot-and-registration.md): first-boot, enrollment and claim design (broader than what is implemented)
- [../apps/robot/src/vision/README.md](../apps/robot/src/vision/README.md): Pi camera, YuNet/SFace models and the local face gallery
- [security.md](security.md): assets, trust boundaries, controls and the tests behind them, and what is not covered

## Research (dated records)

- [research/validation.md](research/validation.md): dated local validation runs, including live OpenRouter acceptance on the earlier speech route
- [research/e2e-latency.md](research/e2e-latency.md): published workstation e2e latency table (generated, do not hand-edit)
- [research/e2e-latency.json](research/e2e-latency.json): raw bench JSON behind that table (`bench:report:check` validates it)
- [research/benchmarks.md](research/benchmarks.md): backend Criterion benchmarks and SQL-per-turn counts (2026-11-23)
- [research/implementation-review.md](research/implementation-review.md): review of server, bootstrap, actors and Rig integration, with findings
- [research/rig-integration.md](research/rig-integration.md): Rig 0.46.0 API and source findings
- [research/rivet-actor-findings.md](research/rivet-actor-findings.md): Rivet actor and Rust example findings
- [research/rivet-and-providers.md](research/rivet-and-providers.md): early runtime and model-provider research
- [research/rivet-ecosystem-review.md](research/rivet-ecosystem-review.md): Rivet docs and ecosystem review
- [research/actors-manifest.json](research/actors-manifest.json), [research/rivet-crawl-manifest.json](research/rivet-crawl-manifest.json), [research/rivet-example-manifest.json](research/rivet-example-manifest.json), [research/rust-source-manifest.json](research/rust-source-manifest.json): inventories of the Rivet pages and sources fetched for that research

## Plans

- [plans/implementation.md](plans/implementation.md): implementation status and remaining acceptance
- [plans/repository.md](plans/repository.md): original Turborepo/Cargo layout plan (historical; the real layout is in the README repo map)

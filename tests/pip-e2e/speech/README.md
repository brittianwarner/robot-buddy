# ARM64 Pip Speech Qualification (`tests/pip-e2e/speech/`)

Repeatable Linux ARM64 qualification for Pip's local speech stack:

- **Real ARM64 `whisper-cli`** built from pinned [`ggml-org/whisper.cpp`](https://github.com/ggml-org/whisper.cpp) `v1.9.6` (static Release, matching `DEPLOYMENT.md` §4).
- **Real Whisper ggml model** `ggml-tiny.en.bin` from HuggingFace `ggerganov/whisper.cpp` (SHA-256 verified).
- **Real ARM64 ONNX Runtime 1.20.1** (`libonnxruntime.so`, `ORT_API_VERSION = 20`, exact ABI match for `ort = "=2.0.0-rc.9"` / `ort-sys`) extracted from the official Microsoft NuGet package (`microsoft.ml.onnxruntime.1.20.1.nupkg`, SHA-256 verified).
- **Real openWakeWord v0.5.1 frontend** (`melspectrogram.onnx`, `embedding_model.onnx`, SHA-256 verified; upstream release weight binaries have no separate model-card license file — see `assets.lock.json` for primary upstream citations and operator review notice).
- **Private `hey_pip.onnx`** mounted **read-only** from the local clone at runtime only (`PIP_E2E_HEY_PIP_ONNX`). Never copied into the repository, git history, or Docker image layers.

All pinned URLs, licenses, and SHA-256 digests live in [`assets.lock.json`](./assets.lock.json).

## Repeatable Commands

Run the full ARM64 build + qualification suite (caches models in gitignored `tests/pip-e2e/.cache`, writes report to gitignored `tests/pip-e2e/.artifacts/speech-qualification.json`):

```sh
tests/pip-e2e/speech/run.sh
```

Re-run without rebuilding the ARM64 image:

```sh
tests/pip-e2e/speech/run.sh --skip-build
```

Also run the ARM64 Rust `robot-buddy-robot` test target (`--all-features`, `pi-vision,onnx-wake`):

```sh
tests/pip-e2e/speech/run.sh --rust-tests
```

Install into a shared E2E container (used by `tests/pip-e2e/Dockerfile` or standalone):

```sh
# Build whisper-cli + libonnxruntime.so + ort-wake-probe into /opt/pip-e2e/speech:
tests/pip-e2e/speech/install-speech-stack.sh --prefix /opt/pip-e2e/speech --cache /opt/pip-e2e/cache --build-only

# Fetch & verify ggml-tiny.en.bin, openWakeWord models, human WAVs, and emit fixtures manifest:
tests/pip-e2e/speech/install-speech-stack.sh --prefix /opt/pip-e2e/speech --cache /opt/pip-e2e/cache --fetch-only
```

## Provenance & Opt-In Human Recordings

This suite strictly separates **real prerecorded human speech**, **synthetic fixtures**, and **missing human evidence**. TTS is **never** represented as a human recording.

| Fixture category | Provenance | Included by default? |
|---|---|---|
| `human_jfk_negative` (`whisper.cpp` `samples/jfk.wav`) | `human_recorded` (Public Domain) | Yes |
| `human_voices_followup` (VOiCES / LibriSpeech `sp0307`) | `human_recorded` (CC-BY-4.0) | Yes |
| `human_librispeech_negative` (OpenSLR-12 LibriSpeech `1688-142285-0007`) | `human_recorded` (CC-BY-4.0) | Yes |
| `human_wake_positive` (human saying "Hey Pip") | `missing` unless opted in | No — reported as `missing_evidence` |
| `human_wake_reference_parity` (PCM + reference scores) | `missing` unless opted in | No — reported as `missing_evidence` |
| `synth_*` (silence, ambient RMS 450/600, echo, 12s truncation, `espeak-ng` policy phrases) | `synthetic` (`is_human_recording: false`) | Yes (pipeline/policy/echo mechanics only) |

To qualify with real local human recordings (mounted read-only, never copied into the repo):

```sh
PIP_E2E_HEY_PIP_ONNX=/path/to/authorized/hey_pip.onnx \
PIP_E2E_HUMAN_WAKE_POSITIVE_WAV=/path/to/human_hey_pip_16k_mono.wav \
PIP_E2E_HUMAN_WAKE_NEGATIVE_WAV=/path/to/human_negative_16k_mono.wav \
PIP_E2E_HUMAN_FOLLOWUP_WAV=/path/to/human_followup_16k_mono.wav \
PIP_E2E_HUMAN_WAKE_PCM=/path/to/reference_clip_16k_s16le.pcm \
PIP_E2E_HUMAN_WAKE_SCORES=/path/to/reference_scores.txt \
tests/pip-e2e/speech/run.sh --skip-build
```

Or point `PIP_E2E_HUMAN_SPEECH_DIR` at a read-only folder containing any subset of `wake_positive.wav`, `wake_negative.wav`, `followup.wav`, `reference_clip.pcm`, and `reference_scores.txt`.

## Fixtures vs. Physical Hardware

Passing this suite proves:
- Native Linux ARM64 `whisper-cli`, `ggml-tiny.en.bin`, `libonnxruntime.so` (`ORT_API_VERSION = 20`), openWakeWord models, and read-only mounted `hey_pip.onnx` load, pass checksums, and execute cleanly within real-time budgets.
- Real VAD endpointing (including 700-frame sustained >500 RMS ambient recovery and 12s truncation), real Whisper transcription, transcript wake/policy decisions, echo-tail rejection, slow-Whisper onset windowing, and priority-wake cancellation (zero orphan `whisper-cli` processes) work on ARM64 Linux.
- Zero false triggers from `hey_pip.onnx` on real human negative recordings (`jfk`, `voices`, `librispeech`), silence, ambient RMS 600 noise, and busy playback echo.

Passing this suite does **not** prove physical Pi 5 bench gates:
- Physical reSpeaker USB mic array beamforming, room reverberation, or real-speaker acoustic echo cancellation on MAX98357A.
- Human positive "Hey Pip" recall / false-accept rate across speakers until real human positive recordings are supplied via `PIP_E2E_HUMAN_WAKE_POSITIVE_WAV` and `PIP_E2E_HUMAN_WAKE_PCM` + `PIP_E2E_HUMAN_WAKE_SCORES`.
- Physical Pi 5 Cortex-A76 thermals and sustained CPU/memory headroom alongside camera and 50 Hz servos.

# Human Speech Wake Positive Provenance Recipe (`tests/pip-e2e/speech/human-wake-recipe/`)

Reproducible, license-credited provenance recipe for prerecorded human speech wake fixtures used to test Pip's **Whisper transcript wake** and **ONNX neural acoustic wake (`hey_pip.onnx`)** paths on Linux ARM64.

**No audio media or private ONNX models are committed to git.** All upstream audio downloads and materialized 16 kHz mono S16_LE WAV/PCM files live exclusively inside gitignored `tests/pip-e2e/.cache/speech/human-wake-positive/`. The private `hey_pip.onnx` classifier stays outside the repo and is supplied explicitly at runtime via operator-owned `$PIP_E2E_HEY_PIP_ONNX` (mounted read-only; no default path).

> **Important provenance boundary:** A verified, consented natural recording of *"Hey Pip"* from Brittian supersedes the `edited_spliced_human` fixtures below for any release recall/FAR acceptance claim.

## Provenance Taxonomy & `whisper` vs. Acoustic Wake Distinction

We explicitly distinguish **natural unmodified human recordings** from **edited/spliced component-level recordings**, and **Whisper transcript wake** from **ONNX neural acoustic wake**:

| Output clip | Provenance | `whisper-cli` (ARM64 `ggml-tiny.en.bin`) | `Policy` (`apps/robot/src/listen/policy.rs`) | `hey_pip.onnx` (ARM64 ORT 1.20.1, RO mount) | Intended use |
|---|---|---|---|---|---|
| `natural_human_transcript_wake_pip.wav` | **`human_recorded_natural`** (Public Domain, unedited contiguous utterance) | `"Pip, what a fool you are."` | Idle: `Wake` + `Say("what a fool you are.")`<br>Busy: `[]` (no bare-Pip barge-in) | `score_max = 0.006470`<br>Idle: `false`, Busy: `false` | **Natural human transcript-wake positive** (idle bare `"Pip, ..."` path) and neural-wake bare-`"Pip"` negative. |
| `edited_spliced_human_hey_pip.wav` | **`edited_spliced_human`** (component-level only; **NOT** a natural held-out recording) | Direct: `"Hey, Pip."`<br>After VAD: `"Hey, Pip!"` | Idle: `Wake`<br>Busy: `Wake` | **`score_max = 0.985009`** (hop 36, `warmup_nonzero=0`, `fire_count=1`)<br>Idle (0.50): **`true`**, Busy (0.80): **`true`** | **Component-level neural acoustic wake + greeted transcript wake** positive (standalone wake). |
| `edited_spliced_human_hey_pip_command.wav` | **`edited_spliced_human`** (component-level only; **NOT** a natural held-out recording) | Direct: `"Hey! Pip! I had that curiosity beside me at this moment."`<br>VAD clip 0: `"Hey, Pip."`<br>VAD clip 1: `"I had that curiosity beside me at this moment."` | Transcript mode: clip 0 `Wake` -> clip 1 `Say(...)`<br>Acoustic mode: wake fires -> clip 1 `Say(...)` | **`score_max = 0.929532`** (hop 36, `warmup_nonzero=0`, `fire_count=1`)<br>Idle (0.50): **`true`**, Busy (0.80): **`true`** | **Component-level full-turn wake + follow-up command** stimulus for end-to-end neural-wake E2E turns (`runner.mjs`). |
| `control_human_hey_only.wav` | `human_recorded_ablation_control` | `"Hey!"` | Idle/Busy: `[]` | `score_max = 0.000547` (`false`) | Proves `"Hey"` alone does not fire `hey_pip.onnx`. |
| `control_human_pip_only.wav` | `human_recorded_ablation_control` | `"Pip."` | Idle: `Wake`, Busy: `[]` | `score_max = 0.057181` (`false`) | Proves `"Pip"` alone does not fire `hey_pip.onnx`. |
| `control_human_pip_hey_reversed.wav` | `edited_spliced_ablation_control` | `"Pip! Hey!"` | Idle: `Wake` + `Say("Hey!")`, Busy: `[]` | `score_max = 0.013264` (`false`) | Proves reversed word order does not fire `hey_pip.onnx`. |

No EQ, gain, pitch/speed manipulation, signal tuning, or threshold adjustment was performed. Inter-word spacing robustness on `Dvortygirl` `"Hey"` + `"Pip"` was observed without tuning across `0 ms` (`0.9152`), `20 ms` (`0.9856`), `40 ms` (`0.9819`/`0.9850`), and `80 ms` (`0.9577`), plus same-speaker `"Hi, Pip."` (`En-us-hi.ogg` + `En-us-pip.ogg`, `0.8605`).

## Upstream License Credits & Pinned Digests

All pinned URLs and SHA-256 digests live in [`recipe.lock.json`](./recipe.lock.json):

1. **LibriVox — *Great Expectations*, Chapter 17** (`greatexpectations_17_dickens.mp3`)
   - Author: Charles Dickens; Reader: Mark F. Smith (`great_expectations_mfs_0812_librivox`)
   - License: **Public Domain** (`http://creativecommons.org/licenses/publicdomain/`)
   - SHA-256: `78bc98825c2500d65a720467f97a134a922b7ac85253d91b8594022139f03d84`
2. **Wikimedia Commons — `File:En-us-hey.ogg`**
   - Speaker/Author: `Dvortygirl`
   - License: **Public Domain (`PD-self`)**
   - SHA-256: `0072ba1654084c9df0dba3b7865046c13e91824edeba451e869380afa1eb7bc8`
3. **Wikimedia Commons — `File:En-us-pip.ogg`**
   - Speaker/Author: `Dvortygirl` (via `Commons:Shtooka`)
   - License: **CC BY-SA 3.0** (`https://creativecommons.org/licenses/by-sa/3.0/`, dual-licensed GFDL / CC BY-SA 3.0, 2.5, 2.0, 1.0)
   - Attribution: *Dvortygirl (Wikimedia Commons, File:En-us-pip.ogg), CC BY-SA 3.0*
   - Usage: Fetched locally into gitignored `.cache/` for local/CI component-level test execution only; never committed or distributed in the repository.
   - SHA-256: `6965bc8f3138572a75c0ee544b5695bef6b2af6f8dca75207cad956abc0c0df7`
4. **VOiCES / LibriSpeech `sp0307`** (`Lab41-SRI-VOiCES-src-sp0307-ch127535-sg0042.wav`, already pinned in `tests/pip-e2e/speech/assets.lock.json`)
   - License: **CC-BY-4.0** (`https://creativecommons.org/licenses/by/4.0/`)
   - SHA-256: `c65fcd726d6b08c82c1e5dc7558f863cd8d483e3ed2f4a7bcf271dc1865ada14`

## Reproducible Materialization & ARM64 Validation

Materialize all clips into gitignored `tests/pip-e2e/.cache/speech/human-wake-positive/` inside the locked ARM64 speech container (uses `libsndfile1` already installed in both `robot-buddy-pip-speech:arm64` and `robot-buddy-pip-e2e:arm64`):

```sh
docker run --rm --platform linux/arm64 \
  -v "$(pwd):/workspace:ro" \
  -v "$(pwd)/tests/pip-e2e/.cache:/opt/pip-e2e/cache" \
  robot-buddy-pip-speech:arm64 \
  python3 /workspace/tests/pip-e2e/speech/human-wake-recipe/prepare.py \
    --cache /opt/pip-e2e/cache \
    --verify-output-hashes
```

Validate through the locked ARM64 speech qualification suite (note: keep `edited_spliced_human` labeled separately from natural held-out recordings in any release report):

```sh
PIP_E2E_HEY_PIP_ONNX=/path/to/authorized/hey_pip.onnx \
PIP_E2E_HUMAN_WAKE_POSITIVE_WAV="$(pwd)/tests/pip-e2e/.cache/speech/human-wake-positive/edited_spliced_human_hey_pip.wav" \
tests/pip-e2e/speech/run.sh --skip-build
```

For full-turn E2E neural-wake + follow-up command testing in `tests/pip-e2e/runner.mjs`, pass:

```sh
PIP_E2E_HUMAN_WAKE_POSITIVE_WAV="$(pwd)/tests/pip-e2e/.cache/speech/human-wake-positive/edited_spliced_human_hey_pip_command.wav"
```

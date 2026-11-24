# Pip hardware port and external components

The Pip on-device port was undertaken in collaboration with ToschLogic. The neck choreography, expression poses and tuning in `apps/robot/src/motion/poses.rs`, and the local voice coloring in `apps/robot/src/piper.rs`, adapt behavior from the private **butty** project. They are not independently invented Robot Buddy assets. Permission for this port does not itself document a separate MIT relicense or a grant to redistribute the original project or its model weights. Confirm applicable terms with the respective rights holders before distributing a downstream bundle.

The robot client does not link Piper or espeak-ng. Optional offline speech launches the separately installed Piper executable; GPL versions of Piper and its espeak-ng dependency retain their own terms. Redistributing those programs, rather than installing them separately, requires handling their licenses and applicable source-distribution obligations. Voice files have their own model cards. The full vision build dynamically links system OpenCV libraries. Keep their package notices and the license/model card of each separately installed inference model. No private weights, recordings, or firmware sources are shipped here.

## Credits

- **butty** is Brittian's private project and Pip's original robot: the 3D-printed body and CAD, the ESP32-S3 face firmware, and a Python "brain" that this Rust port replaces on the Pi. Its `SPEC.md` names the repository `ToschLogic/butty`. Ask Brittian for access. No butty source, firmware, model or recording is copied into this repository.
- **ToschLogic** collaborated on the port.
- This repository's own code is under the MIT [LICENSE](LICENSE). The butty-derived behaviour and the third-party components below are covered by the notes above, not by that grant alone.

## What adapts or depends on butty

These are the files that adapt butty behaviour or depend on butty assets, found by comparing the port with the butty tree:

| File | Relationship to butty |
| --- | --- |
| `apps/robot/src/motion/poses.rs` | Neck gestures and dance, expression poses and their tuning, adapted from butty's motion behaviour |
| `apps/robot/src/piper.rs` | Offline voice coloring (a slower Piper `--length-scale`, played back 1.3× faster for a higher voice), adapted from butty's Pip voice |
| `apps/robot/src/face.rs` | Speaks the line protocol of butty's ESP32-S3 face firmware (`{"e":…}`, `{"look":…}`, `{"led":…}`, `{"qr":…}`, `hello`/`hb`). It interoperates with that firmware and does not include it |
| `apps/robot/src/listen/wake.rs` | Loads the private `hey_pip.onnx` wake classifier, trained for butty. The model is not in this repository |

The hardware design (parts, wiring, CAD) is butty's. [DEPLOYMENT.md](DEPLOYMENT.md) summarises it and points to the butty repo.

## Third-party components

None of these are bundled in this repository or in the robot binary, except where a row says "linked". The licences are those published upstream at the time of writing. Check the version you install.

| Component | How Pip uses it | Licence (upstream) |
| --- | --- | --- |
| Rivet engine and `rivetkit`, `rivetkit-client`, `rivet-error` crates 3.1.1 | Engine runs in the server image. Crates linked | Apache-2.0 (`rivet-dev/rivet`, crate metadata) |
| Rig 0.46.0 | Linked into the server | MIT (crate metadata) |
| Piper (`OHF-Voice/piper1-gpl`) | Optional separate executable for the offline line | GPL-3.0 |
| espeak-ng | Piper's phonemizer, installed with Piper | GPL-3.0 |
| Piper voices | Operator-installed `.onnx` + `.onnx.json` | Per voice model card |
| whisper.cpp v1.9.6 (`ggml-org/whisper.cpp`) | Separate `whisper-cli` executable | MIT |
| Whisper `ggml-tiny.en.bin` | Operator-installed model | MIT (OpenAI Whisper weights, GGML conversion by whisper.cpp) |
| ONNX Runtime 1.20.1 | Loaded at run time through `ORT_DYLIB_PATH` (`ort` crate, MIT OR Apache-2.0) | MIT |
| openWakeWord `melspectrogram.onnx`, `embedding_model.onnx` v0.5.1 | Operator-installed wake frontend | Code Apache-2.0. The release weights carry no separate model licence, so review them before distribution (see `tests/pip-e2e/speech/assets.lock.json`) |
| OpenCV 4.10 | Dynamically linked system or private-prefix libraries (`pi-vision`), through the `opencv` crate (MIT) | Apache-2.0 |
| YuNet `face_detection_yunet_2023mar.onnx` (OpenCV Zoo) | Operator-installed face detector | MIT (model directory `LICENSE`) |
| SFace `face_recognition_sface_2021dec.onnx` (OpenCV Zoo) | Optional operator-installed recognizer | Apache-2.0 (model directory `LICENSE`) |
| `rpicam-apps`, `aplay`/`arecord` (alsa-utils), NetworkManager | OS packages called as separate processes | Per distribution package |

These notes record facts about provenance and licences. They are not legal advice. For anything you plan to redistribute, confirm the terms with the rights holders.

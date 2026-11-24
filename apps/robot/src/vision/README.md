# Pi camera and local face gallery

The robot process starts one `rpicam-vid` child for the CSI camera. At 15 fps it drains packed I420 into a latest-frame slot; slow inference drops intermediate frames. Rust calls the OpenCV CPU detector and recognizer **in process**. The largest valid detected face supplies normalized gaze coordinates (right/down positive); invalid bounding boxes are skipped and no face clears the target. A camera timeout or disconnect clears the preview, tracking target and current recognition match, reports only bounded process exit status (exit code, signal, or stall without stderr/private data), and retries with backoff up to 30 seconds.

## Install on Raspberry Pi OS (64-bit Trixie)

1. Install `rpicam-apps`, `libopencv-dev` (4.8 or newer, including DNN, objdetect, imgproc and imgcodecs), `pkg-config`, `clang` and `libclang-dev`. A device receiving a prebuilt binary needs the matching OpenCV **runtime** libraries and `rpicam-apps`, not the compiler. Use `ldd /usr/local/bin/robot-buddy-robot` to check that no library is missing.
2. From the repo root, build with `cargo build --locked --release -p robot-buddy-robot --features pi-vision`, or use the [arm64 build image](../../../../infra/pi-build.Dockerfile). Install the binary as in the [device unit](../../../../infra/robot-buddy.service). A default build has no OpenCV dependency; setting `BUDDY_VISION=1` on that binary reports that `pi-vision` is required.
3. Give the service account the `video` supplementary group and access to the CSI media/video/subdevice and DMA heap nodes. Follow device udev permissions; the camera does not require a root service. Before starting the service, verify capture as its user. The second command must print **165888** bytes (384 × 288 × 1.5); both Y and UV strides are multiples of the Pi 5 ISP's 64-byte alignment. It writes no image file.

   ```sh
   sudo -u robot-buddy rpicam-hello --list-cameras
   sudo -u robot-buddy rpicam-vid --nopreview --frames 1 --timeout 0 --codec yuv420 \
     --width 384 --height 288 --framerate 15 -o - | wc -c
   ```

4. Provision the ONNX files under `/opt/robot-buddy/models`, readable by the service and writable only by the installer. Use upstream OpenCV Zoo model bytes (not Git LFS pointer text). Verify the SHA-256 of each file against its LFS object ID at the pinned revision; review the license in the linked model directory for your intended use. Startup validation checks that configured model and tuning paths are absolute, readable, non-empty regular files and rejects Git LFS pointer files.

   | File | Upstream directory | SHA-256 |
   | --- | --- | --- |
   | `face_detection_yunet_2023mar.onnx` | [YuNet](https://github.com/opencv/opencv_zoo/tree/48b10fa505aeb51fbd127fc60a5c660040e823b7/models/face_detection_yunet) | `8f2383e4dd3cfbb4553ea8718107fc0423210dc964f9f4280604804ed2552fa4` |
   | `face_recognition_sface_2021dec.onnx` | [SFace](https://github.com/opencv/opencv_zoo/tree/48b10fa505aeb51fbd127fc60a5c660040e823b7/models/face_recognition_sface) | `0ba9fbfa01b5270c96627c4ef784da859931e02f04419c829e83484087c34e79` |

5. Set the following in the service environment, then restart the service. Leave `BUDDY_VISION_SFACE` unset for tracking and preview without creating a gallery. Optional NoIR tuning (`BUDDY_VISION_TUNING_FILE`) and fixed manual white-balance gains (`BUDDY_VISION_AWB_GAINS=r,b`, finite values in `0.05..=32.0`) are validated at startup and passed through to `rpicam-vid`. No model download runs at startup.

   ```ini
   BUDDY_VISION=1
   BUDDY_VISION_YUNET=/opt/robot-buddy/models/face_detection_yunet_2023mar.onnx
   BUDDY_VISION_SFACE=/opt/robot-buddy/models/face_recognition_sface_2021dec.onnx
   # BUDDY_VISION_GALLERY=/var/lib/robot-buddy/faces
   # BUDDY_VISION_CAMERA=/usr/bin/rpicam-vid
   # BUDDY_VISION_ROTATE_180=1
   # BUDDY_VISION_TUNING_FILE=/usr/share/libcamera/ipa/rpi/pisp/imx219_noir.json
   # BUDDY_VISION_AWB_GAINS=1.5,1.2
   ```

## Enroll, recognize and forget

Use the authenticated local operator panel with the person in front of the camera. Enrollment requires **exactly one valid face**, at least 40 pixels on each side, in a frame less than two seconds old. It runs YuNet again on that frame, aligns the five landmarks with SFace, then saves the normalized 128-value embedding. Add a second sample at another angle with the same name. Up to eight samples per person and 64 people are supported. Forget removes every sample for the selected name. Both forget and re-enroll clear any cached recognition label before mutating the on-disk gallery and trigger an immediate re-check on the next frame.

Only name and vectors persist, in a service-owned `0700` directory and `0600` files under the private state directory. Use a trusted, non-shared parent directory. The gallery has a single process-level writer lock (released explicitly on drop), rejects symlinks and insecure modes, and uses atomic writes. There is no import of another application's gallery. Changing recognizer models requires re-enrollment. JPEG previews are encoded statelessly outside the inference engine mutex only when requested, cached per captured frame to avoid redundant re-encoding; the module has no network or cloud client and writes no photos.

YuNet uses score 0.8 and NMS 0.3. SFace matches the largest valid face at most once per second while a face remains present (and immediately after a face re-enters or the gallery changes); the maximum cosine similarity over all samples must be at least 0.42. This is a local recognition hint, not an authentication decision.

## Integration and verification

`Config::from_env()` returns `None` when vision is off and validates configured paths, regular non-LFS model/tuning files, and optional AWB gains before returning. Give `run(config, panel.clone(), callback)` its own Tokio task; abort it during shutdown. The callback receives `Observation { target, recognition }` and must not block. `Recognition` explicitly distinguishes `Unchecked` (between recognition intervals or recognition disabled), `NoMatch` (checked with no match, face lost, or error/disconnect clear), and `Known(String)`. Transient per-frame inference errors are isolated: each failure clears the tracking target and current recognition match, rate-limits warning logs, and allows recovery on subsequent healthy frames; consecutive failures stop `run()` once the bounded policy limit (10 consecutive frames) is reached. While valid inference is active, `vision` emits `tracing::info!(vision_frames = N, "camera inference active")` at most once every 5 seconds with no coordinates or names. Hardware capture errors retry with backoff; startup or bounded inference failures return to the caller. The camera child is killed on task cancellation.

`Panel::default()` is initially detached. `jpeg()`, `recognized()`, `people()`, `enroll(name)` and `forget(name)` are synchronous; call them on a blocking pool from the authenticated operator API. `jpeg()` returns `Ok(None)` without a fresh preview and never contends with the detector/recognizer mutex. `recognized()` returns `Ok(None)` when offline, cleared, unmatched, or older than two seconds. Enrollment always uses the existing camera and never opens a second capture session.

```sh
cargo test --locked -p robot-buddy-robot vision::
cargo test --locked -p robot-buddy-robot --features pi-vision vision::
```

The first command uses fake camera/inference plus config, JPEG caching/decoupling, recognition interval, failure-recovery, filesystem, and capture-pipe tests. The native suite also converts a synthetic I420 frame and decodes the resulting JPEG. For the opt-in recognition test, supply the configured models and set `BUDDY_FIXTURE_ENROLL`, `BUDDY_FIXTURE_KNOWN`, `BUDDY_FIXTURE_UNKNOWN` to **private, single-frame, 384×288 packed I420 files**: two different views of one consenting person and one view of another. Run `cargo test --locked -p robot-buddy-robot --features pi-vision enrollment_matches_another_view -- --ignored`. This uses a temporary private gallery, never the installed one.

On the bench, check preview orientation, motion direction and return to center after the person leaves; verify recognition/forget across restart, camera unplug/replug, and cancellation while capturing. Measure CPU, memory and frame cadence with audio and motion active under the actual service limits. Module and fixture tests do not establish hardware accuracy or frame rate.

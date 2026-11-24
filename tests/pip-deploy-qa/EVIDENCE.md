# Fresh ARM64 Deployment Rehearsal Evidence (PR #1 — `feat/pip-hardware`)

## 1. Run Provenance & Reproduction

| Field | Value |
|---|---|
| Status | **PASS** (all phases 0–5 passed) |
| Run ID | `mwv0d836-9de72c` |
| Window (UTC) | `2026-11-24T18:08:33.361Z` → `2026-11-24T18:09:57.468Z` |
| Candidate product freeze commit | `d7a5b34` (`feat/pip-hardware`; zero diff in `apps/`, `crates/`, `infra/`, `Cargo.toml`, `Cargo.lock` vs `HEAD`) |
| Prior comparison commit | `b377904` (`b3779046f77027eee760a411a061d3d7117857f5`) |
| Rehearsal command | `bun tests/pip-deploy-qa/rehearsal.mjs` |
| Raw JSON artifact (gitignored) | `tests/pip-deploy-qa/.artifacts/rehearsal-mwv0d836-9de72c.json` |

## 2. Isolation & Platform Confinement Posture

* **Host:** macOS `26.6.2` (`Darwin 25.6.0`, Apple Silicon `arm64`). No standalone Linux VM hypervisor CLI (`limactl`, `colima`, `orb`, `tart`, `utmctl`, `qemu-system-aarch64`, `multipass`, `vfkit`) is installed on this host.
* **Confinement:** Two disposable `linux/arm64` containers inside Docker Desktop `29.7.2` (LinuxKit `7.0.12-linuxkit` `aarch64`, cgroup v2 `cgroupfs`) attached to a per-run private Docker bridge network (`pip-deploy-qa-net-<runId>`), torn down automatically in `finally`:
  * **Zero host mounts** (`hostMounts: []`), **no host cgroup tree bind mount**, **no host Docker socket**, **no `--privileged` flag**, **default Docker seccomp profile** retained.
  * **Pi guest (`pip-deploy-qa-pi-*`):** `--cgroupns=private`, `--cap-add SYS_ADMIN`, `--cap-add SYS_NICE`, `--tmpfs /run:exec,mode=755`, `--tmpfs /run/lock`, `--tmpfs /tmp`. Entrypoint remounts only its own private cgroup v2 namespace writable (`mount -o remount,rw /sys/fs/cgroup`) before `exec /lib/systemd/systemd`.
  * **Backend guest (`pip-deploy-qa-backend-*`):** `--cap-drop ALL`, `--security-opt no-new-privileges:true`, `--memory 1g`, `--pids-limit 512`, `--tmpfs /tmp:mode=1777,size=64m`. Separate host from the Pi guest per `DEPLOYMENT.md` §1 and `infra/robot-buddy.service`.
  * **Zero real credentials:** All keys, Test CA certs, Ed25519 issuer/device keys, OIDC secrets, and operator tokens are generated fresh per run and discarded on teardown. No real robot, production service, host systemd, or private reference checkout was touched.

## 3. Software & Binary Versions

| Component | Observed Version / Provenance |
|---|---|
| Pi guest OS | `Debian GNU/Linux 13 (trixie)`, `aarch64 (arm64)`, kernel `7.0.12-linuxkit` |
| systemd | `systemd 257 (257.13-1~deb13u1)` (`is-system-running` = `running`, 0 failed units at boot) |
| cgroup v2 controllers | `cpuset cpu io memory hugetlb pids rdma`; subtree active: `memory pids` |
| NetworkManager / polkitd | `network-manager 1.52.1-1`, `polkitd 126-2` |
| OpenCV runtime libraries | `libopencv-{core,imgproc,imgcodecs,dnn,objdetect,calib3d,features2d,flann}410` `4.10.0+dfsg-5` |
| Rivet engine (pinned) | `Rivet 3.1.1` (`rivetdev/engine:3.1.1@sha256:55cdfe9a92c0ee2f7d3ce5f388dab2ef8e0be1bab8e3f1adb7ba972f862e25d4`) |
| Backend Bun fixture host | `Bun 1.4.4` (`linux-aarch64`, SHA-256 verified) |
| **Candidate ELF (`d7a5b34`, Rust 1.100.0)** | `ELF 64-bit LSB pie executable, ARM aarch64`, `10,439,080` bytes, `BuildID[sha1]=c706f2b154ec4c94fb5926a635fbb17700ef7c7f`, SHA-256 `4660e15f750fc10c95db216568614051b7abc757760b4a7b6c9e58dc6c5348b8` (`--features pi-vision,onnx-wake`) |
| **Prior ELF (`b377904`, Rust 1.97.1)** | `ELF 64-bit LSB pie executable, ARM aarch64`, `10,242,048` bytes, `BuildID[sha1]=fb312d161b01618008aa50a93eaa6f84d7d5535e`, SHA-256 `c2e11f68b6d2c76469844a84c5b471607c6d3df8442620721fc18fe0a2a4a53a` (`--features pi-vision,onnx-wake`) |

## 4. Syntax / Static Checks (Distinct from Live Checks)

| Check | Result | Evidence |
|---|---|---|
| Candidate & Prior `file` / `ldd` | **PASS** | Both ARM64 ELF PIEs resolve all shared libraries (`0` `not found`), linking `/lib/aarch64-linux-gnu/libopencv_{core,imgproc,imgcodecs,dnn,objdetect,calib3d,features2d,flann}.so.410`. Distinct SHA-256 and ELF BuildID verified. |
| `systemd-analyze verify` on candidate base unit | **PASS** | Exit `0`, empty output (zero warnings/errors). |
| `systemd-analyze verify` on candidate + `20-pip.conf` | **PASS** | Exit `0`, empty output (zero warnings/errors). |
| `systemd-analyze security robot-buddy.service` | **PASS** | `Overall exposure level for robot-buddy.service: 2.0 OK :-)` |
| `systemd-analyze verify` on prior `b377904` unit | **Observed pre-fix defect** | `/etc/systemd/system/robot-buddy.service:107: System call ~@privileged is not known, ignoring.` (confirmed fixed in candidate `6c41452` / `d7a5b34`). |
| Polkit directory & rule priority | **PASS** | `/etc/polkit-1/rules.d` exists after `apt-get install polkitd`; candidate installs `40-robot-buddy-nm.rules`, prior used `50-robot-buddy-nm.rules`. |
| `rpicam-apps-lite` on stock Debian Trixie | **Distro gap (expected)** | Absent from stock `deb.debian.org` Trixie archive (`rpicam-apps-lite` is a Raspberry Pi OS archive package; `DEPLOYMENT.md` §3 targets Raspberry Pi OS Lite). |

## 5. Live Systemd, 1 GiB Cgroup & Sandbox Verification

### 5.1 Fresh Install & Base Unit (Phase 1 — `DEPLOYMENT.md` §3)

* Executed `DEPLOYMENT.md` §3 verbatim: `groupadd --system pip-control`, `useradd --system --user-group --home-dir /var/lib/robot-buddy --shell /usr/sbin/nologin robot-buddy`, `usermod -aG audio,dialout,pip-control robot-buddy`, `install` permissions for `/etc/robot-buddy` (`0750 root:robot-buddy`), `/var/lib/robot-buddy` (`0700 robot-buddy:robot-buddy`), `/etc/robot-buddy/device.identity.json` (`0600 robot-buddy:robot-buddy`), staged key deleted, `/etc/robot-buddy/device.env` (`0600 root:root`), `/etc/polkit-1/rules.d/40-robot-buddy-nm.rules` (`0644`), `/etc/systemd/system/robot-buddy.service` (`0644`).
* `runuser -u robot-buddy -- /usr/local/bin/robot-buddy-robot setup-card` created `/var/lib/robot-buddy/setup-wifi.json` (`robot-buddy:robot-buddy 600`) with SSID `RobotBuddy-kaxt78`.
* `systemctl enable --now robot-buddy` reached **`active (running)`** (`Type=notify`, `NotifyAccess=main`, `MainPID=1401`, `NRestarts=0`), base `MemoryMax=268435456` (256 MiB), `TasksMax=128`, `/run/robot-buddy` `robot-buddy:robot-buddy 711`, `/run/robot-buddy/control.sock` **`robot-buddy:pip-control 660`**.

### 5.2 `20-pip.conf` Drop-In & Effective 1 GiB Sandbox Enforcement (Phase 2 — `DEPLOYMENT.md` §4)

After `usermod -aG video robot-buddy`, installing `/etc/systemd/system/robot-buddy.service.d/20-pip.conf`, `systemctl daemon-reload`, and `systemctl restart robot-buddy`:

| Property / Kernel Check | Observed Live Value |
|---|---|
| Service state (`MainPID=1485`) | **`active (running)`**, `Type=notify`, `NotifyAccess=main`, `NRestarts=0` |
| **Effective `MemoryMax` (systemd & kernel cgroup)** | **`1073741824` (exact 1 GiB)** in `systemctl show` **and** `/sys/fs/cgroup/system.slice/robot-buddy.service/memory.max` |
| Observed memory usage (idle / dialogue) | `memory.current=18,599,936` B (~17.7 MiB), `memory.peak=18,870,272` B (~18.0 MiB) |
| **Effective `TasksMax` (systemd & kernel cgroup)** | **`256`** in `systemctl show` **and** `/sys/fs/cgroup/system.slice/robot-buddy.service/pids.max` (`pids.current=8`) |
| `User` / `Group` / `SupplementaryGroups` | `robot-buddy` / `robot-buddy` / **`audio dialout pip-control video`** (`/proc/1485/status` `Groups: 20 29 44 994 995`) |
| `Environment` | `BUDDY_CONTROL_SOCKET=/run/robot-buddy/control.sock OMP_NUM_THREADS=2 OPENBLAS_NUM_THREADS=1` |
| `Nice` / `IOSchedulingClass` / `IOSchedulingPriority` | `-5` / `2` (`best-effort`) / `2` |
| `WatchdogUSec` / `TimeoutStartUSec` / `TimeoutStopUSec` | `1min` / `1min` / `10s` |
| Capabilities (`/proc/1485/status`) | `CapInh=CapPrm=CapEff=CapBnd=CapAmb=0000000000000400` (**`CAP_NET_BIND_SERVICE` only**) |
| `NoNewPrivileges` & `Seccomp` (`/proc/1485/status`) | **`NoNewPrivs: 1`**, **`Seccomp: 2`** (`SECCOMP_MODE_FILTER`), `SystemCallArchitectures=native`, `SystemCallErrorNumber=1` (`EPERM`) |
| `SystemCallFilter` effective allowlist audit | Allows `chown`, `fchown`, `fchownat`; **blocks** `setuid`, `capset`, `mount`, `chroot`, `ptrace`, `reboot` |
| `MemoryDenyWriteExecute` / `LockPersonality` / `RemoveIPC` | `yes` / `yes` / `yes` |
| `ProtectSystem` / `ProtectHome` / `PrivateTmp` / `ProtectProc` | `strict` / `yes` / `yes` / `invisible` (`/proc/1485/mountinfo`: `/` is `ro,nosuid,relatime`; `/var/lib/robot-buddy` and `/run/robot-buddy` are `rw`) |
| Kernel / namespace / family guards | `ProtectKernelTunables=yes`, `ProtectKernelModules=yes`, `ProtectKernelLogs=yes`, `ProtectControlGroups=yes`, `ProtectClock=yes`, `ProtectHostname=yes`, `RestrictNamespaces=yes`, `RestrictRealtime=yes`, `RestrictSUIDSGID=yes`, `RestrictAddressFamilies=AF_INET AF_INET6 AF_NETLINK AF_UNIX`, `UMask=0077` |

**Live permission boundary checks:**
* `speech-qa` (member of `pip-control` only): control socket write **ALLOWED**; reading `/etc/robot-buddy/device.identity.json`, `/etc/robot-buddy/device.env`, or listing `/var/lib/robot-buddy` **DENIED**.
* `outsider-qa` (not in `pip-control`): control socket write **DENIED**.
* `robot-buddy` (unprivileged service account): direct read of root-only `/etc/robot-buddy/device.env` (`0600 root:root`) **DENIED**.

## 6. Identity Enrollment, OIDC Claim & Operator Panel (Phase 3)

* **Device enrollment (live HTTPS+WSS, no `--simulate`):** Robot authenticated with `/etc/robot-buddy/device.identity.json` (`deviceId=c27951e6-00fd-49da-8cae-7163fe3c626d`) via `POST /v1/challenge` + `POST /v1/enroll` against `https://backend.pip-qa.internal:8443` and connected over WSS to `https://rivet.pip-qa.internal:6443` (`bot_id=RtvI44BNnQK9GuKGGvTIsbWrJfnBmIpP5gIbjYJY_WQ`).
* **Pre-claim operator panel (`127.0.0.1:8091`):** Unauthenticated → `401`; valid Basic Auth while unclaimed → `403`.
* **Negative OIDC callback test (`error=access_denied`):** Returned `400`, did **not** echo `access_denied` in HTML, cleared `buddy_login` (`Max-Age=0`), and spent the in-flight login.
* **Positive OIDC claim flow:**
  * `GET /claim` returned `CSP: default-src 'none'; form-action 'self' https:; frame-ancestors 'none'; base-uri 'none'`, `Referrer-Policy: same-origin`, `buddy_claim` (`HttpOnly; SameSite=Strict; Path=/login; Max-Age=600; Secure`).
  * `POST /login` validated `Origin` + signed CSRF cookie, issued `buddy_login` (`HttpOnly; SameSite=Lax; Path=/auth/callback; Max-Age=300; Secure`), redirected to `https://oidc.pip-qa.internal:9443/authorize` with PKCE `S256`, `state`, and `nonce`.
  * `GET /auth/callback` exchanged code via `POST /token` (PKCE `S256` + `client_secret_post`), fetched `GET /userinfo`, validated RS256 ID token against `GET /jwks`, cleared `buddy_login`, and returned 6-digit code `383654`.
  * **Verified OIDC identity:** issuer `https://oidc.pip-qa.internal:9443`, client `pip-deploy-qa-client`, subject `pip-deploy-qa-owner-01`, canonical owner **`https://oidc.pip-qa.internal:9443|pip-deploy-qa-owner-01`** (audit counts: `discoveries=1`, `jwksFetches=1`, `authorizes=1`, `deniedAuthorizes=1`, `tokens=1`, `userinfos=1`).
* **Robot confirmation & post-claim verification:**
  * Loopback panel `http://127.0.0.1:8090/` displayed matching `383654`; `POST /confirm` with CSRF + `Origin: http://127.0.0.1:8090` returned `303`, panel transitioned to `Ready — hello!`, `claim-url` reported `No active claim URL (robot is already owned or waiting to start a claim).`.
  * Post-claim operator panel `http://127.0.0.1:8091/`: unauthenticated → `401`; valid Basic Auth → `200`.
  * Live utterance submitted via `/run/robot-buddy/control.sock` as `speech-qa` completed through real Rivet `Conversation` actor + mock model, advancing `/var/lib/robot-buddy/cache.json` `cursor` to `4`.

## 7. Genuine Cross-Version Upgrade & Rollback State Table (Phase 4 — `DEPLOYMENT.md` §5)

Used two genuinely distinct compiled ARM64 Trixie ELFs (`b377904` Rust 1.97.1 vs `d7a5b34` Rust 1.100.0) and their matching unit/polkit configs (`50-robot-buddy-nm.rules` ↔ `40-robot-buddy-nm.rules`). A sentinel file (`/var/lib/robot-buddy/faces/qa-sentinel.json`, `0600 robot-buddy:robot-buddy`) verified local gallery preservation.

| Stage | Active Binary (BuildID / SHA-256 prefix) | Polkit Rule | `memory.max` | Identity / Env / Wi-Fi / Gallery SHA-256 | Owned (`8091`=`200`, `claim-url`) | `bot_id` | `cache.json` `cursor` (after live turn) |
|---|---|---|---|---|---|---|---|
| Initial Candidate (`d7a5b34`) | `c706f2b1…` / `4660e15f…` | `40-robot-buddy-nm.rules` | `1073741824` (1 GiB) | Baseline (`9f02510d…` / `ec3dc17b…` / `a5b14a7f…` / `5af91af5…`) | Yes | `RtvI44BN…` | `4` |
| Step 4A: Prior (`b377904`) | `fb312d16…` / `c2e11f68…` | `50-robot-buddy-nm.rules` | `1073741824` (1 GiB) | **Identical** (modes `600` preserved) | Yes (no re-claim) | `RtvI44BN…` | `8` |
| Step 4B: Upgrade `b377904` → `d7a5b34` | `c706f2b1…` / `4660e15f…` | `40-robot-buddy-nm.rules` (legacy `50-*` removed) | `1073741824` (1 GiB) | **Identical** (modes `600` preserved) | Yes (no re-claim) | `RtvI44BN…` | `12` |
| Step 4C: Rollback `d7a5b34` → saved `b377904` backup | `fb312d16…` / `c2e11f68…` | `50-robot-buddy-nm.rules` (restored from backup) | `1073741824` (1 GiB) | **Identical** (modes `600` preserved) | Yes (no re-claim) | `RtvI44BN…` | `16` |

## 8. Watchdog Heartbeat & Log Redaction Summary (Phase 5)

* Candidate service restored at 1 GiB (`MemoryMax=1073741824`) and observed for `35 s` (`WatchdogSec=60s`, ping interval `30 s`):
  * `ActiveState=active`, `SubState=running`, `Result=success`, **`NRestarts=0`**.
  * **`WatchdogTimestamp=Tue 2026-11-24 18:09:52 UTC`**, `WatchdogTimestampMonotonic=84241388889` (proves real `WATCHDOG=1` datagram from `robot-buddy-robot` `MainPID` accepted by `systemd`).
  * `MemoryCurrent=17,543,168` B (~16.7 MiB), `MemoryPeak=18,812,928` B (~17.9 MiB), `TasksCurrent=5`.
* **Log redaction (`53` journal lines across full lifecycle + backend logs):**
  * **Device journal (`journalctl -u robot-buddy`):** `0` leaks of Ed25519 device private key, `BUDDY_OPERATOR_TOKEN`, `setup-wifi.json` password, or any of the 4 submitted conversation utterances (compact `event sequence=… turn=… kind=…` lines only). Pre-claim onboarding emitted `claim url=…` once per unclaimed process start (`2` total: base start + `20-pip.conf` restart) per `apps/robot/src/journal.rs`, and **`0`** times after ownership (consistent with `DEPLOYMENT.md` §3 warning to treat device journals as private).
  * **Backend logs (`robot-buddy-server`):** `0` leaks of claim token, OIDC client secret, or user utterances; 4 model calls recorded across 14 registration TLS connections and 12 Rivet WSS/HTTP TLS connections.

## 9. Fixtures vs Real Hardware Boundaries & Observations

* **What this rehearsal proves:** Real ARM64 Trixie `systemd` lifecycle (`Type=notify`, `WatchdogSec=60s`, `20-pip.conf` 1 GiB cgroup cap, full unit sandbox enforcement, `control.sock` group separation), real HTTPS/WSS Ed25519 enrollment + OIDC PKCE/RS256 claiming + 6-digit panel confirmation + operator panel gating, and genuine cross-version (`b377904` ↔ `d7a5b34`) upgrade/rollback preserving identity, ownership, cache cursor, setup Wi-Fi, env, and gallery directory.
* **Fixtures used (not production services):** Disposable EC P-256 Test CA, isolated mock RS256 OIDC IdP, isolated mock OpenAI-compatible streaming model, sentinel gallery file.
* **Requires physical Raspberry Pi 5 bench verification (not proven here):** 5 V / 5 A Buck A/B power rails and `usb_max_current_enable=1`, thermal throttling, MAX98357A ALSA playback, reSpeaker 2-channel capture and echo cancellation, ESP32-S3 USB face heartbeat, STS3215 neck servo EEPROM limits and torque-off safety, OV5647 CSI camera + `rpicam-apps-lite` (Raspberry Pi OS archive package), and real `wlan0` NetworkManager AP/client transitions.
* **Doc/unit alignment check:** `DEPLOYMENT.md`, `infra/robot-buddy.service`, `infra/robot-buddy-pip.conf`, and `infra/robot-buddy-nm.rules` (as updated in `6c41452`) are consistent with the fresh rehearsal; no further doc or product-code defects were uncovered on frozen candidate `d7a5b34`.

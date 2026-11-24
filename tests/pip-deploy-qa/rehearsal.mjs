#!/usr/bin/env bun
// Fresh Pi-like ARM64 systemd deployment rehearsal following DEPLOYMENT.md.
//
// Runs two isolated linux/arm64 containers on a private disposable Docker bridge
// network — ZERO host bind mounts, ZERO host Docker socket, ZERO host cgroup mount,
// ZERO `--privileged`, ZERO real credentials:
//   1. `pip-qa-backend`: real `robot-buddy-server` + pinned `rivetdev/engine:3.1.1` +
//      ephemeral Test CA TLS proxies + mock RS256 OIDC IdP + mock streaming model.
//   2. `pip-qa-pi`: fresh Debian Trixie ARM64 guest booted with REAL `/lib/systemd/systemd`
//      as PID 1 inside its own `--cgroupns=private` cgroup v2 namespace.
//
// Upgrade & rollback uses TWO GENUINE ARM64 Trixie ELFs compiled from distinct git
// revisions: a real prior revision (default `b377904`, Rust 1.97.1, pre-hardening unit
// + `50-robot-buddy-nm.rules`) vs the current candidate revision (Rust 1.100.0, hardened
// unit + `40-robot-buddy-nm.rules`). Never uses objcopy/metadata patching.
//
// Usage:
//   bun tests/pip-deploy-qa/rehearsal.mjs [--skip-build] [--keep] [--prior-ref=b377904] [--watchdog-wait-secs=35]

import { spawnSync } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';

const REPO_ROOT = path.resolve(import.meta.dir, '../..');
const ARTIFACTS_DIR = path.join(import.meta.dir, '.artifacts');
const WORK_DIR = path.join(import.meta.dir, '.work');
const RUN_ID = `${Date.now().toString(36)}-${crypto.randomBytes(3).toString('hex')}`;
const NET_NAME = `pip-deploy-qa-net-${RUN_ID}`;
const BACKEND_CTR = `pip-deploy-qa-backend-${RUN_ID}`;
const PI_CTR = `pip-deploy-qa-pi-${RUN_ID}`;
const BACKEND_IMAGE = 'robot-buddy:pip-deploy-qa-backend';
const PI_IMAGE = 'robot-buddy:pip-deploy-qa-pi-systemd';

const args = process.argv.slice(2);
const SKIP_BUILD = args.includes('--skip-build');
const KEEP_CONTAINERS = args.includes('--keep');
const priorArg = args.find((a) => a.startsWith('--prior-ref='));
const PRIOR_REF = priorArg ? priorArg.split('=')[1] : 'b377904';
const watchdogArg = args.find((a) => a.startsWith('--watchdog-wait-secs='));
const WATCHDOG_WAIT_SECS = watchdogArg ? Number(watchdogArg.split('=')[1]) : 35;

const OPERATOR_TOKEN = `qa-operator-token-${crypto.randomBytes(16).toString('hex')}`;
const UTTERANCE_INITIAL_CANDIDATE = 'Rehearsal initial candidate canary alpha';
const UTTERANCE_PRIOR_ROLLBACK_1 = 'Rehearsal prior revision canary beta';
const UTTERANCE_UPGRADED_CANDIDATE = 'Rehearsal upgraded candidate canary gamma';
const UTTERANCE_FINAL_ROLLBACK_PRIOR = 'Rehearsal final rollback prior canary delta';

fs.mkdirSync(ARTIFACTS_DIR, { recursive: true });
fs.mkdirSync(WORK_DIR, { recursive: true });

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function run(cmd, argv, opts = {}) {
  const res = spawnSync(cmd, argv, {
    cwd: opts.cwd ?? REPO_ROOT,
    encoding: 'utf8',
    maxBuffer: 16 * 1024 * 1024,
    input: opts.input,
    env: opts.env ?? process.env,
  });
  if (!opts.allowFail && res.status !== 0) {
    throw new Error(
      `Command failed (exit ${res.status}): ${cmd} ${argv.join(' ')}\nSTDOUT:\n${res.stdout}\nSTDERR:\n${res.stderr}`,
    );
  }
  return res;
}

function dockerExec(container, shellScript, opts = {}) {
  return run('docker', ['exec', '-i', container, '/bin/sh', '-eu'], {
    ...opts,
    input: shellScript,
  });
}

function assert(cond, msg) {
  if (!cond) throw new Error(`ASSERTION FAILED: ${msg}`);
}

function parseProps(raw) {
  const obj = {};
  for (const line of raw.split('\n')) {
    const eq = line.indexOf('=');
    if (eq > 0) obj[line.slice(0, eq)] = line.slice(eq + 1);
  }
  return obj;
}

const priorCommitFull = run('git', ['rev-parse', PRIOR_REF]).stdout.trim();
const priorCommitShort = run('git', ['rev-parse', '--short', PRIOR_REF]).stdout.trim();

const report = {
  runId: RUN_ID,
  startedAt: new Date().toISOString(),
  finishedAt: null,
  status: 'running',
  gitCommit: run('git', ['rev-parse', '--short', 'HEAD']).stdout.trim(),
  gitBranch: run('git', ['branch', '--show-current']).stdout.trim(),
  priorRevision: { ref: PRIOR_REF, short: priorCommitShort, full: priorCommitFull },
  hostEnvironment: {},
  isolationPosture: {
    containerRuntime: 'Docker Desktop LinuxKit ARM64 VM',
    privilegedFlag: false,
    hostMounts: [],
    hostCgroupBindMount: false,
    hostDockerSocketMount: false,
    cgroupNamespace: 'private',
    capabilitiesAdded: ['SYS_ADMIN', 'SYS_NICE'],
    dockerSeccompProfile: 'default',
    realCredentialsUsed: false,
    disposableVmNote:
      'No standalone hypervisor CLI (limactl/colima/orb/tart/utmctl/qemu-system-aarch64) is installed on this macOS host; confinement uses disposable ARM64 containers inside Docker Desktop LinuxKit VM with --cgroupns=private and zero host mounts.',
  },
  fixturesVsHardware: {
    realComponents: [
      'Candidate robot-buddy-robot (release aarch64-unknown-linux-gnu --features pi-vision,onnx-wake, Rust 1.100.0, no --simulate)',
      `Prior robot-buddy-robot from git commit ${priorCommitShort} (release aarch64-unknown-linux-gnu --features pi-vision,onnx-wake, Rust 1.97.1, no --simulate)`,
      'robot-buddy-server (release aarch64, no BUDDY_ALLOW_INSECURE_LOCAL)',
      'rivetdev/engine:3.1.1 (pinned digest)',
      'Debian 13 Trixie ARM64 systemd PID 1 (live cgroup v2 memory & pids controllers, seccomp, mount namespaces, WatchdogSec, Type=notify)',
      `Both ${priorCommitShort} and candidate infra/{robot-buddy.service,robot-buddy-pip.conf,robot-buddy-nm.rules,device.env.example}`,
      'Ed25519 device identity (init + provision), TLS HTTPS+WSS, RS256 OIDC JWT validation + PKCE S256',
    ],
    fixturesUsed: [
      'Ephemeral EC P-256 Test CA installed into container /usr/local/share/ca-certificates/ (replaces public ACME CA)',
      'Isolated mock RS256 OIDC IdP (discovery, JWKS, authorize, token with PKCE S256, userinfo, plus negative error=access_denied)',
      'Isolated mock OpenAI-compatible streaming chat completions server',
      'Sentinel JSON file under /var/lib/robot-buddy/faces/ representing local face gallery state',
    ],
    notTestedHereHardwareRequired: [
      'Physical Raspberry Pi 5 hardware, 5 V / 5 A power supply, thermal throttling (vcgencmd)',
      'Physical MAX98357A I2S speaker playback and reSpeaker USB microphone capture / acoustic echo',
      'Physical ESP32-S3 USB face serial heartbeat and STS3215 neck servo EEPROM calibration / torque release',
      'Physical OV5647 CSI camera and rpicam-apps-lite (Raspberry Pi OS archive package absent from stock Debian Trixie)',
      'Real Wi-Fi AP/client transitions via NetworkManager on physical wlan0',
    ],
  },
  versions: {},
  syntaxAndStaticChecks: {},
  liveChecks: {},
  stateSnapshots: {},
  docFindings: [],
  logsSummary: {},
  error: null,
};

function log(msg) {
  console.log(`[pip-deploy-qa] ${msg}`);
}

try {
  // ---------------------------------------------------------------------------
  // Host & Docker version capture
  // ---------------------------------------------------------------------------
  report.hostEnvironment = {
    uname: run('uname', ['-a']).stdout.trim(),
    dockerVersion: run('docker', [
      'info',
      '--format',
      '{{.ServerVersion}} {{.Architecture}} {{.OSType}} cgroup={{.CgroupDriver}}/v{{.CgroupVersion}}',
    ]).stdout.trim(),
  };
  log(`Host: ${report.hostEnvironment.dockerVersion}`);

  // ---------------------------------------------------------------------------
  // Prepare real prior-revision ARM64 ELF & infra files from git commit PRIOR_REF
  // ---------------------------------------------------------------------------
  const priorSrcDir = path.join(WORK_DIR, `prior-src-${priorCommitShort}`);
  const priorOutDir = path.join(WORK_DIR, `prior-out-${priorCommitShort}`);
  const priorBinPath = path.join(priorOutDir, 'robot-buddy-robot');

  // Always extract fresh prior infra/ files from git archive (fast, <1s)
  fs.rmSync(priorSrcDir, { recursive: true, force: true });
  fs.mkdirSync(priorSrcDir, { recursive: true });
  const archive = spawnSync('git', ['archive', priorCommitFull], {
    cwd: REPO_ROOT,
    maxBuffer: 64 * 1024 * 1024,
  });
  assert(archive.status === 0, `git archive ${priorCommitFull} failed`);
  const untar = spawnSync('tar', ['-x', '-C', priorSrcDir], { input: archive.stdout });
  assert(untar.status === 0, `tar extract of ${priorCommitShort} failed`);

  // Reuse prebuilt prior ELF if present (e.g. from earlier run in .work/prior-out) or build via prior's pi-build.Dockerfile
  const cachedPriorBin = path.join(WORK_DIR, 'prior-out', 'robot-buddy-robot');
  if (!fs.existsSync(priorBinPath) && fs.existsSync(cachedPriorBin) && PRIOR_REF === 'b377904') {
    fs.mkdirSync(priorOutDir, { recursive: true });
    fs.copyFileSync(cachedPriorBin, priorBinPath);
    fs.chmodSync(priorBinPath, 0o755);
  }
  if (!fs.existsSync(priorBinPath) || !SKIP_BUILD) {
    if (!fs.existsSync(priorBinPath)) {
      log(`Building REAL prior-revision (${priorCommitShort}) ARM64 Trixie ELF via its pi-build.Dockerfile...`);
      fs.mkdirSync(priorOutDir, { recursive: true });
      run('docker', [
        'buildx', 'build', '--platform', 'linux/arm64',
        '-f', path.join(priorSrcDir, 'infra/pi-build.Dockerfile'),
        '--output', `type=local,dest=${priorOutDir}`,
        priorSrcDir,
      ]);
    } else {
      log(`Using real prior-revision (${priorCommitShort}) ARM64 ELF at ${path.relative(REPO_ROOT, priorBinPath)}`);
    }
  }
  assert(fs.existsSync(priorBinPath), `Missing prior-revision ELF at ${priorBinPath}`);

  // ---------------------------------------------------------------------------
  // Build isolated ARM64 backend & candidate Pi systemd images
  // ---------------------------------------------------------------------------
  if (!SKIP_BUILD) {
    log('Building isolated ARM64 backend image (Dockerfile.backend)...');
    run('docker', [
      'buildx', 'build', '--platform', 'linux/arm64', '--load',
      '-f', 'tests/pip-deploy-qa/Dockerfile.backend',
      '-t', BACKEND_IMAGE, '.',
    ]);
    log('Building fresh ARM64 Pi systemd image with candidate ELF (Dockerfile.pi-systemd)...');
    run('docker', [
      'buildx', 'build', '--platform', 'linux/arm64', '--load',
      '-f', 'tests/pip-deploy-qa/Dockerfile.pi-systemd',
      '-t', PI_IMAGE, '.',
    ]);
  } else {
    log('Skipping image build (--skip-build)');
  }

  // ---------------------------------------------------------------------------
  // Create isolated private bridge network & boot backend container
  // ---------------------------------------------------------------------------
  log(`Creating isolated Docker bridge network ${NET_NAME}...`);
  run('docker', ['network', 'create', '--driver', 'bridge', NET_NAME]);

  log(`Starting isolated backend container ${BACKEND_CTR}...`);
  run('docker', [
    'run', '-d',
    '--name', BACKEND_CTR,
    '--platform', 'linux/arm64',
    '--network', NET_NAME,
    '--network-alias', 'backend.pip-qa.internal',
    '--network-alias', 'rivet.pip-qa.internal',
    '--network-alias', 'oidc.pip-qa.internal',
    '--tmpfs', '/tmp:mode=1777,size=64m',
    '--cap-drop', 'ALL',
    '--security-opt', 'no-new-privileges:true',
    '--memory', '1g',
    '--pids-limit', '512',
    BACKEND_IMAGE,
  ]);

  let backendStatus = null;
  for (let i = 0; i < 120; i++) {
    const probe = run(
      'docker',
      ['exec', BACKEND_CTR, 'curl', '-fsS', 'http://127.0.0.1:19999/status'],
      { allowFail: true },
    );
    if (probe.status === 0) {
      const parsed = JSON.parse(probe.stdout);
      if (parsed.ready) {
        backendStatus = parsed;
        break;
      }
    }
    await sleep(500);
  }
  assert(backendStatus?.ready, 'Isolated backend container did not become ready within 60s');
  log(`Backend ready! deviceId=${backendStatus.deviceId} oidc=${backendStatus.oidc.issuer}`);

  report.versions.backend = {
    rivetEngine: run('docker', ['exec', BACKEND_CTR, '/usr/local/bin/rivet-engine', '--version']).stdout.trim(),
    bun: run('docker', ['exec', BACKEND_CTR, '/usr/local/bin/bun', '--version']).stdout.trim(),
    osRelease: run('docker', ['exec', BACKEND_CTR, 'sh', '-c', '. /etc/os-release && echo "$PRETTY_NAME"']).stdout.trim(),
    backendKernel: run('docker', ['exec', BACKEND_CTR, 'uname', '-a']).stdout.trim(),
  };

  // Extract ONLY public CA cert and provisioned device identity JSON via short-lived host transfer dir
  const transferDir = path.join(ARTIFACTS_DIR, `transfer-${RUN_ID}`);
  fs.mkdirSync(transferDir, { recursive: true, mode: 0o700 });
  run('docker', ['cp', `${BACKEND_CTR}:/shared/ca.crt`, path.join(transferDir, 'ca.crt')]);
  run('docker', ['cp', `${BACKEND_CTR}:/shared/pip-01.identity.json`, path.join(transferDir, 'pip-01.identity.json')]);
  const provisionedIdentity = JSON.parse(fs.readFileSync(path.join(transferDir, 'pip-01.identity.json'), 'utf8'));

  // ---------------------------------------------------------------------------
  // Boot fresh ARM64 Pi systemd guest (ZERO host mounts, --cgroupns=private)
  // ---------------------------------------------------------------------------
  log(`Starting fresh Pi-like ARM64 systemd container ${PI_CTR}...`);
  run('docker', [
    'run', '-d',
    '--name', PI_CTR,
    '--platform', 'linux/arm64',
    '--network', NET_NAME,
    '--cgroupns=private',
    '--cap-add', 'SYS_ADMIN',
    '--cap-add', 'SYS_NICE',
    '--tmpfs', '/run:exec,mode=755',
    '--tmpfs', '/run/lock',
    '--tmpfs', '/tmp',
    '--stop-signal', 'SIGRTMIN+3',
    '-e', 'container=docker',
    PI_IMAGE,
  ]);

  let sysState = '';
  for (let i = 0; i < 40; i++) {
    const r = run('docker', ['exec', PI_CTR, 'systemctl', 'is-system-running'], { allowFail: true });
    sysState = (r.stdout || '').trim();
    if (sysState === 'running' || sysState === 'degraded') break;
    await sleep(500);
  }
  log(`Pi guest systemd state: ${sysState}`);
  assert(sysState === 'running' || sysState === 'degraded', `systemd did not finish booting: ${sysState}`);

  // Record Pi guest OS/systemd/cgroup/package versions
  const piVersions = parseProps(
    dockerExec(PI_CTR, `
      . /etc/os-release
      echo "OS=$PRETTY_NAME"
      echo "ARCH=$(uname -m) ($(dpkg --print-architecture))"
      echo "KERNEL=$(uname -r)"
      echo "SYSTEMD=$(systemctl --version | head -n 1)"
      echo "OPENCV_PACKAGES=$(dpkg-query -W -f='\${Package}=\${Version} ' 'libopencv-*' 2>/dev/null || true)"
      echo "NM_VERSION=$(dpkg-query -W -f='\${Version}' network-manager 2>/dev/null || true)"
      echo "POLKIT_VERSION=$(dpkg-query -W -f='\${Version}' polkitd 2>/dev/null || true)"
      echo "RPICAM_LITE_POLICY=$(apt-cache policy rpicam-apps-lite 2>&1 | tr '\\n' '; ' || true)"
      echo "CGROUP_CONTROLLERS=$(cat /sys/fs/cgroup/cgroup.controllers)"
      echo "CGROUP_SUBTREE=$(cat /sys/fs/cgroup/cgroup.subtree_control)"
    `).stdout.trim(),
  );
  report.versions.piGuest = piVersions;
  report.liveChecks.systemdBootState = sysState;
  const failedUnits = dockerExec(PI_CTR, 'systemctl --failed --no-legend --no-pager || true').stdout.trim();
  report.liveChecks.systemdFailedUnitsAtBoot = failedUnits ? failedUnits.split('\n') : [];

  // Stage public Test CA, candidate artifacts, real prior-revision artifacts, and single device identity
  run('docker', ['cp', path.join(transferDir, 'ca.crt'), `${PI_CTR}:/usr/local/share/ca-certificates/pip-deploy-qa-ca.crt`]);
  run('docker', ['cp', priorBinPath, `${PI_CTR}:/opt/qa-staging-prior/robot-buddy-robot`]);
  for (const f of ['robot-buddy.service', 'robot-buddy-nm.rules', 'robot-buddy-pip.conf', 'device.env.example']) {
    run('docker', ['cp', path.join(priorSrcDir, 'infra', f), `${PI_CTR}:/opt/qa-staging-prior/infra/${f}`]);
  }
  dockerExec(PI_CTR, `
    chmod 0755 /opt/qa-staging-prior/robot-buddy-robot
    umask 077
    mkdir -p /root/buddy-install/infra
    cp -a /opt/qa-staging-candidate/* /root/buddy-install/
    update-ca-certificates >/dev/null
  `);
  run('docker', ['cp', path.join(transferDir, 'pip-01.identity.json'), `${PI_CTR}:/root/buddy-install/pip-01.identity.json`]);
  fs.rmSync(transferDir, { recursive: true, force: true });

  // Verify TLS trust to backend from Pi guest
  dockerExec(PI_CTR, 'curl -fsS https://backend.pip-qa.internal:8443/ready >/dev/null');

  // ---------------------------------------------------------------------------
  // PHASE 0: SYNTAX / STATIC CHECKS (explicitly separated from live checks)
  // ---------------------------------------------------------------------------
  log('Phase 0: Running syntax & static checks on both candidate and prior ARM64 ELFs...');
  const qualifyElf = (binPath) => {
    const fileOut = dockerExec(PI_CTR, `file ${binPath}`).stdout.trim();
    const lddOut = dockerExec(PI_CTR, `ldd ${binPath}`).stdout.trim();
    const sha256 = dockerExec(PI_CTR, `sha256sum ${binPath} | awk '{print $1}'`).stdout.trim();
    const sizeBytes = Number(dockerExec(PI_CTR, `stat -c '%s' ${binPath}`).stdout.trim());
    const buildIdMatch = fileOut.match(/BuildID\[sha1\]=([0-9a-f]+)/);
    assert(fileOut.includes('ARM aarch64'), `Expected ARM aarch64 ELF for ${binPath}, got: ${fileOut}`);
    assert(!lddOut.includes('not found'), `Missing shared library in ldd for ${binPath}:\n${lddOut}`);
    assert(lddOut.includes('libopencv_core.so.410'), `Expected OpenCV 4.10 SONAME in ${binPath}:\n${lddOut}`);
    return {
      file: fileOut,
      buildId: buildIdMatch ? buildIdMatch[1] : null,
      sha256,
      sizeBytes,
      lddMissingCount: 0,
      lddOpenCvLibs: lddOut.split('\n').filter((l) => l.includes('opencv')).map((l) => l.trim()),
    };
  };

  const candidateElf = qualifyElf('/opt/qa-staging-candidate/robot-buddy-robot');
  const priorElf = qualifyElf('/opt/qa-staging-prior/robot-buddy-robot');
  assert(candidateElf.sha256 !== priorElf.sha256, 'Candidate and prior ELFs must have distinct SHA-256 digests');
  assert(candidateElf.buildId !== priorElf.buildId, 'Candidate and prior ELFs must have distinct ELF BuildIDs');

  const polkitDirExists = dockerExec(PI_CTR, 'test -d /etc/polkit-1/rules.d && echo YES || echo NO').stdout.trim();
  const polkitHeader = dockerExec(PI_CTR, 'head -n 1 /opt/qa-staging-candidate/infra/robot-buddy-nm.rules').stdout.trim();
  const priorPolkitHeader = dockerExec(PI_CTR, 'head -n 1 /opt/qa-staging-prior/infra/robot-buddy-nm.rules').stdout.trim();

  report.syntaxAndStaticChecks = {
    candidateElf,
    priorElf: { ref: priorCommitShort, ...priorElf },
    distinctBinaryAndBuildId: true,
    polkitRulesDirExistsAfterApt: polkitDirExists === 'YES',
    candidatePolkitRuleHeader: polkitHeader,
    priorPolkitRuleHeader: priorPolkitHeader,
    rpicamAppsLiteOnStockDebianTrixie:
      !piVersions.RPICAM_LITE_POLICY.includes('Candidate: (none)') &&
      piVersions.RPICAM_LITE_POLICY.includes('Candidate:'),
  };

  // ---------------------------------------------------------------------------
  // PHASE 1: Fresh DEPLOYMENT.md §3 Installation of Candidate & Base Unit Start
  // ---------------------------------------------------------------------------
  log('Phase 1: Executing fresh DEPLOYMENT.md §3 installation steps (candidate)...');
  dockerExec(PI_CTR, `
    cd /root/buddy-install
    getent group pip-control >/dev/null || groupadd --system pip-control
    id robot-buddy >/dev/null 2>&1 || useradd --system --user-group --home-dir /var/lib/robot-buddy \
      --shell /usr/sbin/nologin robot-buddy
    usermod -aG audio,dialout,pip-control robot-buddy
    install -d -m 0750 -o root -g robot-buddy /etc/robot-buddy
    install -d -m 0700 -o robot-buddy -g robot-buddy /var/lib/robot-buddy
    install -m 0755 robot-buddy-robot /usr/local/bin/
    ldd /usr/local/bin/robot-buddy-robot >/dev/null
    install -m 0600 -o robot-buddy -g robot-buddy pip-01.identity.json /etc/robot-buddy/device.identity.json
    rm pip-01.identity.json
    install -m 0600 -o root -g root infra/device.env.example /etc/robot-buddy/device.env
    install -m 0644 infra/robot-buddy-nm.rules /etc/polkit-1/rules.d/40-robot-buddy-nm.rules
    install -m 0644 infra/robot-buddy.service /etc/systemd/system/

    sed -i 's|^BUDDY_SERVER_URL=.*|BUDDY_SERVER_URL=https://backend.pip-qa.internal:8443|' /etc/robot-buddy/device.env
    sed -i 's|^BUDDY_SPEECH=.*|BUDDY_SPEECH=false|' /etc/robot-buddy/device.env
    printf '\\nBUDDY_OPERATOR_TOKEN=%s\\nBUDDY_OPERATOR_ADDR=127.0.0.1:8091\\n' '${OPERATOR_TOKEN}' >> /etc/robot-buddy/device.env
    chmod 0600 /etc/robot-buddy/device.env
  `);

  const stagedKeyRemoved = dockerExec(PI_CTR, 'test ! -e /root/buddy-install/pip-01.identity.json && echo REMOVED').stdout.trim();
  assert(stagedKeyRemoved === 'REMOVED', 'Staged pip-01.identity.json was not removed');

  // Verify `setup-card` unprivileged as robot-buddy
  const setupCardOut = dockerExec(PI_CTR, 'runuser -u robot-buddy -- /usr/local/bin/robot-buddy-robot setup-card').stdout;
  const setupSsidMatch = setupCardOut.match(/RobotBuddy-[0-9a-z]+/);
  assert(setupSsidMatch, `setup-card did not print a RobotBuddy-* SSID:\n${setupCardOut}`);
  const setupWifiStat = dockerExec(PI_CTR, "stat -c '%U:%G %a' /var/lib/robot-buddy/setup-wifi.json").stdout.trim();
  assert(setupWifiStat === 'robot-buddy:robot-buddy 600', `Unexpected setup-wifi.json stat: ${setupWifiStat}`);

  // SYNTAX check: systemd-analyze verify on base candidate unit
  const verifyBase = dockerExec(PI_CTR, 'systemd-analyze verify /etc/systemd/system/robot-buddy.service 2>&1', { allowFail: true });
  report.syntaxAndStaticChecks.systemdAnalyzeVerifyCandidateBase = {
    exitCode: verifyBase.status,
    output: verifyBase.stdout.trim(),
  };
  assert(verifyBase.status === 0 && verifyBase.stdout.trim() === '', `systemd-analyze verify base unit had warnings/errors:\n${verifyBase.stdout}`);

  // LIVE check: enable and start base unit (256 MiB / 128 tasks)
  log('Starting candidate robot-buddy.service (base 256 MiB unit)...');
  dockerExec(PI_CTR, 'systemctl daemon-reload && systemctl enable --now robot-buddy');

  const waitForActiveService = () =>
    dockerExec(PI_CTR, `
      for i in $(seq 1 40); do
        st=$(systemctl show robot-buddy -p ActiveState --value)
        sub=$(systemctl show robot-buddy -p SubState --value)
        if [ "$st" = "active" ] && [ "$sub" = "running" ] && [ -S /run/robot-buddy/control.sock ]; then
          break
        fi
        sleep 0.25
      done
    `);
  waitForActiveService();

  const baseProps = parseProps(
    dockerExec(PI_CTR, `
      systemctl show robot-buddy \
        -p ActiveState -p SubState -p MainPID -p Type -p NotifyAccess \
        -p MemoryMax -p TasksMax -p SupplementaryGroups -p NRestarts
    `).stdout.trim(),
  );
  const baseSockStat = dockerExec(PI_CTR, "stat -c '%U:%G %a' /run/robot-buddy/control.sock").stdout.trim();
  const baseRunDirStat = dockerExec(PI_CTR, "stat -c '%U:%G %a' /run/robot-buddy").stdout.trim();
  assert(baseProps.ActiveState === 'active' && baseProps.SubState === 'running', `Base unit not active(running): ${JSON.stringify(baseProps)}`);
  assert(baseProps.MemoryMax === '268435456', `Expected base MemoryMax=268435456 (256MiB), got ${baseProps.MemoryMax}`);
  assert(baseProps.TasksMax === '128', `Expected base TasksMax=128, got ${baseProps.TasksMax}`);
  assert(baseSockStat === 'robot-buddy:pip-control 660', `Expected control.sock robot-buddy:pip-control 660, got ${baseSockStat}`);
  assert(baseRunDirStat === 'robot-buddy:robot-buddy 711', `Expected /run/robot-buddy 711, got ${baseRunDirStat}`);

  report.liveChecks.phase1BaseInstall = {
    passed: true,
    setupCardSsid: setupSsidMatch[0],
    setupWifiStat,
    stagedKeyRemoved: true,
    baseUnitProperties: baseProps,
    controlSocketStat: baseSockStat,
    runtimeDirStat: baseRunDirStat,
  };
  log(`Phase 1 PASSED: active(running) PID=${baseProps.MainPID}, MemoryMax=256MiB, control.sock=${baseSockStat}`);

  // ---------------------------------------------------------------------------
  // PHASE 2: Install 20-pip.conf Drop-In & Verify Effective 1 GiB Cap + Sandbox
  // ---------------------------------------------------------------------------
  log('Phase 2: Installing 20-pip.conf drop-in (+ usermod -aG video) and verifying 1 GiB cap & kernel sandbox...');
  dockerExec(PI_CTR, `
    usermod -aG video robot-buddy
    install -D -m 0644 /root/buddy-install/infra/robot-buddy-pip.conf /etc/systemd/system/robot-buddy.service.d/20-pip.conf
    systemctl daemon-reload
    systemctl restart robot-buddy
  `);
  waitForActiveService();

  const verifyWithDropIn = dockerExec(PI_CTR, 'systemd-analyze verify /etc/systemd/system/robot-buddy.service 2>&1', { allowFail: true });
  const securityScore = dockerExec(PI_CTR, 'systemd-analyze security robot-buddy.service 2>&1 | tail -n 1', { allowFail: true }).stdout.trim();
  report.syntaxAndStaticChecks.systemdAnalyzeVerifyCandidateWithDropIn = {
    exitCode: verifyWithDropIn.status,
    output: verifyWithDropIn.stdout.trim(),
    securityOverallLine: securityScore,
  };
  assert(verifyWithDropIn.status === 0 && verifyWithDropIn.stdout.trim() === '', `systemd-analyze verify with drop-in failed:\n${verifyWithDropIn.stdout}`);

  const pipProps = parseProps(
    dockerExec(PI_CTR, `
      systemctl show robot-buddy \
        -p ActiveState -p SubState -p MainPID -p Type -p NotifyAccess \
        -p User -p Group -p SupplementaryGroups \
        -p MemoryMax -p MemoryCurrent -p MemoryPeak -p TasksMax -p TasksCurrent \
        -p WatchdogUSec -p TimeoutStartUSec -p TimeoutStopUSec -p NRestarts \
        -p Nice -p IOSchedulingClass -p IOSchedulingPriority \
        -p AmbientCapabilities -p CapabilityBoundingSet -p NoNewPrivileges \
        -p ProtectSystem -p ProtectHome -p PrivateTmp -p ReadWritePaths \
        -p ProtectKernelTunables -p ProtectKernelModules -p ProtectKernelLogs \
        -p ProtectControlGroups -p ProtectClock -p ProtectHostname \
        -p RestrictNamespaces -p RestrictRealtime -p RestrictSUIDSGID \
        -p LockPersonality -p MemoryDenyWriteExecute -p RemoveIPC -p ProtectProc \
        -p RestrictAddressFamilies -p SystemCallArchitectures -p SystemCallErrorNumber \
        -p UMask -p StateDirectoryMode -p RuntimeDirectoryMode -p Environment
    `).stdout.trim(),
  );

  const scfSet = new Set(
    dockerExec(PI_CTR, 'systemctl show robot-buddy -p SystemCallFilter --value').stdout.trim().split(/\s+/),
  );
  const scfChecks = {
    hasChown: scfSet.has('chown'),
    hasFchown: scfSet.has('fchown'),
    hasFchownat: scfSet.has('fchownat'),
    blocksSetuid: !scfSet.has('setuid'),
    blocksCapset: !scfSet.has('capset'),
    blocksMount: !scfSet.has('mount'),
    blocksChroot: !scfSet.has('chroot'),
    blocksPtrace: !scfSet.has('ptrace'),
    blocksReboot: !scfSet.has('reboot'),
  };

  assert(pipProps.ActiveState === 'active' && pipProps.SubState === 'running', `Pip drop-in unit not active(running): ${JSON.stringify(pipProps)}`);
  assert(pipProps.MemoryMax === '1073741824', `Expected MemoryMax=1073741824 (1GiB), got ${pipProps.MemoryMax}`);
  assert(pipProps.TasksMax === '256', `Expected TasksMax=256, got ${pipProps.TasksMax}`);
  assert(pipProps.SupplementaryGroups === 'audio dialout pip-control video', `Unexpected SupplementaryGroups: ${pipProps.SupplementaryGroups}`);
  assert(pipProps.MemoryDenyWriteExecute === 'yes', `Expected MemoryDenyWriteExecute=yes`);
  assert(pipProps.ProtectSystem === 'strict', `Expected ProtectSystem=strict`);
  assert(pipProps.NoNewPrivileges === 'yes', `Expected NoNewPrivileges=yes`);
  assert(pipProps.CapabilityBoundingSet === 'cap_net_bind_service', `Unexpected CapBnd: ${pipProps.CapabilityBoundingSet}`);
  assert(pipProps.AmbientCapabilities === 'cap_net_bind_service', `Unexpected CapAmb: ${pipProps.AmbientCapabilities}`);
  assert(pipProps.Environment.includes('OMP_NUM_THREADS=2') && pipProps.Environment.includes('OPENBLAS_NUM_THREADS=1'), `Missing thread env: ${pipProps.Environment}`);
  assert(Object.values(scfChecks).every(Boolean), `SystemCallFilter audit failed: ${JSON.stringify(scfChecks)}`);

  const kernelAudit = parseProps(
    dockerExec(PI_CTR, `
      PID="${pipProps.MainPID}"
      CG=$(awk -F: '{print $3}' "/proc/$PID/cgroup")
      echo "CGROUP_PATH=/sys/fs/cgroup$CG"
      echo "CGROUP_MEMORY_MAX=$(cat "/sys/fs/cgroup$CG/memory.max")"
      echo "CGROUP_MEMORY_CURRENT=$(cat "/sys/fs/cgroup$CG/memory.current")"
      echo "CGROUP_MEMORY_PEAK=$(cat "/sys/fs/cgroup$CG/memory.peak")"
      echo "CGROUP_PIDS_MAX=$(cat "/sys/fs/cgroup$CG/pids.max")"
      echo "CGROUP_PIDS_CURRENT=$(cat "/sys/fs/cgroup$CG/pids.current")"
      for field in Uid Gid Groups NoNewPrivs Seccomp CapInh CapPrm CapEff CapBnd CapAmb; do
        val=$(awk -v f="$field:" '$1 == f { $1=""; sub(/^[ \t]+/, ""); print }' "/proc/$PID/status")
        echo "PROC_$field=$val"
      done
      echo "PROC_MOUNT_ROOT_OPTS=$(awk '$5 == "/" { print $6; exit }' "/proc/$PID/mountinfo")"
      echo "PROC_MOUNT_USR_OPTS=$(awk '$5 == "/usr" { print $6; exit }' "/proc/$PID/mountinfo")"
      echo "PROC_MOUNT_VARLIB_OPTS=$(awk '$5 == "/var/lib/robot-buddy" { print $6; exit }' "/proc/$PID/mountinfo")"
      echo "PROC_MOUNT_RUN_OPTS=$(awk '$5 == "/run/robot-buddy" { print $6; exit }' "/proc/$PID/mountinfo")"
      echo "GROUP_IDS=$(for g in robot-buddy audio dialout video pip-control; do printf '%s:%s ' "$g" "$(getent group "$g" | cut -d: -f3)"; done)"
    `).stdout.trim(),
  );

  assert(kernelAudit.CGROUP_MEMORY_MAX === '1073741824', `Kernel cgroup memory.max != 1 GiB: ${kernelAudit.CGROUP_MEMORY_MAX}`);
  assert(kernelAudit.CGROUP_PIDS_MAX === '256', `Kernel cgroup pids.max != 256: ${kernelAudit.CGROUP_PIDS_MAX}`);
  assert(kernelAudit.PROC_NoNewPrivs === '1', `Kernel NoNewPrivs != 1`);
  assert(kernelAudit.PROC_Seccomp === '2', `Kernel Seccomp != 2`);
  assert(kernelAudit.PROC_CapEff === '0000000000000400', `Kernel CapEff != 0x400`);
  assert(kernelAudit.PROC_CapBnd === '0000000000000400', `Kernel CapBnd != 0x400`);
  assert(kernelAudit.PROC_CapAmb === '0000000000000400', `Kernel CapAmb != 0x400`);
  assert((kernelAudit.PROC_MOUNT_ROOT_OPTS || '').split(',').includes('ro'), `Root mount not ro`);
  assert((kernelAudit.PROC_MOUNT_VARLIB_OPTS || '').split(',').includes('rw'), `/var/lib/robot-buddy not rw`);
  assert((kernelAudit.PROC_MOUNT_RUN_OPTS || '').split(',').includes('rw'), `/run/robot-buddy not rw`);

  const permBoundary = parseProps(
    dockerExec(PI_CTR, `
      id speech-qa >/dev/null 2>&1 || useradd --system --user-group --groups pip-control --shell /usr/sbin/nologin speech-qa
      id outsider-qa >/dev/null 2>&1 || useradd --system --user-group --shell /usr/sbin/nologin outsider-qa
      runuser -u speech-qa -- sh -c 'printf "{\\"wake\\":true}\\n" | socat - UNIX-CONNECT:/run/robot-buddy/control.sock' 2>/dev/null && echo "SPEECH_SOCK_WRITE=ALLOWED" || echo "SPEECH_SOCK_WRITE=DENIED"
      runuser -u outsider-qa -- sh -c 'printf "{\\"wake\\":true}\\n" | socat - UNIX-CONNECT:/run/robot-buddy/control.sock' 2>/dev/null && echo "OUTSIDER_SOCK_WRITE=ALLOWED" || echo "OUTSIDER_SOCK_WRITE=DENIED"
      runuser -u speech-qa -- cat /etc/robot-buddy/device.identity.json >/dev/null 2>&1 && echo "SPEECH_READ_IDENTITY=ALLOWED" || echo "SPEECH_READ_IDENTITY=DENIED"
      runuser -u speech-qa -- cat /etc/robot-buddy/device.env >/dev/null 2>&1 && echo "SPEECH_READ_ENV=ALLOWED" || echo "SPEECH_READ_ENV=DENIED"
      runuser -u speech-qa -- ls /var/lib/robot-buddy >/dev/null 2>&1 && echo "SPEECH_LIST_STATE=ALLOWED" || echo "SPEECH_LIST_STATE=DENIED"
      runuser -u robot-buddy -- cat /etc/robot-buddy/device.env >/dev/null 2>&1 && echo "SERVICE_READ_ROOT_ENV=ALLOWED" || echo "SERVICE_READ_ROOT_ENV=DENIED"
    `).stdout.trim(),
  );
  assert(permBoundary.SPEECH_SOCK_WRITE === 'ALLOWED', 'speech-qa must write control.sock');
  assert(permBoundary.OUTSIDER_SOCK_WRITE === 'DENIED', 'outsider-qa must be denied control.sock');
  assert(permBoundary.SPEECH_READ_IDENTITY === 'DENIED', 'speech-qa must not read identity');
  assert(permBoundary.SPEECH_READ_ENV === 'DENIED', 'speech-qa must not read device.env');
  assert(permBoundary.SPEECH_LIST_STATE === 'DENIED', 'speech-qa must not list state dir');
  assert(permBoundary.SERVICE_READ_ROOT_ENV === 'DENIED', 'robot-buddy must not read root-only device.env');

  report.liveChecks.phase2PipDropInAndSandbox = {
    passed: true,
    effectiveSystemdProperties: pipProps,
    systemCallFilterChecks: scfChecks,
    kernelCgroupAndProcStatus: kernelAudit,
    permissionBoundaryChecks: permBoundary,
  };
  log(`Phase 2 PASSED: 1 GiB cgroup memory.max=${kernelAudit.CGROUP_MEMORY_MAX}, pids.max=${kernelAudit.CGROUP_PIDS_MAX}, Seccomp=${kernelAudit.PROC_Seccomp}, NoNewPrivs=${kernelAudit.PROC_NoNewPrivs}`);

  // ---------------------------------------------------------------------------
  // PHASE 3: Identity Enrollment + Full OIDC Claim + Negative Callback + Live Turn
  // ---------------------------------------------------------------------------
  log('Phase 3: Verifying automatic Ed25519 enrollment, OIDC claim (negative + positive), 6-digit confirm, and live turn...');

  let claimUrl = '';
  for (let i = 0; i < 60; i++) {
    const out = dockerExec(PI_CTR, 'runuser -u robot-buddy -- /usr/local/bin/robot-buddy-robot claim-url').stdout.trim();
    if (out.startsWith('https://backend.pip-qa.internal:8443/claim?')) {
      claimUrl = out;
      break;
    }
    await sleep(500);
  }
  assert(claimUrl.startsWith('https://backend.pip-qa.internal:8443/claim?'), `Did not receive live HTTPS claim-url; last=${claimUrl}`);
  const parsedClaimUrl = new URL(claimUrl);
  const botId = parsedClaimUrl.searchParams.get('bot');
  const claimToken = parsedClaimUrl.searchParams.get('token');
  assert(botId && claimToken, `Invalid claim URL parameters`);
  log(`Live enrollment succeeded! bot_id=${botId}`);

  const opPreUnauth = dockerExec(PI_CTR, "curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:8091/").stdout.trim();
  const opPreAuth = dockerExec(PI_CTR, `curl -s -o /dev/null -w '%{http_code}' -u "operator:${OPERATOR_TOKEN}" http://127.0.0.1:8091/`).stdout.trim();
  assert(opPreUnauth === '401', `Expected pre-claim unauth operator panel 401, got ${opPreUnauth}`);
  assert(opPreAuth === '403', `Expected pre-claim auth operator panel 403, got ${opPreAuth}`);

  const oidcFlowScript = `
    const claimUrl = ${JSON.stringify(claimUrl)};
    const extractHidden = (html, name) => {
      const m = html.match(new RegExp('name=' + name + ' value="([^"]+)"'));
      return m ? m[1] : null;
    };
    const extractCookie = (headers, name) => {
      const all = headers.getSetCookie ? headers.getSetCookie() : [headers.get('set-cookie')].filter(Boolean);
      for (const c of all) if (c.startsWith(name + '=')) return c;
      return null;
    };

    // 1. Negative OIDC callback test (error=access_denied)
    const rClaimNeg = await fetch(claimUrl, { redirect: 'manual' });
    if (rClaimNeg.status !== 200) throw new Error('GET /claim neg status ' + rClaimNeg.status);
    const cspHeader = rClaimNeg.headers.get('content-security-policy') || '';
    const refPolicy = rClaimNeg.headers.get('referrer-policy') || '';
    const claimCookieFullNeg = extractCookie(rClaimNeg.headers, 'buddy_claim');
    const claimHtmlNeg = await rClaimNeg.text();
    const formNeg = new URLSearchParams({
      bot: extractHidden(claimHtmlNeg, 'bot'),
      token: extractHidden(claimHtmlNeg, 'token'),
      csrf: extractHidden(claimHtmlNeg, 'csrf'),
    });
    const rLoginNeg = await fetch('https://backend.pip-qa.internal:8443/login', {
      method: 'POST', redirect: 'manual',
      headers: {
        'content-type': 'application/x-www-form-urlencoded',
        'origin': 'https://backend.pip-qa.internal:8443',
        'cookie': claimCookieFullNeg.split(';')[0],
      },
      body: formNeg.toString(),
    });
    const loginCookieNeg = extractCookie(rLoginNeg.headers, 'buddy_login');
    const rAuthNeg = await fetch(rLoginNeg.headers.get('location'), {
      redirect: 'manual',
      headers: { 'x-qa-oidc-error': 'access_denied' },
    });
    const rCbNeg = await fetch(rAuthNeg.headers.get('location'), {
      redirect: 'manual',
      headers: { 'cookie': loginCookieNeg.split(';')[0] },
    });
    const negBody = await rCbNeg.text();
    const negClearCookie = extractCookie(rCbNeg.headers, 'buddy_login') || '';

    // 2. Positive OIDC claim flow
    const rClaim = await fetch(claimUrl, { redirect: 'manual' });
    if (rClaim.status !== 200) throw new Error('GET /claim status ' + rClaim.status);
    const claimCookieFull = extractCookie(rClaim.headers, 'buddy_claim');
    const claimHtml = await rClaim.text();
    const form = new URLSearchParams({
      bot: extractHidden(claimHtml, 'bot'),
      token: extractHidden(claimHtml, 'token'),
      csrf: extractHidden(claimHtml, 'csrf'),
    });
    const rLogin = await fetch('https://backend.pip-qa.internal:8443/login', {
      method: 'POST', redirect: 'manual',
      headers: {
        'content-type': 'application/x-www-form-urlencoded',
        'origin': 'https://backend.pip-qa.internal:8443',
        'cookie': claimCookieFull.split(';')[0],
      },
      body: form.toString(),
    });
    const loginCookieFull = extractCookie(rLogin.headers, 'buddy_login');
    const authorizeUrl = rLogin.headers.get('location');
    const rAuth = await fetch(authorizeUrl, { redirect: 'manual' });
    if (rAuth.status !== 302) throw new Error('GET /authorize status ' + rAuth.status);
    const callbackUrl = rAuth.headers.get('location');
    const rCb = await fetch(callbackUrl, {
      redirect: 'manual',
      headers: { 'cookie': loginCookieFull.split(';')[0] },
    });
    const cbBody = await rCb.text();
    if (rCb.status !== 200) throw new Error('GET /auth/callback status ' + rCb.status);
    const codeMatch = cbBody.match(/<h2>([0-9]{6})<\\/h2>/);
    if (!codeMatch) throw new Error('No 6-digit confirmation in callback page');
    const cbClearCookie = extractCookie(rCb.headers, 'buddy_login') || '';

    console.log(JSON.stringify({
      cspHeader,
      refPolicy,
      claimCookieAttributes: claimCookieFull.split(';').slice(1).map((s) => s.trim()),
      loginCookieAttributes: loginCookieFull.split(';').slice(1).map((s) => s.trim()),
      negativeCallbackStatus: rCbNeg.status,
      negativeCallbackEchoedError: negBody.includes('access_denied'),
      negativeCallbackClearedCookie: negClearCookie.includes('Max-Age=0'),
      authorizeHost: new URL(authorizeUrl).host,
      confirmCode: codeMatch[1],
      callbackClearedCookie: cbClearCookie.includes('Max-Age=0'),
    }));
  `;
  const oidcFlow = JSON.parse(
    run('docker', [
      'exec', '-e', 'NODE_EXTRA_CA_CERTS=/tmp/qa-backend-secrets/tls/ca-bundle.pem',
      BACKEND_CTR, 'bun', '-e', oidcFlowScript,
    ]).stdout.trim(),
  );
  assert(oidcFlow.cspHeader.includes("form-action 'self' https:"), `Unexpected CSP header: ${oidcFlow.cspHeader}`);
  assert(oidcFlow.negativeCallbackStatus === 400, `Expected 400 on OIDC error callback, got ${oidcFlow.negativeCallbackStatus}`);
  assert(!oidcFlow.negativeCallbackEchoedError, 'OIDC error callback echoed untrusted error param');
  assert(oidcFlow.negativeCallbackClearedCookie, 'OIDC error callback did not clear buddy_login');
  assert(oidcFlow.confirmCode.length === 6, `Invalid 6-digit code`);
  assert(oidcFlow.callbackClearedCookie, 'Successful OIDC callback did not clear buddy_login');
  log(`OIDC callback succeeded with a 6-digit confirmation; CSP=${oidcFlow.cspHeader}`);

  // Confirm matching 6-digit code on robot's loopback panel (127.0.0.1:8090)
  let panelCsrf = '';
  for (let i = 0; i < 40; i++) {
    const html = dockerExec(PI_CTR, 'curl -fsS http://127.0.0.1:8090/').stdout;
    if (html.includes(`<p>${oidcFlow.confirmCode}</p>`)) {
      const m = html.match(/name=csrf value="([^"]+)"/);
      if (m) { panelCsrf = m[1]; break; }
    }
    await sleep(500);
  }
  assert(panelCsrf.length > 0, 'Loopback panel did not show matching confirmation code');
  delete oidcFlow.confirmCode;
  oidcFlow.confirmationMatched = true;

  const confirmStatus = dockerExec(PI_CTR, `
    curl -s -o /dev/null -w '%{http_code}' -X POST \
      -H 'Origin: http://127.0.0.1:8090' \
      -H 'Content-Type: application/x-www-form-urlencoded' \
      --data-urlencode "csrf=${panelCsrf}" \
      http://127.0.0.1:8090/confirm
  `).stdout.trim();
  assert(confirmStatus === '303' || confirmStatus === '302', `POST /confirm returned ${confirmStatus}`);

  let panelReady = false;
  for (let i = 0; i < 40; i++) {
    if (dockerExec(PI_CTR, 'curl -fsS http://127.0.0.1:8090/').stdout.includes('Ready')) {
      panelReady = true;
      break;
    }
    await sleep(500);
  }
  assert(panelReady, 'Robot loopback panel did not transition to Ready after confirm');

  const claimUrlAfterOwned = dockerExec(PI_CTR, 'runuser -u robot-buddy -- /usr/local/bin/robot-buddy-robot claim-url').stdout.trim();
  assert(claimUrlAfterOwned.includes('No active claim URL'), `Expected owned claim-url message, got: ${claimUrlAfterOwned}`);
  const ownedJournalCutoff = dockerExec(PI_CTR, 'date -u "+%Y-%m-%d %H:%M:%S UTC"').stdout.trim();

  const opPostUnauth = dockerExec(PI_CTR, "curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:8091/").stdout.trim();
  const opPostAuth = dockerExec(PI_CTR, `curl -s -o /dev/null -w '%{http_code}' -u "operator:${OPERATOR_TOKEN}" http://127.0.0.1:8091/`).stdout.trim();
  assert(opPostUnauth === '401', `Expected post-claim unauth 401, got ${opPostUnauth}`);
  assert(opPostAuth === '200', `Expected post-claim auth 200, got ${opPostAuth}`);

  const sendTurnAndAwaitCursor = async (utterance, minCursor) => {
    dockerExec(PI_CTR, `
      runuser -u speech-qa -- sh -c 'printf "%s\\n" ${JSON.stringify(utterance)} | socat - UNIX-CONNECT:/run/robot-buddy/control.sock'
    `);
    for (let i = 0; i < 40; i++) {
      const raw = dockerExec(PI_CTR, 'cat /var/lib/robot-buddy/cache.json 2>/dev/null || echo "{}"').stdout.trim();
      try {
        const parsed = JSON.parse(raw);
        if (typeof parsed.cursor === 'number' && parsed.cursor > minCursor) return parsed;
      } catch {}
      await sleep(500);
    }
    throw new Error(`Cache cursor did not advance past ${minCursor} after utterance`);
  };

  const waitForOwnedOperator = async () => {
    for (let i = 0; i < 40; i++) {
      const code = dockerExec(PI_CTR, `curl -s -o /dev/null -w '%{http_code}' -u "operator:${OPERATOR_TOKEN}" http://127.0.0.1:8091/`).stdout.trim();
      if (code === '200') return;
      await sleep(500);
    }
    throw new Error('Robot did not retain owned status (operator panel != 200)');
  };

  const cacheTurn1 = await sendTurnAndAwaitCursor(UTTERANCE_INITIAL_CANDIDATE, 0);
  const backendAfterClaim = JSON.parse(run('docker', ['exec', BACKEND_CTR, 'curl', '-fsS', 'http://127.0.0.1:19999/status']).stdout);
  assert(backendAfterClaim.oidc.discoveries >= 1, 'OIDC discovery count < 1');
  assert(backendAfterClaim.oidc.authorizes === 1, `Expected 1 OIDC authorize, got ${backendAfterClaim.oidc.authorizes}`);
  assert(backendAfterClaim.oidc.deniedAuthorizes === 1, `Expected 1 denied OIDC authorize, got ${backendAfterClaim.oidc.deniedAuthorizes}`);
  assert(backendAfterClaim.oidc.tokens === 1, `Expected 1 OIDC token exchange`);
  assert(backendAfterClaim.oidc.userinfos === 1, `Expected 1 OIDC userinfo fetch`);
  assert(backendAfterClaim.oidc.jwksFetches >= 1, `Expected >=1 OIDC JWKS fetch`);

  report.liveChecks.phase3EnrollmentAndOidc = {
    passed: true,
    deviceId: provisionedIdentity.certificate.device_id,
    botId,
    oidcIdentity: {
      issuer: backendAfterClaim.oidc.issuer,
      clientId: backendAfterClaim.oidc.clientId,
      subject: backendAfterClaim.oidc.subject,
      expectedOwner: backendAfterClaim.oidc.expectedOwner,
      auditCounts: backendAfterClaim.oidc,
    },
    oidcHttpFlow: oidcFlow,
    claimUrlAfterOwned,
    operatorPanelGates: {
      preClaimUnauth: opPreUnauth,
      preClaimAuth: opPreAuth,
      postClaimUnauth: opPostUnauth,
      postClaimAuth: opPostAuth,
    },
    initialCandidateCacheCursor: cacheTurn1.cursor,
    modelCallsAfterTurn1: backendAfterClaim.model.calls,
  };
  log(`Phase 3 PASSED: OIDC owner=${backendAfterClaim.oidc.expectedOwner}, cursor=${cacheTurn1.cursor}`);

  // ---------------------------------------------------------------------------
  // PHASE 4: Genuine Cross-Version Upgrade & Rollback (Prior b377904 <-> Candidate)
  // ---------------------------------------------------------------------------
  log(`Phase 4: Genuine cross-version upgrade & rollback between real ${priorCommitShort} ELF and candidate ELF...`);

  // Seed a local face gallery sentinel under /var/lib/robot-buddy/faces/ (0700 state dir)
  dockerExec(PI_CTR, `
    install -d -m 0700 -o robot-buddy -g robot-buddy /var/lib/robot-buddy/faces
    printf '{"sentinel":"qa-gallery-v1","enrolled":["alice"]}\\n' > /var/lib/robot-buddy/faces/qa-sentinel.json
    chown robot-buddy:robot-buddy /var/lib/robot-buddy/faces/qa-sentinel.json
    chmod 0600 /var/lib/robot-buddy/faces/qa-sentinel.json
  `);

  const captureStateSnapshot = () => {
    const raw = dockerExec(PI_CTR, `
      echo "BINARY_SHA256=$(sha256sum /usr/local/bin/robot-buddy-robot | awk '{print $1}')"
      echo "BINARY_BUILD_ID=$(file /usr/local/bin/robot-buddy-robot | sed -n 's/.*BuildID\\[sha1\\]=\\([0-9a-f]*\\).*/\\1/p')"
      echo "IDENTITY_SHA256=$(sha256sum /etc/robot-buddy/device.identity.json | awk '{print $1}')"
      echo "IDENTITY_STAT=$(stat -c '%U:%G %a' /etc/robot-buddy/device.identity.json)"
      echo "ENV_SHA256=$(sha256sum /etc/robot-buddy/device.env | awk '{print $1}')"
      echo "ENV_STAT=$(stat -c '%U:%G %a' /etc/robot-buddy/device.env)"
      echo "SETUP_WIFI_SHA256=$(sha256sum /var/lib/robot-buddy/setup-wifi.json | awk '{print $1}')"
      echo "SETUP_WIFI_STAT=$(stat -c '%U:%G %a' /var/lib/robot-buddy/setup-wifi.json)"
      echo "GALLERY_SHA256=$(sha256sum /var/lib/robot-buddy/faces/qa-sentinel.json | awk '{print $1}')"
      echo "GALLERY_STAT=$(stat -c '%U:%G %a' /var/lib/robot-buddy/faces/qa-sentinel.json)"
      echo "POLKIT_FILES=$(ls /etc/polkit-1/rules.d/*robot-buddy* 2>/dev/null | tr '\\n' ' ')"
      echo "CACHE_JSON=$(tr -d '\\n' < /var/lib/robot-buddy/cache.json)"
      echo "CLAIM_URL_OUTPUT=$(runuser -u robot-buddy -- /usr/local/bin/robot-buddy-robot claim-url)"
      echo "MAIN_PID=$(systemctl show robot-buddy -p MainPID --value)"
      echo "ACTIVE_STATE=$(systemctl show robot-buddy -p ActiveState --value)/$(systemctl show robot-buddy -p SubState --value)"
      echo "MEMORY_MAX=$(systemctl show robot-buddy -p MemoryMax --value)"
      PID=$(systemctl show robot-buddy -p MainPID --value)
      CG=$(awk -F: '{print $3}' "/proc/$PID/cgroup")
      echo "CGROUP_MEMORY_MAX=$(cat "/sys/fs/cgroup$CG/memory.max")"
    `).stdout.trim();
    const s = parseProps(raw);
    const cache = JSON.parse(s.CACHE_JSON);
    delete s.CACHE_JSON;
    s.cacheBotId = cache.bot_id;
    s.cacheCursor = cache.cursor;
    s.cacheClaimUrl = cache.claim_url;
    return s;
  };

  const assertStatePreserved = (baseline, current, label) => {
    assert(current.IDENTITY_SHA256 === baseline.IDENTITY_SHA256, `${label}: identity SHA-256 changed`);
    assert(current.IDENTITY_STAT === 'robot-buddy:robot-buddy 600', `${label}: identity stat changed: ${current.IDENTITY_STAT}`);
    assert(current.ENV_SHA256 === baseline.ENV_SHA256, `${label}: device.env SHA-256 changed`);
    assert(current.ENV_STAT === 'root:root 600', `${label}: device.env stat changed: ${current.ENV_STAT}`);
    assert(current.SETUP_WIFI_SHA256 === baseline.SETUP_WIFI_SHA256, `${label}: setup-wifi.json SHA-256 changed`);
    assert(current.SETUP_WIFI_STAT === 'robot-buddy:robot-buddy 600', `${label}: setup-wifi.json stat changed`);
    assert(current.GALLERY_SHA256 === baseline.GALLERY_SHA256, `${label}: gallery sentinel SHA-256 changed`);
    assert(current.cacheBotId === baseline.cacheBotId, `${label}: bot_id changed`);
    assert(current.cacheClaimUrl === '', `${label}: unexpected active claim_url after already owned`);
    assert(current.CLAIM_URL_OUTPUT.includes('No active claim URL'), `${label}: claim-url did not report owned`);
    assert(current.CGROUP_MEMORY_MAX === '1073741824', `${label}: 1 GiB cgroup memory.max lost`);
  };

  const snapInitialCandidate = captureStateSnapshot();
  report.stateSnapshots.initialCandidate = snapInitialCandidate;

  // Step 4A: Install REAL prior revision (b377904) + its unit + legacy 50-robot-buddy-nm.rules
  // (exercises backward cache compatibility from candidate -> prior AND sets up genuine prior -> candidate upgrade!)
  log(`Step 4A: Switching to real prior revision (${priorCommitShort}) binary + unit + 50-robot-buddy-nm.rules...`);
  dockerExec(PI_CTR, `
    systemctl stop robot-buddy
    install -m 0755 /opt/qa-staging-prior/robot-buddy-robot /usr/local/bin/robot-buddy-robot
    install -m 0644 /opt/qa-staging-prior/infra/robot-buddy.service /etc/systemd/system/robot-buddy.service
    install -D -m 0644 /opt/qa-staging-prior/infra/robot-buddy-pip.conf /etc/systemd/system/robot-buddy.service.d/20-pip.conf
    rm -f /etc/polkit-1/rules.d/40-robot-buddy-nm.rules
    install -m 0644 /opt/qa-staging-prior/infra/robot-buddy-nm.rules /etc/polkit-1/rules.d/50-robot-buddy-nm.rules
    ldd /usr/local/bin/robot-buddy-robot | grep -q 'not found' && exit 1 || true
    systemctl daemon-reload
    systemctl start robot-buddy
  `);
  waitForActiveService();
  const verifyPriorUnit = dockerExec(PI_CTR, 'systemd-analyze verify /etc/systemd/system/robot-buddy.service 2>&1', { allowFail: true });
  await waitForOwnedOperator();
  await sendTurnAndAwaitCursor(UTTERANCE_PRIOR_ROLLBACK_1, snapInitialCandidate.cacheCursor);
  const snapPriorBaseline = captureStateSnapshot();
  report.stateSnapshots.priorBaseline = snapPriorBaseline;
  assert(snapPriorBaseline.BINARY_SHA256 === priorElf.sha256, 'Prior baseline SHA-256 mismatch');
  assert(snapPriorBaseline.BINARY_BUILD_ID === priorElf.buildId, 'Prior baseline BuildID mismatch');
  assert(snapPriorBaseline.POLKIT_FILES.trim() === '/etc/polkit-1/rules.d/50-robot-buddy-nm.rules', `Unexpected prior polkit files: ${snapPriorBaseline.POLKIT_FILES}`);
  assertStatePreserved(snapInitialCandidate, snapPriorBaseline, 'Step 4A (prior baseline)');
  assert(snapPriorBaseline.cacheCursor > snapInitialCandidate.cacheCursor, 'Cursor did not advance on prior binary');
  log(`Step 4A PASSED: real ${priorCommitShort} ELF active at 1 GiB, owned, cursor=${snapPriorBaseline.cacheCursor}`);

  // Step 4B: Genuine UPGRADE (Prior b377904 -> Candidate) following DEPLOYMENT.md §5 verbatim
  log(`Step 4B: Upgrading real ${priorCommitShort} -> candidate (backing up prior binary/config, migrating 50->40 polkit + hardened unit)...`);
  dockerExec(PI_CTR, `
    install -d -m 0700 -o root -g root /var/backups/robot-buddy-prior
    cp -a /usr/local/bin/robot-buddy-robot /var/backups/robot-buddy-prior/robot-buddy-robot
    cp -a /etc/robot-buddy/device.env /var/backups/robot-buddy-prior/device.env
    cp -a /etc/systemd/system/robot-buddy.service /var/backups/robot-buddy-prior/robot-buddy.service
    cp -a /etc/systemd/system/robot-buddy.service.d/20-pip.conf /var/backups/robot-buddy-prior/20-pip.conf
    cp -a /etc/polkit-1/rules.d/50-robot-buddy-nm.rules /var/backups/robot-buddy-prior/50-robot-buddy-nm.rules

    systemctl stop robot-buddy
    install -m 0755 /opt/qa-staging-candidate/robot-buddy-robot /usr/local/bin/robot-buddy-robot
    install -m 0644 /opt/qa-staging-candidate/infra/robot-buddy.service /etc/systemd/system/robot-buddy.service
    install -D -m 0644 /opt/qa-staging-candidate/infra/robot-buddy-pip.conf /etc/systemd/system/robot-buddy.service.d/20-pip.conf
    install -m 0644 /opt/qa-staging-candidate/infra/robot-buddy-nm.rules /etc/polkit-1/rules.d/40-robot-buddy-nm.rules
    rm -f /etc/polkit-1/rules.d/50-robot-buddy-nm.rules
    ldd /usr/local/bin/robot-buddy-robot | grep -q 'not found' && exit 1 || true
    systemd-analyze verify /etc/systemd/system/robot-buddy.service
    systemctl daemon-reload
    systemctl start robot-buddy
  `);
  waitForActiveService();
  await waitForOwnedOperator();
  await sendTurnAndAwaitCursor(UTTERANCE_UPGRADED_CANDIDATE, snapPriorBaseline.cacheCursor);
  const snapAfterUpgrade = captureStateSnapshot();
  report.stateSnapshots.afterUpgradeToCandidate = snapAfterUpgrade;
  assert(snapAfterUpgrade.BINARY_SHA256 === candidateElf.sha256, 'Upgraded candidate SHA-256 mismatch');
  assert(snapAfterUpgrade.BINARY_BUILD_ID === candidateElf.buildId, 'Upgraded candidate BuildID mismatch');
  assert(snapAfterUpgrade.POLKIT_FILES.trim() === '/etc/polkit-1/rules.d/40-robot-buddy-nm.rules', `Unexpected upgraded polkit files: ${snapAfterUpgrade.POLKIT_FILES}`);
  assertStatePreserved(snapPriorBaseline, snapAfterUpgrade, 'Step 4B (upgrade to candidate)');
  assert(snapAfterUpgrade.cacheCursor > snapPriorBaseline.cacheCursor, 'Cursor did not advance after upgrade to candidate');
  log(`Step 4B PASSED: upgraded to candidate ELF, 50->40 polkit migrated, owned, cursor=${snapAfterUpgrade.cacheCursor}`);

  // Step 4C: Genuine ROLLBACK (Candidate -> Saved Prior b377904 Backup) following DEPLOYMENT.md §5 verbatim
  log(`Step 4C: Rolling back candidate -> saved ${priorCommitShort} backup from /var/backups/robot-buddy-prior/...`);
  dockerExec(PI_CTR, `
    systemctl stop robot-buddy
    install -m 0755 /var/backups/robot-buddy-prior/robot-buddy-robot /usr/local/bin/robot-buddy-robot
    install -m 0600 -o root -g root /var/backups/robot-buddy-prior/device.env /etc/robot-buddy/device.env
    install -m 0644 /var/backups/robot-buddy-prior/robot-buddy.service /etc/systemd/system/robot-buddy.service
    install -D -m 0644 /var/backups/robot-buddy-prior/20-pip.conf /etc/systemd/system/robot-buddy.service.d/20-pip.conf
    rm -f /etc/polkit-1/rules.d/40-robot-buddy-nm.rules
    install -m 0644 /var/backups/robot-buddy-prior/50-robot-buddy-nm.rules /etc/polkit-1/rules.d/50-robot-buddy-nm.rules
    ldd /usr/local/bin/robot-buddy-robot | grep -q 'not found' && exit 1 || true
    systemctl daemon-reload
    systemctl start robot-buddy
  `);
  waitForActiveService();
  await waitForOwnedOperator();
  await sendTurnAndAwaitCursor(UTTERANCE_FINAL_ROLLBACK_PRIOR, snapAfterUpgrade.cacheCursor);
  const snapAfterRollback = captureStateSnapshot();
  report.stateSnapshots.afterRollbackToPrior = snapAfterRollback;
  assert(snapAfterRollback.BINARY_SHA256 === priorElf.sha256, 'Rolled-back prior SHA-256 mismatch');
  assert(snapAfterRollback.BINARY_BUILD_ID === priorElf.buildId, 'Rolled-back prior BuildID mismatch');
  assert(snapAfterRollback.POLKIT_FILES.trim() === '/etc/polkit-1/rules.d/50-robot-buddy-nm.rules', `Unexpected rolled-back polkit files: ${snapAfterRollback.POLKIT_FILES}`);
  assertStatePreserved(snapPriorBaseline, snapAfterRollback, 'Step 4C (rollback to prior)');
  assert(snapAfterRollback.cacheCursor > snapAfterUpgrade.cacheCursor, 'Cursor did not advance after rollback to prior');
  log(`Step 4C PASSED: rolled back to real ${priorCommitShort} ELF, state & ownership preserved, cursor=${snapAfterRollback.cacheCursor}`);

  // Step 4D: Re-apply candidate so Phase 5 watchdog & journal verification measures the candidate binary at 1 GiB
  log('Step 4D: Restoring candidate binary & hardened unit for Phase 5 watchdog & journal verification...');
  dockerExec(PI_CTR, `
    systemctl stop robot-buddy
    install -m 0755 /opt/qa-staging-candidate/robot-buddy-robot /usr/local/bin/robot-buddy-robot
    install -m 0644 /opt/qa-staging-candidate/infra/robot-buddy.service /etc/systemd/system/robot-buddy.service
    install -D -m 0644 /opt/qa-staging-candidate/infra/robot-buddy-pip.conf /etc/systemd/system/robot-buddy.service.d/20-pip.conf
    install -m 0644 /opt/qa-staging-candidate/infra/robot-buddy-nm.rules /etc/polkit-1/rules.d/40-robot-buddy-nm.rules
    rm -f /etc/polkit-1/rules.d/50-robot-buddy-nm.rules
    systemctl daemon-reload
    systemctl start robot-buddy
  `);
  waitForActiveService();
  await waitForOwnedOperator();

  report.liveChecks.phase4UpgradeAndRollback = {
    passed: true,
    priorRevision: priorCommitShort,
    priorSha256: priorElf.sha256,
    priorBuildId: priorElf.buildId,
    priorUnitAnalyzeOutput: verifyPriorUnit.stdout.trim(),
    candidateSha256: candidateElf.sha256,
    candidateBuildId: candidateElf.buildId,
    polkitMigrationVerified: '50-robot-buddy-nm.rules -> 40-robot-buddy-nm.rules -> 50-robot-buddy-nm.rules',
    identityPreserved: true,
    setupWifiPreserved: true,
    envPreserved: true,
    galleryPreserved: true,
    ownershipPreservedAcrossUpgradeAndRollback: true,
    cursors: {
      initialCandidate: snapInitialCandidate.cacheCursor,
      priorBaseline: snapPriorBaseline.cacheCursor,
      afterUpgradeToCandidate: snapAfterUpgrade.cacheCursor,
      afterRollbackToPrior: snapAfterRollback.cacheCursor,
    },
  };

  // ---------------------------------------------------------------------------
  // PHASE 5: Watchdog Heartbeat & Journal Redaction Verification (Candidate @ 1 GiB)
  // ---------------------------------------------------------------------------
  log(`Phase 5: Waiting ${WATCHDOG_WAIT_SECS}s to verify live systemd WATCHDOG=1 heartbeat and journal redaction on candidate...`);
  if (WATCHDOG_WAIT_SECS > 0) await sleep(WATCHDOG_WAIT_SECS * 1000);

  const wdProps = parseProps(
    dockerExec(PI_CTR, `
      systemctl show robot-buddy \
        -p ActiveState -p SubState -p NRestarts -p Result \
        -p WatchdogUSec -p WatchdogTimestamp -p WatchdogTimestampMonotonic \
        -p MemoryMax -p MemoryCurrent -p MemoryPeak -p TasksCurrent
    `).stdout.trim(),
  );
  assert(wdProps.ActiveState === 'active' && wdProps.SubState === 'running', `Service failed during watchdog window: ${JSON.stringify(wdProps)}`);
  assert(wdProps.NRestarts === '0', `Unexpected service restarts: ${wdProps.NRestarts}`);
  if (WATCHDOG_WAIT_SECS >= 32) {
    assert(Number(wdProps.WatchdogTimestampMonotonic) > 0, `No WATCHDOG=1 ping received within ${WATCHDOG_WAIT_SECS}s`);
  }

  const journalText = dockerExec(PI_CTR, 'journalctl -u robot-buddy --no-pager').stdout;
  const setupWifiJson = JSON.parse(dockerExec(PI_CTR, 'cat /var/lib/robot-buddy/setup-wifi.json').stdout);
  const finalBackend = JSON.parse(run('docker', ['exec', BACKEND_CTR, 'curl', '-fsS', 'http://127.0.0.1:19999/status']).stdout);
  const backendLogs = JSON.parse(run('docker', ['exec', BACKEND_CTR, 'curl', '-fsS', 'http://127.0.0.1:19999/logs']).stdout);
  const backendLogsText = backendLogs.lines.join('\n');

  // Device journal redaction: must never contain private keys, operator token, setup Wi-Fi
  // password, or conversational utterances (compact journal mode). Note: per
  // apps/robot/src/journal.rs (`claim()`), the onboarding `claim url=...` line is emitted
  // intentionally once for headless setup (hence DEPLOYMENT.md's warning to treat device
  // journals as private), and must not repeat after ownership.
  const forbiddenDeviceJournalSecrets = [
    { label: 'device_private_key', value: provisionedIdentity.private_key },
    { label: 'operator_token', value: OPERATOR_TOKEN },
    { label: 'setup_wifi_password', value: setupWifiJson.password },
    { label: 'utterance_initial_candidate', value: UTTERANCE_INITIAL_CANDIDATE },
    { label: 'utterance_prior_rollback_1', value: UTTERANCE_PRIOR_ROLLBACK_1 },
    { label: 'utterance_upgraded_candidate', value: UTTERANCE_UPGRADED_CANDIDATE },
    { label: 'utterance_final_rollback_prior', value: UTTERANCE_FINAL_ROLLBACK_PRIOR },
  ];
  const deviceJournalLeaks = forbiddenDeviceJournalSecrets
    .filter((s) => s.value && journalText.includes(s.value))
    .map((s) => s.label);
  assert(deviceJournalLeaks.length === 0, `Secrets or utterances leaked into device journald: ${deviceJournalLeaks.join(', ')}`);

  const claimLinesTotal = journalText
    .split('\n')
    .filter((l) => l.includes('claim url='));
  const postOwnedJournal = dockerExec(
    PI_CTR,
    `journalctl -u robot-buddy --since ${JSON.stringify(ownedJournalCutoff)} --no-pager`,
  ).stdout;
  const claimLinesAfterOwned = postOwnedJournal
    .split('\n')
    .filter((l) => l.includes('claim url='));
  assert(claimLinesTotal.length >= 1 && claimLinesTotal.length <= 2, `Expected 1-2 pre-claim onboarding claim url lines (base start + 20-pip restart), saw ${claimLinesTotal.length}`);
  assert(claimLinesAfterOwned.length === 0, `Unexpected claim url emitted after ownership (including upgrade/rollback): ${claimLinesAfterOwned.length}`);

  // Backend server log redaction: must never log claim token, OIDC secret, or user utterances.
  const forbiddenBackendLogSecrets = [
    { label: 'claim_token', value: claimToken },
    { label: 'oidc_client_secret', value: 'pip-deploy-qa-oidc-secret-not-real-0123456789' },
    { label: 'utterance_initial_candidate', value: UTTERANCE_INITIAL_CANDIDATE },
    { label: 'utterance_prior_rollback_1', value: UTTERANCE_PRIOR_ROLLBACK_1 },
    { label: 'utterance_upgraded_candidate', value: UTTERANCE_UPGRADED_CANDIDATE },
    { label: 'utterance_final_rollback_prior', value: UTTERANCE_FINAL_ROLLBACK_PRIOR },
  ];
  const backendLogLeaks = forbiddenBackendLogSecrets
    .filter((s) => s.value && backendLogsText.includes(s.value))
    .map((s) => s.label);
  assert(backendLogLeaks.length === 0, `Secrets or utterances leaked into backend server logs: ${backendLogLeaks.join(', ')}`);

  report.liveChecks.phase5WatchdogAndRedaction = {
    passed: true,
    watchdogWaitSecs: WATCHDOG_WAIT_SECS,
    watchdogProperties: wdProps,
    journalLineCount: journalText.split('\n').filter(Boolean).length,
    deviceJournalRedactionLeaks: deviceJournalLeaks,
    deviceJournalPreClaimLines: claimLinesTotal.length,
    deviceJournalPostOwnedClaimLines: claimLinesAfterOwned.length,
    backendLogRedactionLeaks: backendLogLeaks,
    finalBackendStatus: finalBackend,
  };

  // Redact the single onboarding claim token from the saved journal tail summary before writing JSON.
  const sanitizedJournalTail = journalText
    .split('\n')
    .filter(Boolean)
    .slice(-30)
    .map((l) => l.replace(claimToken, '<REDACTED_CLAIM_TOKEN>'));

  report.logsSummary = {
    journalTail: sanitizedJournalTail,
    backendLogTail: backendLogs.lines.slice(-25),
  };

  if (!report.syntaxAndStaticChecks.rpicamAppsLiteOnStockDebianTrixie) {
    report.docFindings.push(
      'Stock Debian Trixie ARM64 (deb.debian.org) does not ship `rpicam-apps-lite` (Raspberry Pi OS archive package); DEPLOYMENT.md §3 correctly scopes this to Raspberry Pi OS Lite.',
    );
  }
  const depMd = fs.readFileSync(path.join(REPO_ROOT, 'DEPLOYMENT.md'), 'utf8');
  if (!depMd.includes('40-robot-buddy-nm.rules')) {
    report.docFindings.push('DEPLOYMENT.md does not reference `40-robot-buddy-nm.rules`.');
  }

  report.status = 'PASS';
  report.finishedAt = new Date().toISOString();
  log('ALL PHASES PASSED!');
} catch (err) {
  report.status = 'FAIL';
  report.finishedAt = new Date().toISOString();
  report.error = String(err?.stack || err);
  console.error(`[pip-deploy-qa] FAILED: ${report.error}`);
  try {
    report.logsSummary.journalTail = run('docker', ['exec', PI_CTR, 'journalctl', '-u', 'robot-buddy', '--no-pager', '-n', '40'], { allowFail: true }).stdout.split('\n');
    report.logsSummary.backendLogTail = run('docker', ['logs', '--tail', '40', BACKEND_CTR], { allowFail: true }).stderr.split('\n');
  } catch {}
} finally {
  const reportPath = path.join(ARTIFACTS_DIR, `rehearsal-${RUN_ID}.json`);
  const latestPath = path.join(ARTIFACTS_DIR, 'latest.json');
  fs.writeFileSync(reportPath, JSON.stringify(report, null, 2));
  fs.writeFileSync(latestPath, JSON.stringify(report, null, 2));
  log(`Report written to ${path.relative(REPO_ROOT, reportPath)}`);

  if (!KEEP_CONTAINERS) {
    log('Cleaning up disposable containers and network...');
    run('docker', ['rm', '-f', PI_CTR, BACKEND_CTR], { allowFail: true });
    run('docker', ['network', 'rm', NET_NAME], { allowFail: true });
  } else {
    log(`Keeping containers (--keep): ${PI_CTR}, ${BACKEND_CTR} on ${NET_NAME}`);
  }
}

process.exit(report.status === 'PASS' ? 0 : 1);

//! Shared integration harness: one isolated Rivet engine, one registry and one mock model
//! per test binary.
//!
//! The harness owns the engine process itself instead of letting rivetkit spawn or reuse
//! one, so every run gets fresh storage, an engine it knows it started, and a cleanup path
//! that survives panics and SIGKILL:
//!
//! - storage lives in a per-run directory `<state>/run-<pid>-<millis>`;
//! - a watchdog shell in its own process group holds a pipe from this process and, when the
//!   pipe closes (normal exit, panic or SIGKILL), stops the engine and removes the run dir;
//! - at start, engines left by dead runs of this suite (matched by storage path) are killed
//!   and reported, and any other listener on our ports fails the run with its PID.
//!
//! Background tasks (engine I/O, the registry, the mock model) run on a runtime owned by the
//! harness, because each `#[tokio::test]` runtime is dropped when its test returns.

use anyhow::{Context, Result, bail, ensure};
use axum::response::{IntoResponse, Response};
use rivetkit::{
    EngineSpawnMode, ServeConfig, TypedActorConnection, TypedActorHandle, TypedClientExt,
    client::{Client, ClientConfig, GetOrCreateOptions},
};
use robot_buddy::{
    auth::{self, Session},
    protocol::{Call, Command, OutputEvent},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fmt::Write as _,
    io::Write as _,
    os::unix::process::CommandExt as _,
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use tokio_util::sync::CancellationToken;

pub type BotActor = TypedActorHandle<robot_buddy::Bot>;
pub type ConversationActor = TypedActorHandle<robot_buddy::conversation::Conversation>;
pub type MemoryActor = TypedActorHandle<robot_buddy::memory::Memory>;

/// rivetkit's local engine admin token.
const ENGINE_TOKEN: &str = "default";
const NAMESPACE: &str = "default";
const POOL: &str = "robot-buddy-test";
const ENGINE_VERSION: &str = "3.1.1";
/// Storage of the suite before it owned its engine. Engines there are ours too.
const LEGACY_STORAGE: &str = "/tmp/robot-buddy-integration";
const READY_TIMEOUT: Duration = Duration::from_secs(90);
/// How long a turn or event wait may take before a test fails.
pub const WAIT: Duration = Duration::from_secs(20);

// ---------------------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------------------

pub struct Harness {
    pub endpoint: String,
    pub engine_port: u16,
    pub model_port: u16,
    pub run_dir: PathBuf,
    pub mock: Arc<Mock>,
    runtime: tokio::runtime::Runtime,
    engine: Mutex<Option<Engine>>,
    serving: Mutex<Option<(CancellationToken, tokio::task::JoinHandle<Result<()>>)>>,
    /// Shared by ordinary tests, exclusive for a restart.
    gate: RwLock<()>,
    /// Write end of the watchdog pipe. Never closed while the process lives.
    _watchdog: Mutex<ChildStdin>,
    binary: PathBuf,
    signing_key: String,
}

static HARNESS: tokio::sync::OnceCell<std::result::Result<&'static Harness, String>> =
    tokio::sync::OnceCell::const_new();

/// The process-wide harness, started on first use.
pub async fn harness() -> &'static Harness {
    let started = HARNESS
        .get_or_init(|| async {
            let (tx, rx) = tokio::sync::oneshot::channel();
            std::thread::spawn(move || {
                let _ = tx.send(Harness::start().map_err(|e| format!("{e:#}")));
            });
            match rx.await {
                Ok(Ok(h)) => Ok(&*Box::leak(Box::new(h))),
                Ok(Err(e)) => Err(e),
                Err(_) => Err("harness start thread panicked".into()),
            }
        })
        .await;
    match started {
        Ok(h) => h,
        Err(e) => panic!("integration harness failed to start: {e}"),
    }
}

fn env_port(name: &str, default: u16) -> Result<u16> {
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => v
            .parse()
            .with_context(|| format!("{name}={v} is not a port")),
        _ => Ok(default),
    }
}

/// `BUDDY_TEST_STATE_DIR`, else `<tmp>/robot-buddy-it`.
fn state_dir() -> PathBuf {
    std::env::var_os("BUDDY_TEST_STATE_DIR").map_or_else(
        || std::env::temp_dir().join("robot-buddy-it"),
        PathBuf::from,
    )
}

fn millis_since_epoch() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

impl Harness {
    fn start() -> Result<Self> {
        let engine_port = env_port("BUDDY_TEST_ENGINE_PORT", 16420)?;
        let model_port = env_port("BUDDY_TEST_MODEL_PORT", 19091)?;
        ensure!(
            engine_port <= u16::MAX - 10,
            "BUDDY_TEST_ENGINE_PORT too large"
        );
        let expected_model = format!("http://127.0.0.1:{model_port}/v1");
        let configured = std::env::var("AI_BASE_URL").unwrap_or_default();
        ensure!(
            configured == expected_model,
            "AI_BASE_URL must be {expected_model} (got {configured:?}); run `bun run test:integration`"
        );
        let signing_key = std::env::var("BUDDY_SIGNING_KEY").unwrap_or_default();
        ensure!(
            signing_key.len() >= 32,
            "BUDDY_SIGNING_KEY (test-only, >=32 bytes) is required; run `bun run test:integration`"
        );

        let state = state_dir();
        std::fs::create_dir_all(&state).with_context(|| format!("create {}", state.display()))?;
        for line in reap_stale_engines(&state) {
            eprintln!("integration harness: {line}");
        }
        remove_dead_run_dirs(&state);
        let mut ports = vec![engine_port, engine_port + 1, engine_port + 10, model_port];
        ports.dedup();
        ensure_ports_free(&ports)?;

        let run_dir = state.join(format!(
            "run-{}-{}",
            std::process::id(),
            millis_since_epoch()
        ));
        std::fs::create_dir_all(&run_dir)?;
        let watchdog = spawn_watchdog(&run_dir)?;

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .thread_name("it-harness")
            .build()?;
        let mock = Arc::new(Mock::default());
        let listener =
            runtime.block_on(tokio::net::TcpListener::bind(("127.0.0.1", model_port)))?;
        runtime.spawn(serve_mock(listener, mock.clone()));

        let binary = runtime.block_on(resolve_engine_binary())?;
        let harness = Self {
            endpoint: format!("http://127.0.0.1:{engine_port}"),
            engine_port,
            model_port,
            run_dir,
            mock,
            runtime,
            engine: Mutex::new(None),
            serving: Mutex::new(None),
            gate: RwLock::new(()),
            _watchdog: Mutex::new(watchdog),
            binary,
            signing_key,
        };
        let started = Instant::now();
        harness.runtime.block_on(harness.bring_up())?;
        eprintln!(
            "integration harness: engine {} on {} ready in {} ms (run dir {})",
            harness.binary.display(),
            harness.endpoint,
            started.elapsed().as_millis(),
            harness.run_dir.display()
        );
        Ok(harness)
    }

    /// Ordinary tests share the engine; a restart waits for them.
    pub async fn shared(&self) -> RwLockReadGuard<'_, ()> {
        self.gate.read().await
    }

    pub async fn exclusive(&self) -> RwLockWriteGuard<'_, ()> {
        self.gate.write().await
    }

    /// A client bound to the calling test's runtime.
    pub fn client(&self) -> Client {
        Client::new(
            ClientConfig::new(&self.endpoint)
                .token(ENGINE_TOKEN)
                .namespace(NAMESPACE)
                .pool_name(POOL)
                .disable_metadata_lookup(true),
        )
    }

    pub fn token(&self, bot: &str, role: &str) -> String {
        auth::sign(
            self.signing_key.as_bytes(),
            "session",
            &Session {
                bot_id: bot.into(),
                role: role.into(),
                expires: auth::now() + 600,
            },
        )
        .expect("sign test session")
    }

    async fn bring_up(&self) -> Result<()> {
        let engine = Engine::spawn(self).await?;
        *self
            .engine
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(engine);
        self.serve(robot_buddy::registry).await
    }

    /// Serves `registry` against the running engine. Every registry the suite serves has
    /// the real `Bot`, which the readiness probe enrolls.
    async fn serve(&self, registry: fn() -> rivetkit::Registry) -> Result<()> {
        let shutdown = CancellationToken::new();
        let config = ServeConfig {
            endpoint: self.endpoint.clone(),
            token: Some(ENGINE_TOKEN.into()),
            namespace: NAMESPACE.into(),
            version: 1,
            engine_port: Some(self.engine_port),
            // The harness owns the engine; rivetkit only connects.
            engine_spawn: EngineSpawnMode::Never,
            engine_auto_download: false,
            start_services: false,
            pool_name: POOL.into(),
            ..Default::default()
        };
        let task = tokio::spawn(registry().serve_with_config(config, shutdown.clone()));
        // rivetkit does not expose envoy readiness, so prove it with a real action.
        let client = self.client();
        let probe = format!("probe-{}", auth::random());
        let admin = self.token(&probe, "admin");
        let deadline = Instant::now() + READY_TIMEOUT;
        let mut last: String;
        loop {
            if task.is_finished() {
                let result = task.await.context("join registry")?;
                bail!("registry exited during startup: {result:?}");
            }
            let handle = handle::<robot_buddy::Bot>(&client, "bot", &probe, &admin)?;
            let call = handle.send(Call {
                session: admin.clone(),
                command: Command::Enroll {
                    device_id: "probe".into(),
                    public_key: "probe".into(),
                },
            });
            match tokio::time::timeout(Duration::from_secs(5), call).await {
                Ok(Ok(_)) => break,
                Ok(Err(e)) => last = format!("{e:#}"),
                Err(_) => last = "probe action timed out".into(),
            }
            ensure!(
                Instant::now() < deadline,
                "registry never served an action: {last}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        *self
            .serving
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((shutdown, task));
        Ok(())
    }

    async fn stop_serving(&self) -> Result<()> {
        let serving = self
            .serving
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some((shutdown, task)) = serving {
            shutdown.cancel();
            tokio::time::timeout(Duration::from_secs(30), task)
                .await
                .context("registry did not stop within 30 s")?
                .context("join registry")?
                .context("registry stopped with an error")?;
        }
        Ok(())
    }

    /// Stops the registry and the engine, then starts both again on the same storage.
    /// Callers hold [`Harness::exclusive`]. Returns how long the restart took.
    pub async fn restart(&'static self) -> Result<Duration> {
        let this = self;
        self.runtime
            .spawn(async move {
                let started = Instant::now();
                this.stop_serving().await?;
                let engine = this
                    .engine
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take();
                if let Some(engine) = engine {
                    engine.stop(this).await?;
                }
                this.bring_up().await?;
                Ok(started.elapsed())
            })
            .await
            .context("join restart")?
    }

    /// Stops the registry and serves `registry` on the same engine and storage. Actors
    /// stop as in a deploy and wake under the new registry with their databases intact.
    /// Callers hold [`Harness::exclusive`] and serve `robot_buddy::registry` again before
    /// releasing it. Returns how long the swap took.
    pub async fn swap_registry(
        &'static self,
        registry: fn() -> rivetkit::Registry,
    ) -> Result<Duration> {
        let this = self;
        self.runtime
            .spawn(async move {
                let started = Instant::now();
                this.stop_serving().await?;
                this.serve(registry).await?;
                Ok(started.elapsed())
            })
            .await
            .context("join registry swap")?
    }

    /// Listeners on the engine ports, for the no-engine-left assertions.
    fn engine_ports(&self) -> [u16; 3] {
        [
            self.engine_port,
            self.engine_port + 1,
            self.engine_port + 10,
        ]
    }
}

// ---------------------------------------------------------------------------------------
// Engine process
// ---------------------------------------------------------------------------------------

struct Engine {
    child: Child,
}

impl Engine {
    async fn spawn(h: &Harness) -> Result<Self> {
        let port = h.engine_port;
        let engine_dir = h.run_dir.join("engine");
        let depot = h.run_dir.join("depot");
        let logs = h.run_dir.join("logs");
        for dir in [
            &engine_dir,
            &depot.join("log"),
            &depot.join("objects"),
            &logs,
        ] {
            std::fs::create_dir_all(dir)?;
        }
        let config = engine_dir.join("config.json");
        let topology = json!({"topology":{"datacenter_label":1,"datacenters":{"default":{
            "datacenter_label":1,"is_leader":true,
            "public_url":format!("http://127.0.0.1:{port}/"),
            "peer_url":format!("http://127.0.0.1:{}", port + 1)}}}});
        std::fs::write(&config, serde_json::to_vec_pretty(&topology)?)?;
        let stamp = millis_since_epoch();
        let stdout = std::fs::File::create(logs.join(format!("engine-{stamp}.out")))?;
        let stderr_path = logs.join(format!("engine-{stamp}.err"));
        let stderr = std::fs::File::create(&stderr_path)?;
        let mut command = std::process::Command::new(&h.binary);
        command
            .arg("--config")
            .arg(&config)
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr);
        // The engine stays local: no proxy, and nothing from the test's model wiring.
        for var in [
            "HTTPS_PROXY",
            "https_proxy",
            "HTTP_PROXY",
            "http_proxy",
            "ALL_PROXY",
            "all_proxy",
        ] {
            command.env_remove(var);
        }
        for (key, value) in engine_env(port, &depot) {
            command.env(key, value);
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("spawn engine {}", h.binary.display()))?;
        std::fs::write(h.run_dir.join("engine.pid"), child.id().to_string())?;
        if let Err(e) = wait_engine_ready(&h.endpoint, &mut child).await {
            let _ = child.kill();
            let _ = child.wait();
            let tail = std::fs::read_to_string(&stderr_path).unwrap_or_default();
            let tail: String = tail.lines().rev().take(20).collect::<Vec<_>>().join("\n");
            bail!(
                "{e:#}\nengine stderr ({}), newest first:\n{tail}",
                stderr_path.display()
            );
        }
        Ok(Self { child })
    }

    /// SIGTERM, wait up to 20 s, then SIGKILL; returns once the ports are closed.
    async fn stop(mut self, h: &Harness) -> Result<()> {
        signal(self.child.id(), "TERM");
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if self.child.try_wait()?.is_some() {
                break;
            }
            if Instant::now() >= deadline {
                eprintln!("integration harness: engine ignored SIGTERM for 20 s; killing");
                self.child.kill()?;
                self.child.wait()?;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let _ = std::fs::remove_file(h.run_dir.join("engine.pid"));
        wait_ports_closed(&h.engine_ports(), Duration::from_secs(10)).await
    }
}

/// The environment rivetkit's own engine manager sets for a local engine.
fn engine_env(port: u16, depot: &Path) -> Vec<(&'static str, String)> {
    vec![
        ("RIVET__AUTH__ADMIN_TOKEN", ENGINE_TOKEN.into()),
        ("RIVET__GUARD__HOST", "127.0.0.1".into()),
        ("RIVET__GUARD__PORT", port.to_string()),
        ("RIVET__API_PEER__HOST", "127.0.0.1".into()),
        ("RIVET__API_PEER__PORT", (port + 1).to_string()),
        ("RIVET__METRICS__HOST", "127.0.0.1".into()),
        ("RIVET__METRICS__PORT", (port + 10).to_string()),
        (
            "RIVET__STORAGE__LOG_DIRECTORY",
            depot.join("log").display().to_string(),
        ),
        (
            "RIVET__OBJECT_STORAGE__PRIMARY__FILE_SYSTEM__PATH",
            depot.join("objects").display().to_string(),
        ),
        ("RIVET__STORAGE__BALANCING__ENABLED", "false".into()),
        ("RIVET__STORAGE__AUTO_PROVISION_PARTITIONS", "1".into()),
        (
            "RIVET__STORAGE__LOG_SHARD__MAX_DISK_BYTES",
            (1024_u64 * 1024 * 1024).to_string(),
        ),
        ("RIVET__STORAGE__COLD_STORE__DISK_CACHE_BYTES", "0".into()),
        ("RIVET__STORAGE__WRITE_TO_COLD_AFTER_MS", "5000".into()),
        ("RIVET__STORAGE__STOP_TIMEOUT_MS", "6000".into()),
        ("RIVET__STORAGE__STOP_TEARDOWN_MS", "2000".into()),
        ("RIVET__PEGBOARD__NODE_LOST_THRESHOLD", "7000".into()),
        ("RIVET__RUNTIME__WORKER_SHUTDOWN_DURATION", "1".into()),
        ("RIVET__RUNTIME__GUARD_SHUTDOWN_DURATION", "1".into()),
        ("RIVET__RUNTIME__FORCE_SHUTDOWN_DURATION", "10".into()),
    ]
}

fn local_http() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()?)
}

/// Health (the listener is up and is a v3 engine), then the readiness probe rivetkit uses.
async fn wait_engine_ready(endpoint: &str, child: &mut Child) -> Result<()> {
    let http = local_http()?;
    let deadline = Instant::now() + READY_TIMEOUT;
    let mut last: String;
    loop {
        if let Some(status) = child.try_wait()? {
            bail!("engine exited during startup with {status}");
        }
        match http.get(format!("{endpoint}/health")).send().await {
            Ok(r) if r.status().is_success() => {
                let body: Value = r.json().await.unwrap_or(Value::Null);
                ensure!(
                    body["runtime"] == "engine",
                    "port answered /health but is not a rivet engine: {body}"
                );
                break;
            }
            Ok(r) => last = format!("health status {}", r.status()),
            Err(e) => last = e.to_string(),
        }
        ensure!(
            Instant::now() < deadline,
            "engine never became healthy: {last}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let probe =
        format!("{endpoint}/actors?namespace={NAMESPACE}&name=rivetkit-readiness-probe&key=probe");
    loop {
        if let Some(status) = child.try_wait()? {
            bail!("engine exited before readiness with {status}");
        }
        match http.get(&probe).bearer_auth(ENGINE_TOKEN).send().await {
            Ok(r) if r.status().is_success() => return Ok(()),
            Ok(r) => last = format!("readiness status {}", r.status()),
            Err(e) => last = e.to_string(),
        }
        ensure!(
            Instant::now() < deadline,
            "engine never became ready: {last}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn artifact_name() -> String {
    let target = match std::env::consts::OS {
        "macos" => format!("{}-apple-darwin", std::env::consts::ARCH),
        "linux" => format!("{}-unknown-linux-musl", std::env::consts::ARCH),
        other => format!("{}-{other}", std::env::consts::ARCH),
    };
    format!("rivet-engine-{target}")
}

/// Explicit path, then any rivetkit cache of the pinned version, then a verified download
/// into `~/.rivetkit` (the cache rivetkit itself uses).
async fn resolve_engine_binary() -> Result<PathBuf> {
    for var in ["RIVET_ENGINE_BINARY_PATH", "BUDDY_ENGINE_BINARY"] {
        if let Some(path) = std::env::var_os(var).filter(|p| !p.is_empty()) {
            let path = PathBuf::from(path);
            ensure!(path.is_file(), "{var}={} does not exist", path.display());
            return Ok(path);
        }
    }
    let relative = Path::new(".rivetkit/engine/v3")
        .join(ENGINE_VERSION)
        .join(artifact_name());
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(p) = std::env::var_os("RIVETKIT_STORAGE_PATH") {
        roots.push(PathBuf::from(p));
    }
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(PathBuf::from(home));
    }
    roots.push(PathBuf::from(LEGACY_STORAGE));
    for root in &roots {
        let candidate = root.join(&relative);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    let destination = roots.first().context("no cache root")?.join(&relative);
    download_engine(&destination).await?;
    Ok(destination)
}

async fn download_engine(destination: &Path) -> Result<()> {
    use sha2::Digest as _;
    let artifact = artifact_name();
    let base = format!("https://releases.rivet.dev/rivet/{ENGINE_VERSION}/engine");
    eprintln!("integration harness: downloading {base}/{artifact}");
    let http = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_mins(5))
        .build()?;
    let sums = http
        .get(format!("{base}/SHA256SUMS"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let expected = sums
        .lines()
        .find_map(|line| {
            let mut parts = line.split_whitespace();
            let sum = parts.next()?;
            (parts.next()?.trim_start_matches('*') == artifact).then(|| sum.to_owned())
        })
        .with_context(|| format!("SHA256SUMS has no {artifact}"))?;
    let bytes = http
        .get(format!("{base}/{artifact}"))
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    let actual: String = sha2::Sha256::digest(&bytes)
        .iter()
        .fold(String::new(), |mut hex, b| {
            let _ = write!(hex, "{b:02x}");
            hex
        });
    ensure!(
        actual.eq_ignore_ascii_case(&expected),
        "engine checksum mismatch"
    );
    let parent = destination.parent().context("cache parent")?;
    std::fs::create_dir_all(parent)?;
    let partial = destination.with_extension("partial");
    std::fs::write(&partial, &bytes)?;
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&partial, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&partial, destination)?;
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Process hygiene
// ---------------------------------------------------------------------------------------

fn signal(pid: u32, signal: &str) {
    let _ = std::process::Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn pid_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Every process as (pid, ppid, command).
fn processes() -> Vec<(u32, u32, String)> {
    let Ok(out) = std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,command="])
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let pid = parts.next()?.parse().ok()?;
            let parent = parts.next()?.parse().ok()?;
            Some((pid, parent, parts.collect::<Vec<_>>().join(" ")))
        })
        .collect()
}

/// The owner PID encoded in a `run-<pid>-<millis>` path segment.
fn run_owner(text: &str) -> Option<u32> {
    let start = text.find("/run-")? + 5;
    text[start..].split('-').next()?.parse().ok()
}

/// Kills engines of dead runs of this suite, found by their storage path.
fn reap_stale_engines(state: &Path) -> Vec<String> {
    let state = state.display().to_string();
    let mut report = Vec::new();
    for (pid, parent, command) in processes() {
        if !command.contains("rivet-engine") {
            continue;
        }
        let ours = command.contains(&state);
        let legacy = command.contains(LEGACY_STORAGE);
        if !ours && !legacy {
            continue;
        }
        let owner_alive = if ours {
            run_owner(&command).is_some_and(pid_alive)
        } else {
            parent != 1
        };
        if owner_alive {
            // A live run of this suite: never touch it; port checks report a clash.
            continue;
        }
        signal(pid, "TERM");
        let deadline = Instant::now() + Duration::from_secs(15);
        while pid_alive(pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        if pid_alive(pid) {
            signal(pid, "KILL");
        }
        report.push(format!(
            "stopped stale engine pid {pid} (parent {parent}) from an earlier run: {command}"
        ));
    }
    report
}

fn remove_dead_run_dirs(state: &Path) {
    let Ok(entries) = std::fs::read_dir(state) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let owner = run_owner(&format!("/{}", entry.file_name().to_string_lossy()));
        if owner.is_some_and(|pid| !pid_alive(pid)) {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

/// PIDs listening on `port`, via lsof when it is installed.
fn listeners(port: u16) -> Option<Vec<u32>> {
    let out = std::process::Command::new("lsof")
        .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-t"])
        .output()
        .ok()?;
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.trim().parse().ok())
            .collect(),
    )
}

fn port_answers(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(200),
    )
    .is_ok()
}

fn describe(pid: u32) -> String {
    processes()
        .into_iter()
        .find(|(p, ..)| *p == pid)
        .map_or_else(
            || format!("pid {pid}"),
            |(p, ppid, c)| format!("pid {p} (parent {ppid}): {c}"),
        )
}

/// Fails with the owner of any listener on our ports.
fn ensure_ports_free(ports: &[u16]) -> Result<()> {
    let mut busy = Vec::new();
    for &port in ports {
        match listeners(port) {
            Some(pids) if !pids.is_empty() => {
                for pid in pids {
                    busy.push(format!("port {port}: {}", describe(pid)));
                }
            }
            _ if port_answers(port) => busy.push(format!("port {port}: in use (owner unknown)")),
            _ => {}
        }
    }
    ensure!(
        busy.is_empty(),
        "integration ports are in use and not by a stale run of this suite:\n  {}\n\
         Stop that process or pick other ports with BUDDY_TEST_ENGINE_PORT / BUDDY_TEST_MODEL_PORT.",
        busy.join("\n  ")
    );
    Ok(())
}

async fn wait_ports_closed(ports: &[u16], within: Duration) -> Result<()> {
    let deadline = Instant::now() + within;
    loop {
        let open: Vec<u16> = ports.iter().copied().filter(|p| port_answers(*p)).collect();
        if open.is_empty() {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "engine ports still open after stop: {open:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Stops the engine and removes the run dir when this process goes away for any reason.
fn spawn_watchdog(run_dir: &Path) -> Result<ChildStdin> {
    const SCRIPT: &str = r#"
cat >/dev/null
pid=$(cat "$1/engine.pid" 2>/dev/null)
if [ -n "$pid" ]; then
  kill -TERM "$pid" 2>/dev/null
  i=0
  while kill -0 "$pid" 2>/dev/null; do
    i=$((i+1))
    if [ "$i" -gt 200 ]; then kill -KILL "$pid" 2>/dev/null; break; fi
    sleep 0.1
  done
fi
rm -rf "$1"
"#;
    let mut child = std::process::Command::new("sh")
        .arg("-c")
        .arg(SCRIPT)
        .arg("robot-buddy-it-watchdog")
        .arg(run_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // Its own group, so a terminal Ctrl-C does not kill it before cleanup.
        .process_group(0)
        .spawn()
        .context("spawn watchdog")?;
    let mut stdin = child.stdin.take().context("watchdog stdin")?;
    stdin.write_all(b"")?;
    // The Child handle is dropped on purpose: the watchdog outlives this process.
    Ok(stdin)
}

// ---------------------------------------------------------------------------------------
// Mock OpenAI-compatible model
// ---------------------------------------------------------------------------------------

/// One chat completion request, classified the way the harness prompts route it.
pub struct ModelRequest {
    pub body: Value,
    /// The first call of a turn, whose content is spoken as it arrives.
    pub direct: bool,
    /// The tool-disabled spoken phase after tool work.
    pub spoken: bool,
    pub tool_results: usize,
}

pub type Script = Arc<dyn Fn(&ModelRequest) -> Response + Send + Sync>;

/// Replies keyed by a tag in the newest user message. Every test uses its own tags.
#[derive(Default)]
pub struct Mock {
    scripts: Mutex<Vec<(String, Script)>>,
    requests: Mutex<BTreeMap<String, Vec<Value>>>,
    /// CONNECT requests seen, i.e. outbound HTTPS attempts routed through this mock.
    pub connects: AtomicUsize,
}

impl Mock {
    pub fn script(
        &self,
        tag: &str,
        script: impl Fn(&ModelRequest) -> Response + Send + Sync + 'static,
    ) {
        self.scripts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((tag.into(), Arc::new(script)));
    }
    /// Requests whose newest user message carried `tag`.
    pub fn requests(&self, tag: &str) -> Vec<Value> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(tag)
            .cloned()
            .unwrap_or_default()
    }
    fn reply(&self, body: Value) -> Response {
        let messages = body["messages"].as_array().cloned().unwrap_or_default();
        let user = messages
            .iter()
            .rev()
            .find(|m| m["role"] == "user")
            .map(|m| m["content"].to_string())
            .unwrap_or_default();
        let all = body.to_string();
        let request = ModelRequest {
            direct: all.contains("RESPONSE ROUTING:"),
            spoken: all.contains("SPOKEN RESPONSE PHASE:"),
            tool_results: messages.iter().filter(|m| m["role"] == "tool").count(),
            body,
        };
        let script = self
            .scripts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .rev()
            .find(|(tag, _)| user.contains(tag.as_str()))
            .map(|(tag, s)| (tag.clone(), s.clone()));
        match script {
            Some((tag, script)) => {
                self.requests
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .entry(tag)
                    .or_default()
                    .push(request.body.clone());
                script(&request)
            }
            None => sse::text("Okay."),
        }
    }
}

async fn serve_mock(listener: tokio::net::TcpListener, mock: Arc<Mock>) {
    let chat = {
        let mock = mock.clone();
        move |axum::Json(body): axum::Json<Value>| {
            let mock = mock.clone();
            async move { mock.reply(body) }
        }
    };
    // HTTPS_PROXY points here, so an outbound HTTPS call (web search) arrives as CONNECT.
    // It is counted and never answered: the call hangs until its own timeout.
    let proxy = move |request: axum::extract::Request| {
        let mock = mock.clone();
        async move {
            if request.method() == axum::http::Method::CONNECT {
                mock.connects.fetch_add(1, Ordering::SeqCst);
                std::future::pending::<()>().await;
            }
            axum::http::StatusCode::NOT_FOUND.into_response()
        }
    };
    let app = axum::Router::new()
        .route("/v1/chat/completions", axum::routing::post(chat))
        .fallback(proxy);
    let _ = axum::serve(listener, app).await;
}

/// Server-sent event frames in the streaming chat completions shape.
pub mod sse {
    use super::{DropGuard, IntoResponse, Response, Value, json};
    use std::time::Duration;

    pub fn chunk(delta: &Value, finish: Option<&str>) -> String {
        let mut chunk = json!({"id":"chatcmpl-fixture","object":"chat.completion.chunk",
            "created":1,"model":"fixture",
            "choices":[{"index":0,"delta":delta,"finish_reason":finish}]});
        if finish.is_some() {
            chunk["usage"] = json!({"prompt_tokens":3,"completion_tokens":2,"total_tokens":5});
        }
        format!("data: {chunk}\n\n")
    }
    pub fn content(text: &str) -> String {
        chunk(&json!({"role":"assistant","content":text}), None)
    }
    pub fn stop() -> String {
        format!("{}data: [DONE]\n\n", chunk(&json!({}), Some("stop")))
    }
    pub fn tool_call(id: &str, name: &str, args: &Value) -> String {
        let delta = json!({"role":"assistant","tool_calls":[{"index":0,"id":id,"type":"function",
            "function":{"name":name,"arguments":args.to_string()}}]});
        format!("{}data: [DONE]\n\n", chunk(&delta, Some("tool_calls")))
    }
    pub fn body(frames: String) -> Response {
        ([("content-type", "text/event-stream")], frames).into_response()
    }
    pub fn text(text: &str) -> Response {
        body(format!("{}{}", content(text), stop()))
    }
    pub fn tool(id: &str, name: &str, args: &Value) -> Response {
        body(tool_call(id, name, args))
    }
    /// Streams `frames` one per `gap`; `guard` records when the stream is dropped.
    pub fn paced(frames: Vec<String>, gap: Duration, guard: Option<DropGuard>) -> Response {
        let stream = futures_util::stream::unfold(
            (frames.into_iter(), guard),
            move |(mut frames, guard)| async move {
                tokio::time::sleep(gap).await;
                let frame = frames.next()?;
                Some((Ok::<_, std::io::Error>(frame), (frames, guard)))
            },
        );
        (
            [("content-type", "text/event-stream")],
            axum::body::Body::from_stream(stream),
        )
            .into_response()
    }
    /// `reply` as ~4-character tokens, one per `gap`.
    pub fn tokens(reply: &str, gap: Duration, guard: Option<DropGuard>) -> Response {
        let chars: Vec<char> = reply.chars().collect();
        let mut frames: Vec<String> = chars
            .chunks(4)
            .map(|c| content(&c.iter().collect::<String>()))
            .collect();
        frames.push(stop());
        paced(frames, gap, guard)
    }
    /// Sends `frames` at once, then holds the stream open for `hold` without finishing.
    pub fn hang_after(frames: String, hold: Duration) -> Response {
        let stream = futures_util::stream::unfold(Some(frames), move |state| async move {
            if let Some(frames) = state {
                Some((Ok::<_, std::io::Error>(frames), None))
            } else {
                tokio::time::sleep(hold).await;
                None
            }
        });
        (
            [("content-type", "text/event-stream")],
            axum::body::Body::from_stream(stream),
        )
            .into_response()
    }
}

/// Records when a mock response body is dropped, i.e. the provider connection closed,
/// and wakes a waiter.
pub struct DropGuard(
    pub Arc<Mutex<Option<Instant>>>,
    pub Option<Arc<tokio::sync::watch::Sender<bool>>>,
);
impl Drop for DropGuard {
    fn drop(&mut self) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Instant::now());
        if let Some(notify) = &self.1 {
            notify.send_replace(true);
        }
    }
}

// ---------------------------------------------------------------------------------------
// Test bots
// ---------------------------------------------------------------------------------------

/// An enrolled bot with its three actors and its sessions.
pub struct TestBot {
    pub id: String,
    pub admin: String,
    pub device: String,
    pub agent: String,
    pub bot: BotActor,
    pub conversation: ConversationActor,
    pub memory: MemoryActor,
    pub client: Client,
}

pub type Handle<A> = TypedActorHandle<A>;

/// A handle to actor `name` keyed by `bot`, connecting with `session`.
pub fn handle<A: rivetkit::Actor>(
    client: &Client,
    name: &str,
    bot: &str,
    session: &str,
) -> Result<TypedActorHandle<A>> {
    client.get_or_create_typed::<A>(
        name,
        [bot.to_owned()],
        GetOrCreateOptions {
            params: Some(json!({ "session": session })),
            ..Default::default()
        },
    )
}

impl TestBot {
    /// Enrolls a fresh bot through the admin path, like `/v1/enroll` does.
    pub async fn enroll(h: &Harness) -> Result<Self> {
        let id = auth::random();
        let client = h.client();
        let device = h.token(&id, "device");
        let bot = Self {
            admin: h.token(&id, "admin"),
            agent: h.token(&id, "agent"),
            bot: handle(&client, "bot", &id, &device)?,
            conversation: handle(&client, "conversation", &id, &device)?,
            memory: handle(&client, "memory", &id, &device)?,
            device,
            id,
            client,
        };
        let enrolled = bot
            .bot_as(
                &bot.admin.clone(),
                Command::Enroll {
                    device_id: format!("device-{}", bot.id),
                    public_key: "test-key".into(),
                },
            )
            .await?;
        ensure!(
            enrolled["bot_id"] == bot.id.as_str(),
            "enroll returned {enrolled}"
        );
        // Like `/v1/session`: the conversation reaches memory with `get`, not `get_or_create`.
        bot.conversation.inner().resolve().await?;
        bot.memory.inner().resolve().await?;
        Ok(bot)
    }

    pub async fn bot_as(&self, session: &str, command: Command) -> Result<Value> {
        self.bot
            .send(Call {
                session: session.into(),
                command,
            })
            .await
    }
    pub async fn conv_as(&self, session: &str, command: Command) -> Result<Value> {
        self.conversation
            .send(Call {
                session: session.into(),
                command,
            })
            .await
    }
    pub async fn mem_as(&self, session: &str, command: Command) -> Result<Value> {
        self.memory
            .send(Call {
                session: session.into(),
                command,
            })
            .await
    }
    /// A conversation call as the device.
    pub async fn conv(&self, command: Command) -> Result<Value> {
        self.conv_as(&self.device, command).await
    }
    pub async fn mem(&self, command: Command) -> Result<Value> {
        self.mem_as(&self.device, command).await
    }

    pub async fn submit(&self, request_id: &str, text: &str) -> Result<String> {
        let turn = self
            .conv(Command::Submit {
                request_id: request_id.into(),
                text: text.into(),
            })
            .await?;
        Ok(turn["turn_id"].as_str().context("turn id")?.to_owned())
    }

    /// Owner-claims the bot (device begins, admin proposes, device confirms).
    pub async fn claim(&self, owner: &str) -> Result<()> {
        let claim = self.bot_as(&self.device, Command::BeginClaim).await?;
        let proposed = self
            .bot_as(
                &self.admin,
                Command::ProposeClaim {
                    token: claim["token"].as_str().context("claim token")?.into(),
                    owner: owner.into(),
                },
            )
            .await?;
        self.bot_as(
            &self.device,
            Command::ConfirmClaim {
                code: proposed["code"].as_str().context("code")?.into(),
            },
        )
        .await?;
        Ok(())
    }

    /// A live event feed for this bot's conversation.
    pub async fn events(&self) -> Result<Events> {
        Events::open(self, &self.device).await
    }

    /// Submits `text` and waits for its turn to finish; returns the turn id and its events.
    pub async fn turn(&self, request_id: &str, text: &str) -> Result<(String, Vec<Seen>)> {
        let mut events = self.events().await?;
        let turn = self.submit(request_id, text).await?;
        let done = events.wait_terminal(&turn, WAIT).await?;
        Ok((turn, done))
    }

    pub async fn turn_record(&self, turn: &str) -> Result<Value> {
        let snapshot = self.conv(Command::Snapshot).await?;
        snapshot["turns"]
            .as_array()
            .context("turns")?
            .iter()
            .find(|t| t["id"] == turn)
            .cloned()
            .with_context(|| format!("turn {turn} not in snapshot"))
    }
}

/// An event and when this test first saw it.
#[derive(Clone, Debug)]
pub struct Seen {
    pub event: OutputEvent,
    pub at: Instant,
}

/// Conversation events from a live subscription, backed by replay so nothing is missed
/// (events before the subscription, or across a reconnect).
pub struct Events {
    conversation: ConversationActor,
    session: String,
    /// Pushed events, stamped when the callback ran (not when the test polled).
    rx: tokio::sync::mpsc::UnboundedReceiver<(Instant, OutputEvent)>,
    _connection: TypedActorConnection<robot_buddy::conversation::Conversation>,
    log: BTreeMap<i64, Seen>,
    /// The pushed copy of each event, as the subscription delivered it.
    pushed: BTreeMap<i64, OutputEvent>,
}

pub const TERMINAL: [&str; 4] = [
    "turn.completed",
    "turn.failed",
    "turn.cancelled",
    "turn.interrupted",
];

impl Events {
    pub async fn open(bot: &TestBot, session: &str) -> Result<Self> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let conversation: ConversationActor =
            handle(&bot.client, "conversation", &bot.id, session)?;
        let connection = conversation.connect();
        connection
            .on::<OutputEvent>(move |event| {
                let _ = tx.send((Instant::now(), event));
            })
            .await;
        let mut events = Self {
            conversation,
            session: session.into(),
            rx,
            _connection: connection,
            log: BTreeMap::new(),
            pushed: BTreeMap::new(),
        };
        events.pull().await?;
        Ok(events)
    }

    fn record_push(&mut self, event: OutputEvent, at: Instant) {
        self.pushed.insert(event.sequence, event.clone());
        self.record(event, at);
    }

    /// Keeps the earliest sighting: a replayed copy never moves `at` later than the push.
    fn record(&mut self, event: OutputEvent, at: Instant) {
        let seen = self.log.entry(event.sequence).or_insert(Seen { event, at });
        seen.at = seen.at.min(at);
    }

    /// The last sequence before the first hole in the log. Sequences are an
    /// `AUTOINCREMENT` key and never deleted, so a hole is an event a reconnect dropped
    /// while a later one was pushed; replaying from the newest key would skip it.
    fn contiguous_end(&self) -> i64 {
        let mut keys = self.log.keys().copied();
        let Some(mut end) = keys.next() else {
            return 0;
        };
        for key in keys {
            if key != end + 1 {
                break;
            }
            end = key;
        }
        end
    }

    /// Replays from the first hole in the log, then pages forward.
    pub async fn pull(&mut self) -> Result<()> {
        let mut after = self.contiguous_end();
        loop {
            let page = self
                .conversation
                .send(Call {
                    session: self.session.clone(),
                    command: Command::Replay { after },
                })
                .await?;
            let page: Vec<OutputEvent> = serde_json::from_value(page)?;
            let full = page.len() >= 128;
            let at = Instant::now();
            for event in page {
                self.record(event, at);
            }
            if !full {
                return Ok(());
            }
            // Follow the contiguous end while it moves, then page past whatever is newest.
            let newest = self.log.keys().next_back().copied().unwrap_or(0);
            let next = self.contiguous_end().max(after);
            let next = if next > after { next } else { newest };
            if next <= after {
                return Ok(());
            }
            after = next;
        }
    }

    /// One raw `Replay` page after `after`, exactly as the actor returns it.
    pub async fn replayed(&self, after: i64) -> Result<Vec<OutputEvent>> {
        let page = self
            .conversation
            .send(Call {
                session: self.session.clone(),
                command: Command::Replay { after },
            })
            .await?;
        Ok(serde_json::from_value(page)?)
    }

    /// Waits for the pushed copy of the event at `sequence`.
    pub async fn wait_pushed(&mut self, sequence: i64, within: Duration) -> Result<OutputEvent> {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            if let Some(event) = self.pushed.get(&sequence) {
                return Ok(event.clone());
            }
            match tokio::time::timeout_at(deadline, self.rx.recv()).await {
                Ok(Some((at, event))) => self.record_push(event, at),
                Ok(None) => bail!("subscription closed before event {sequence} was pushed"),
                Err(_) => bail!("event {sequence} was never pushed within {within:?}"),
            }
        }
    }

    pub fn log(&self) -> Vec<Seen> {
        self.log.values().cloned().collect()
    }

    pub fn of_turn(&self, turn: &str) -> Vec<Seen> {
        self.log
            .values()
            .filter(|s| s.event.turn_id == turn)
            .cloned()
            .collect()
    }

    /// Waits until `check` returns `Some` over the log. Wakes on each pushed event, and
    /// replays every 250 ms in case a push was missed.
    pub async fn until<T>(
        &mut self,
        within: Duration,
        what: &str,
        mut check: impl FnMut(&[Seen]) -> Option<T>,
    ) -> Result<T> {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let log: Vec<Seen> = self.log.values().cloned().collect();
            if let Some(found) = check(&log) {
                return Ok(found);
            }
            tokio::select! {
                event = self.rx.recv() => {
                    if let Some((at, event)) = event {
                        self.record_push(event, at);
                        while let Ok((at, event)) = self.rx.try_recv() {
                            self.record_push(event, at);
                        }
                    } else {
                        tokio::time::sleep(Duration::from_millis(250)).await;
                        self.pull().await?;
                    }
                }
                () = tokio::time::sleep(Duration::from_millis(250)) => {
                    // Replay can fail while the registry restarts; the deadline still holds.
                    let _ = self.pull().await;
                }
                () = tokio::time::sleep_until(deadline) => {
                    let kinds: Vec<String> = log
                        .iter()
                        .map(|s| format!("{}:{}", s.event.sequence, s.event.kind))
                        .collect();
                    bail!("timed out after {within:?} waiting for {what}; saw {kinds:?}");
                }
            }
        }
    }

    /// Waits for `kind` on `turn` and returns it.
    pub async fn wait_kind(&mut self, turn: &str, kind: &str, within: Duration) -> Result<Seen> {
        self.until(within, &format!("{kind} on {turn}"), |log| {
            log.iter()
                .find(|s| s.event.turn_id == turn && s.event.kind == kind)
                .cloned()
        })
        .await
    }

    /// Waits for the turn's terminal event and returns all of its events.
    pub async fn wait_terminal(&mut self, turn: &str, within: Duration) -> Result<Vec<Seen>> {
        self.until(within, &format!("terminal event on {turn}"), |log| {
            let ours: Vec<Seen> = log
                .iter()
                .filter(|s| s.event.turn_id == turn)
                .cloned()
                .collect();
            ours.iter()
                .any(|s| TERMINAL.contains(&s.event.kind.as_str()))
                .then_some(ours)
        })
        .await
    }
}

pub fn kinds(events: &[Seen]) -> Vec<&str> {
    events.iter().map(|s| s.event.kind.as_str()).collect()
}

pub fn terminal(events: &[Seen]) -> Option<&Seen> {
    events
        .iter()
        .find(|s| TERMINAL.contains(&s.event.kind.as_str()))
}

/// Joined `text.delta` payloads.
pub fn streamed_text(events: &[Seen]) -> String {
    events
        .iter()
        .filter(|s| s.event.kind == "text.delta")
        .filter_map(|s| s.event.payload["text"].as_str())
        .collect()
}

pub fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "error,robot_buddy=warn".into()),
        )
        .with_test_writer()
        .try_init();
}

/// Fixed turn ID and sequence in the output event fixtures.
pub const GOLDEN_TURN: &str = "0195f0a2-7c1e-7d2b-9a40-3f1c2b8e9d10";
pub const GOLDEN_SEQUENCE: i64 = 7;

/// Compares an event the actor produced with `fixtures/protocol/output_event.<name>.json`
/// after replacing its sequence and turn ID with the fixture's. `BUDDY_UPDATE_GOLDEN=1`
/// writes the fixture instead, so fixtures are captured from the actor, never written by
/// hand.
pub fn golden_event(name: &str, event: &OutputEvent) -> Result<()> {
    let mut normalized = event.clone();
    normalized.sequence = GOLDEN_SEQUENCE;
    normalized.turn_id = GOLDEN_TURN.into();
    let actual = serde_json::to_value(&normalized)?;
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/protocol")
        .join(format!("output_event.{name}.json"));
    if std::env::var("BUDDY_UPDATE_GOLDEN").is_ok_and(|v| v == "1") {
        let text = serde_json::to_string_pretty(&actual)?;
        std::fs::write(&path, format!("{text}\n"))?;
    }
    let raw = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "{}; capture it with BUDDY_UPDATE_GOLDEN=1 bun run test:integration",
            path.display()
        )
    })?;
    let expected: Value = serde_json::from_str(&raw)?;
    ensure!(
        actual == expected,
        "{name}: the actor emitted {actual}, the fixture says {expected}"
    );
    Ok(())
}

/// Wall-clock bounds are reported, and enforced only with `BUDDY_STRICT_TIMING=1` on a
/// controlled runner. Functional assertions never depend on this.
pub fn timing_bound(what: &str, took: Duration, limit: Duration) -> Result<()> {
    let strict = std::env::var("BUDDY_STRICT_TIMING").is_ok_and(|v| v == "1");
    let within = took < limit;
    println!(
        "TIMING {what}_ms={} limit_ms={} {}",
        took.as_millis(),
        limit.as_millis(),
        match (within, strict) {
            (true, _) => "ok",
            (false, true) => "OVER (enforced)",
            (false, false) => "OVER (reported; BUDDY_STRICT_TIMING=1 enforces)",
        }
    );
    ensure!(within || !strict, "{what} took {took:?}, limit {limit:?}");
    Ok(())
}

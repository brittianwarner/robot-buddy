//! The supervised Rivet engine: its private runtime config and process lifecycle.
use super::config::{env, secret_endpoint};
use anyhow::{Result, ensure};
use robot_buddy::auth;
use serde_json::{Value, json};
use std::{
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::Duration,
};

const CONFIG_PREFIX: &str = "robot-buddy-engine-";
/// A config from before names carried the writer's PID is stale once this old.
const LEGACY_CONFIG_AGE: Duration = Duration::from_hours(1);

/// Engine config holding the admin token and S3 credentials. It lives outside the backed-up
/// data volume and is removed when this guard drops.
pub struct RuntimeFile(pub PathBuf);
impl Drop for RuntimeFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// `$XDG_RUNTIME_DIR` when set, else the system temp directory; never the data volume.
pub fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute() && p.is_dir())
        .unwrap_or_else(std::env::temp_dir)
}

/// Writes `data` to a new `0600` file with an unguessable name (`create_new`, so an
/// existing file or symlink at that path is never followed). The name carries this
/// process's PID, so a later start can tell an orphaned config from a live one.
pub fn write_runtime_config(dir: &Path, data: &[u8]) -> Result<RuntimeFile> {
    let name = format!(
        "{CONFIG_PREFIX}{}-{}.json",
        std::process::id(),
        auth::random()
    );
    let path = dir.join(name);
    private_write(&path, data)?;
    Ok(RuntimeFile(path))
}

/// Removes engine configs, with their secrets, that a crashed or killed server left in
/// `dir`. Only regular files owned by the same user as `keep` (this process's config)
/// are touched: a `{pid}-{id}` config once that process is gone (this PID counts as
/// gone, since `keep` is the only config this process writes), and a legacy `{id}`
/// config once it is older than [`LEGACY_CONFIG_AGE`]. Returns how many were removed.
pub fn sweep_stale_configs(dir: &Path, keep: &Path) -> usize {
    let Ok(owner) = std::fs::symlink_metadata(keep).map(|m| m.uid()) else {
        return 0;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path == keep {
            continue;
        }
        let Some(writer) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(config_writer)
        else {
            continue;
        };
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.file_type().is_file() || meta.uid() != owner {
            continue;
        }
        let stale = match writer {
            Writer::Process(pid) => !alive(pid),
            Writer::Legacy => meta
                .modified()
                .ok()
                .and_then(|at| at.elapsed().ok())
                .is_some_and(|age| age > LEGACY_CONFIG_AGE),
        };
        if stale && std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[derive(Debug, PartialEq, Eq)]
enum Writer {
    Process(i32),
    Legacy,
}

/// Who wrote a config file, from its name; `None` for anything else.
fn config_writer(name: &str) -> Option<Writer> {
    let stem = name.strip_prefix(CONFIG_PREFIX)?.strip_suffix(".json")?;
    let id = |s: &str| s.len() == 36 && s.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-');
    if id(stem) {
        return Some(Writer::Legacy);
    }
    let (pid, rest) = stem.split_once('-')?;
    if !id(rest) || pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    pid.parse().ok().map(Writer::Process)
}

/// Whether another process with this PID exists. Signal 0 checks without signaling;
/// `EPERM` means it exists under another user.
fn alive(pid: i32) -> bool {
    use nix::{errno::Errno, sys::signal, unistd::Pid};
    if pid <= 0 || u32::try_from(pid).is_ok_and(|pid| pid == std::process::id()) {
        return false;
    }
    !matches!(signal::kill(Pid::from_raw(pid), None), Err(Errno::ESRCH))
}

/// `object_storage.primary` for the engine. `var` reads the environment.
fn object_storage(data: &str, var: impl Fn(&str) -> Option<String>) -> Result<Value> {
    match var("BUDDY_STORAGE_BACKEND").as_deref() {
        Some("s3") => {
            let mut s3 = json!({
                "bucket": var("BUDDY_S3_BUCKET").ok_or_else(|| anyhow::anyhow!("BUDDY_S3_BUCKET required"))?,
            });
            for (field, name) in [
                ("region", "BUDDY_S3_REGION"),
                ("endpoint", "BUDDY_S3_ENDPOINT"),
                ("access_key_id", "BUDDY_S3_ACCESS_KEY_ID"),
                ("secret_access_key", "BUDDY_S3_SECRET_ACCESS_KEY"),
            ] {
                if let Some(v) = var(name) {
                    s3[field] = json!(v);
                }
            }
            // The engine signs requests to it with the storage keys.
            if let Some(endpoint) = s3["endpoint"].as_str() {
                let plaintext = var("BUDDY_S3_ENDPOINT_PLAINTEXT_HOST").filter(|h| !h.is_empty());
                secret_endpoint("BUDDY_S3_ENDPOINT", endpoint, plaintext.as_deref())?;
            }
            Ok(json!({"s3":s3}))
        }
        Some("file_system") | None => Ok(json!({"file_system":{"path":format!("{data}/objects")}})),
        _ => anyhow::bail!("unsupported storage backend"),
    }
}

pub async fn spawn_engine(path: &str) -> Result<(tokio::process::Child, RuntimeFile)> {
    let data = std::env::var("BUDDY_DATA_DIR").unwrap_or_else(|_| "/data".into());
    tokio::fs::create_dir_all(format!("{data}/logs")).await?;
    tokio::fs::create_dir_all(format!("{data}/objects")).await?;
    // Earlier releases wrote the config, with secrets, into the backed-up volume.
    let _ = tokio::fs::remove_file(format!("{data}/engine.json")).await;
    let object_storage = object_storage(&data, |name| std::env::var(name).ok())?;
    let port: u16 = std::env::var("BUDDY_ENGINE_PORT")
        .unwrap_or_else(|_| "6420".into())
        .parse()?;
    ensure!(port > 1024 && port < 65525, "invalid engine port");
    let config = json!({
        "auth": {"admin_token": env("RIVET_TOKEN")?},
        "guard": {"host": "0.0.0.0", "port": port},
        "api_peer": {"host": "127.0.0.1", "port": port + 1},
        "metrics": {"host": "127.0.0.1", "port": port + 10},
        "storage": {
            "log_directory": format!("{data}/logs"),
            "balancing": {"enabled": false},
            "auto_provision_partitions": 1,
            "log_shard": {"max_disk_bytes": 4_294_967_296u64},
            "cold_store": {"disk_cache_bytes": 0},
        },
        "object_storage": {"primary": object_storage},
        "telemetry": {"enabled": false},
    });
    let dir = runtime_dir();
    let config_file = write_runtime_config(&dir, &serde_json::to_vec_pretty(&config)?)?;
    let swept = sweep_stale_configs(&dir, &config_file.0);
    if swept > 0 {
        tracing::info!(swept, "removed stale engine configs");
    }
    // The engine reads the file named by `--config` at startup; it stays until shutdown.
    let mut child = tokio::process::Command::new(path)
        .arg("--config")
        .arg(&config_file.0)
        .kill_on_drop(true)
        .spawn()?;
    let http = reqwest::Client::new();
    for _ in 0..100 {
        if http
            .get(format!("http://127.0.0.1:{port}/health"))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
        {
            return Ok((child, config_file));
        }
        if let Some(status) = child.try_wait()? {
            anyhow::bail!("engine exited during startup: {status}");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    anyhow::bail!("engine did not become healthy")
}

/// Ask the engine to drain with SIGTERM, sent directly rather than through `kill(1)`.
pub fn terminate(child: &tokio::process::Child) {
    use nix::{sys::signal, unistd::Pid};
    if let Some(id) = child.id().and_then(|id| i32::try_from(id).ok()) {
        let _ = signal::kill(Pid::from_raw(id), signal::Signal::SIGTERM);
    }
}

pub fn private_write(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(data)?;
    f.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashMap, time::SystemTime};

    fn scratch() -> Result<PathBuf> {
        let dir = std::env::temp_dir().join(format!("rb-engine-test-{}", auth::random()));
        std::fs::create_dir(&dir)?;
        Ok(dir)
    }

    #[test]
    fn engine_config_is_private_ephemeral_and_removed() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch()?;
        let first = write_runtime_config(&dir, b"{\"auth\":{}}")?;
        let second = write_runtime_config(&dir, b"{}")?;
        assert_ne!(first.0, second.0, "unique names");
        let mode = std::fs::metadata(&first.0)?.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(std::fs::read(&first.0)?, b"{\"auth\":{}}");
        let name = first.0.file_name().unwrap().to_str().unwrap().to_owned();
        let pid = i32::try_from(std::process::id())?;
        assert_eq!(config_writer(&name), Some(Writer::Process(pid)));
        let path = first.0.clone();
        drop(first);
        assert!(!path.exists(), "removed on drop");
        assert!(!runtime_dir().starts_with("/data"));
        drop(second);
        std::fs::remove_dir(&dir)?;
        Ok(())
    }

    #[test]
    fn config_names_name_their_writer() {
        let id = "0a1b2c3d-0000-4000-8000-00000000abcd";
        assert_eq!(
            config_writer(&format!("robot-buddy-engine-{id}.json")),
            Some(Writer::Legacy)
        );
        assert_eq!(
            config_writer(&format!("robot-buddy-engine-42-{id}.json")),
            Some(Writer::Process(42))
        );
        // A legacy id whose first group is all digits is still legacy.
        let digits = "12345678-0000-4000-8000-00000000abcd";
        assert_eq!(
            config_writer(&format!("robot-buddy-engine-{digits}.json")),
            Some(Writer::Legacy)
        );
        for other in [
            "robot-buddy-engine-.json",
            "robot-buddy-engine-x-0a1b2c3d-0000-4000-8000-00000000abcd.json",
            "robot-buddy-engine-42-short.json",
            "robot-buddy-engine-42-0a1b2c3d-0000-4000-8000-00000000abcd.txt",
            "other-0a1b2c3d-0000-4000-8000-00000000abcd.json",
        ] {
            assert_eq!(config_writer(other), None, "{other}");
        }
    }

    /// Orphaned configs go on start; live, recent, foreign and linked ones stay.
    #[test]
    fn a_start_sweeps_only_orphaned_configs() -> Result<()> {
        let dir = scratch()?;
        let id = || auth::random();
        // A PID that has exited.
        let mut child = std::process::Command::new("true").spawn()?;
        child.wait()?;
        let dead = child.id();
        let parent = std::os::unix::process::parent_id();
        let ours = std::process::id();
        let names: HashMap<&str, String> = [
            ("dead", format!("robot-buddy-engine-{dead}-{}.json", id())),
            (
                "earlier run, same pid",
                format!("robot-buddy-engine-{ours}-{}.json", id()),
            ),
            ("live", format!("robot-buddy-engine-{parent}-{}.json", id())),
            ("old legacy", format!("robot-buddy-engine-{}.json", id())),
            ("fresh legacy", format!("robot-buddy-engine-{}.json", id())),
            ("unrelated", "robot-buddy-engine-notes.json".to_owned()),
        ]
        .into_iter()
        .collect();
        for name in names.values() {
            private_write(&dir.join(name), b"{}")?;
        }
        std::fs::File::options()
            .write(true)
            .open(dir.join(&names["old legacy"]))?
            .set_modified(SystemTime::now() - Duration::from_hours(2))?;
        // A link named like an orphan is never followed or removed.
        let target = dir.join("target.json");
        private_write(&target, b"{}")?;
        let link = dir.join(format!("robot-buddy-engine-{dead}-{}.json", id()));
        std::os::unix::fs::symlink(&target, &link)?;
        let keep = write_runtime_config(&dir, b"{}")?;

        assert_eq!(sweep_stale_configs(&dir, &keep.0), 3);
        for gone in ["dead", "earlier run, same pid", "old legacy"] {
            assert!(!dir.join(&names[gone]).exists(), "{gone} removed");
        }
        for kept in ["live", "fresh legacy", "unrelated"] {
            assert!(dir.join(&names[kept]).exists(), "{kept} kept");
        }
        assert!(keep.0.exists() && target.exists());
        assert!(std::fs::symlink_metadata(&link).is_ok(), "link kept");
        drop(keep);
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |name| map.get(name).cloned()
    }

    #[test]
    fn s3_endpoints_carry_keys_only_over_tls() {
        let s3 = |extra: &[(&str, &str)]| {
            let mut pairs = vec![
                ("BUDDY_STORAGE_BACKEND", "s3"),
                ("BUDDY_S3_BUCKET", "b"),
                ("BUDDY_S3_SECRET_ACCESS_KEY", "k"),
            ];
            pairs.extend_from_slice(extra);
            object_storage("/d", vars(&pairs))
        };
        let ok = s3(&[("BUDDY_S3_ENDPOINT", "https://s3.example")]).unwrap();
        assert_eq!(ok["s3"]["endpoint"], "https://s3.example");
        assert!(s3(&[]).is_ok(), "provider default endpoint");
        assert!(s3(&[("BUDDY_S3_ENDPOINT", "http://127.0.0.1:9000")]).is_ok());
        let error = s3(&[("BUDDY_S3_ENDPOINT", "http://minio:9000")])
            .unwrap_err()
            .to_string();
        assert!(error.contains("BUDDY_S3_ENDPOINT"), "{error}");
        let opted_in = s3(&[
            ("BUDDY_S3_ENDPOINT", "http://minio:9000"),
            ("BUDDY_S3_ENDPOINT_PLAINTEXT_HOST", "minio"),
        ]);
        assert!(opted_in.is_ok(), "explicit opt-in for one host");
        let files = object_storage("/d", vars(&[])).unwrap();
        assert_eq!(files["file_system"]["path"], "/d/objects");
    }
}

//! The setup hotspot's name and password, generated once per robot and kept in a
//! 0600 file in the service's state directory. The robot has no display of its
//! own, so `robot-buddy-robot setup-card`, run once at provisioning as the
//! service account, prints them with a QR code for a printed card. The loopback
//! panel shows the same QR when a kiosk is attached.
//!
//! The password is a possession token like a router's sticker: whoever holds
//! the card can join the setup network while it is up. Deleting the file
//! makes a new one at the next setup or `setup-card`; reprint the card then.
use anyhow::{Context, Result, bail, ensure};
use rand::{Rng, rngs::OsRng};
use robot_buddy::auth::Secret;
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

const FILE_NAME: &str = "setup-wifi.json";
const DEFAULT_DIR: &str = "/var/lib/robot-buddy";
const SSID_PREFIX: &str = "RobotBuddy-";
/// No `0`/`o`, `1`/`l`/`i`: the password may be typed from the card.
const ALPHABET: &[u8] = b"23456789abcdefghjkmnpqrstuvwxyz";
const SSID_SUFFIX: usize = 6;
/// Four groups of four: about 79 bits.
const GROUPS: usize = 4;
const GROUP: usize = 4;
/// The JSON is under 100 bytes; anything larger is not ours.
const MAX_FILE: u64 = 4096;

#[derive(Serialize, Deserialize)]
pub struct SetupWifi {
    pub ssid: String,
    pub password: Secret<String>,
}

impl SetupWifi {
    pub fn generate() -> Self {
        let pick = |n: usize| -> String {
            (0..n)
                .map(|_| char::from(ALPHABET[OsRng.gen_range(0..ALPHABET.len())]))
                .collect()
        };
        let groups: Vec<String> = (0..GROUPS).map(|_| pick(GROUP)).collect();
        Self {
            ssid: format!("{SSID_PREFIX}{}", pick(SSID_SUFFIX)),
            password: Secret::new(groups.join("-")),
        }
    }

    /// The `WIFI:` payload phones read from a QR code, escaped per the format.
    pub fn join_payload(&self) -> Secret<String> {
        let escape = |value: &str| {
            let mut out = String::with_capacity(value.len());
            for c in value.chars() {
                if matches!(c, '\\' | ';' | ',' | ':' | '"') {
                    out.push('\\');
                }
                out.push(c);
            }
            out
        };
        let password = Secret::new(escape(self.password.expose()));
        Secret::new(format!(
            "WIFI:T:WPA;S:{};P:{};;",
            escape(&self.ssid),
            password.expose()
        ))
    }
}

/// `BUDDY_SETUP_WIFI_FILE`, else `setup-wifi.json` in the unit's
/// `STATE_DIRECTORY`, else in `/var/lib/robot-buddy`.
pub fn path() -> PathBuf {
    if let Some(file) = std::env::var_os("BUDDY_SETUP_WIFI_FILE").filter(|v| !v.is_empty()) {
        return file.into();
    }
    let directory = std::env::var("STATE_DIRECTORY")
        .ok()
        .and_then(|dirs| dirs.split(':').next().map(PathBuf::from))
        .filter(|dir| dir.is_absolute())
        .unwrap_or_else(|| PathBuf::from(DEFAULT_DIR));
    directory.join(FILE_NAME)
}

/// Read the robot's setup credentials, creating them on first use. Two
/// processes racing here agree: the file appears complete or not at all.
pub fn load_or_create(path: &Path) -> Result<SetupWifi> {
    match read(path) {
        Ok(setup) => return Ok(setup),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) => {}
        Err(error) => return Err(error),
    }
    let directory = path.parent().context("setup Wi-Fi file has no directory")?;
    let staged = directory.join(format!(".{FILE_NAME}.{}", uuid::Uuid::new_v4().simple()));
    let written = write_new(&staged, &SetupWifi::generate());
    let linked = written.and_then(|()| match std::fs::hard_link(&staged, path) {
        Ok(()) => Ok(()),
        // Another process created it first; use theirs.
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error).context("could not save the setup Wi-Fi file"),
    });
    let _ = std::fs::remove_file(&staged);
    linked?;
    if let Ok(dir) = std::fs::File::open(directory) {
        let _ = dir.sync_all();
    }
    // A file the service account cannot read would break setup: refuse to leave
    // one owned by another account (`setup-card` run as root by mistake).
    let owner = std::fs::metadata(path)?.uid();
    if owner != std::fs::metadata(directory)?.uid() {
        let _ = std::fs::remove_file(path);
        bail!(
            "run setup-card as the account that owns {}",
            directory.display()
        );
    }
    read(path)
}

fn write_new(path: &Path, setup: &SetupWifi) -> Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("could not create {}", path.display()))?;
    let json = Secret::new(serde_json::to_vec(setup)?);
    file.write_all(json.expose())?;
    file.sync_all()?;
    Ok(())
}

fn read(path: &Path) -> Result<SetupWifi> {
    let file = std::fs::File::open(path)?;
    let meta = file.metadata()?;
    ensure!(meta.is_file(), "{} is not a file", path.display());
    let shared = meta.permissions().mode() & 0o077;
    ensure!(
        shared == 0,
        "{} must be readable by its owner only (0600)",
        path.display()
    );
    let mut raw = Vec::new();
    file.take(MAX_FILE + 1).read_to_end(&mut raw)?;
    let bytes = Secret::new(raw);
    ensure!(
        bytes.expose().len() as u64 <= MAX_FILE,
        "{} is too large",
        path.display()
    );
    let setup: SetupWifi = serde_json::from_slice(bytes.expose())
        .with_context(|| format!("{} is not a setup Wi-Fi file", path.display()))?;
    super::validate(&setup.ssid, setup.password.expose())?;
    Ok(setup)
}

/// The printable card: network, password, the setup address and a QR code
/// (dark modules as blocks, for printing on white).
pub fn card(setup: &SetupWifi) -> Result<Secret<String>> {
    let payload = setup.join_payload();
    let qr = Secret::new(
        qrcode::QrCode::new(payload.expose().as_bytes())?
            .render::<qrcode::render::unicode::Dense1x2>()
            .quiet_zone(true)
            .build(),
    );
    Ok(Secret::new(format!(
        "Robot Buddy setup Wi-Fi\n\n\
         Network:  {}\n\
         Password: {}\n\n\
         Join this network when the robot opens it, then visit http://{}\n\n\
         {}\n",
        setup.ssid,
        setup.password.expose(),
        super::SETUP_HOST,
        qr.expose()
    )))
}

/// `robot-buddy-robot setup-card`: print the card on stdout, creating the
/// credentials if this robot has none yet. Nothing is logged.
pub fn print_setup_card() -> Result<()> {
    write_setup_card(&path(), &mut std::io::stdout().lock())
}

fn write_setup_card(path: &Path, out: &mut impl Write) -> Result<()> {
    let setup = load_or_create(path)?;
    let card = card(&setup)?;
    out.write_all(card.expose().as_bytes())?;
    writeln!(out, "Stored in {}", path.display())?;
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("buddy-card-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn credentials_are_created_once_private_and_reused() {
        let dir = scratch();
        let file = dir.join(FILE_NAME);
        let first = load_or_create(&file).unwrap();
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(first.ssid.starts_with(SSID_PREFIX));
        let password = first.password.expose();
        assert_eq!(password.len(), GROUPS * GROUP + GROUPS - 1);
        super::super::validate(&first.ssid, password).unwrap();

        let again = load_or_create(&file).unwrap();
        assert_eq!(again.ssid, first.ssid);
        assert_eq!(again.password.expose(), password);
        // No staging file is left beside it.
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, [FILE_NAME]);

        // A loosened file is refused rather than trusted.
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        let error = load_or_create(&file).err().unwrap();
        assert!(error.to_string().contains("0600"), "{error}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_damaged_file_is_an_error_not_a_new_password() {
        let dir = scratch();
        let file = dir.join(FILE_NAME);
        std::fs::write(&file, b"{\"ssid\":\"x\"").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(load_or_create(&file).is_err());
        // The printed card stays valid: nothing replaced the file.
        assert_eq!(std::fs::read(&file).unwrap(), b"{\"ssid\":\"x\"");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_card_carries_the_network_password_and_a_qr_code() {
        let setup = SetupWifi {
            ssid: "RobotBuddy-abc234".into(),
            password: Secret::new("k7m2-xq9a-bcde-fghj".into()),
        };
        let card = card(&setup).unwrap();
        let text = card.expose();
        assert!(text.contains("Network:  RobotBuddy-abc234\n"), "{text}");
        assert!(text.contains("Password: k7m2-xq9a-bcde-fghj\n"), "{text}");
        assert!(text.contains("http://10.42.0.1"));
        assert!(text.contains('\u{2588}'), "QR blocks");
        assert_eq!(
            setup.join_payload().expose(),
            "WIFI:T:WPA;S:RobotBuddy-abc234;P:k7m2-xq9a-bcde-fghj;;"
        );
        let odd = SetupWifi {
            ssid: "a;b".into(),
            password: Secret::new("p:a\\ss,word\"".into()),
        };
        assert_eq!(
            odd.join_payload().expose(),
            "WIFI:T:WPA;S:a\\;b;P:p\\:a\\\\ss\\,word\\\";;"
        );
    }

    #[test]
    fn the_setup_card_command_prints_the_stored_network() {
        let dir = scratch();
        let file = dir.join(FILE_NAME);
        let mut out = Vec::new();
        write_setup_card(&file, &mut out).unwrap();
        let stored = load_or_create(&file).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains(&format!("Network:  {}\n", stored.ssid)));
        assert!(text.contains(&format!("Password: {}\n", stored.password.expose())));
        assert!(text.ends_with(&format!("Stored in {}\n", file.display())));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn generated_credentials_differ_per_robot() {
        let a = SetupWifi::generate();
        let b = SetupWifi::generate();
        assert_ne!(a.password.expose(), b.password.expose());
        assert!(
            a.password
                .expose()
                .bytes()
                .all(|c| c == b'-' || ALPHABET.contains(&c))
        );
    }
}

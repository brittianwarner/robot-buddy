//! Only normalized `SFace` vectors are durable. The parent directory must be trusted.
use anyhow::{Context, Result, ensure};
use std::{
    collections::BTreeMap,
    fs::{self, DirBuilder, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

const DIM: usize = 128;
const MAX_PEOPLE: usize = 64;
const MAX_SAMPLES: usize = 8;
const THRESHOLD: f32 = 0.42;

pub(super) struct Feature([f32; DIM]);

impl Feature {
    pub(super) fn new(values: &[f32]) -> Result<Self> {
        let mut row: [f32; DIM] = values.try_into().context("unexpected SFace dimension")?;
        let norm = row.iter().map(|v| v * v).sum::<f32>().sqrt();
        ensure!(norm.is_finite() && norm > 0.0, "invalid SFace feature");
        for value in &mut row {
            *value /= norm;
        }
        Ok(Self(row))
    }

    fn similarity(&self, other: &Self) -> f32 {
        self.0.iter().zip(other.0.iter()).map(|(a, b)| a * b).sum()
    }
}

pub(super) fn key(name: &str) -> Result<String> {
    let name = name.trim().to_ascii_lowercase();
    ensure!(
        (1..=32).contains(&name.len())
            && name.as_bytes()[0].is_ascii_lowercase()
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b" '-".contains(&b)),
        "name must start with a letter and use 1-32 ASCII letters, digits, spaces, apostrophes or hyphens"
    );
    Ok(name)
}

pub(super) struct Gallery {
    path: PathBuf,
    // Exclusive process ownership prevents competing robot processes from losing updates.
    directory: File,
    rows: BTreeMap<String, Vec<Feature>>,
}

impl Gallery {
    pub(super) fn open(path: &Path) -> Result<Self> {
        match DirBuilder::new().mode(0o700).create(path) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(e) => return Err(e.into()),
        }
        let uid = nix::unistd::geteuid().as_raw();
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW)
            .open(path)?;
        let meta = directory.metadata()?;
        ensure!(
            meta.uid() == uid && meta.mode() & 0o777 == 0o700,
            "gallery must be service-owned and mode 0700"
        );
        directory.try_lock().context("gallery already in use")?;
        let mut rows = BTreeMap::new();
        for entry in fs::read_dir(path)? {
            let file = entry?.path();
            if file.file_name().is_some_and(|s| s == ".pending") {
                fs::remove_file(file)?; // interrupted write, never a committed enrollment
                continue;
            }
            ensure!(
                file.extension().is_some_and(|e| e == "sf1"),
                "unexpected file in gallery"
            );
            let name = file
                .file_stem()
                .and_then(|v| v.to_str())
                .context("invalid gallery key")?;
            ensure!(
                key(name)? == name && rows.len() < MAX_PEOPLE,
                "invalid gallery entry"
            );
            let mut handle = OpenOptions::new()
                .read(true)
                .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
                .open(&file)?;
            let meta = handle.metadata()?;
            ensure!(
                meta.is_file()
                    && meta.uid() == uid
                    && meta.nlink() == 1
                    && meta.mode() & 0o777 == 0o600,
                "gallery files must be private regular files"
            );
            let mut bytes = Vec::new();
            (&mut handle)
                .take((MAX_SAMPLES * DIM * 4 + 1) as u64)
                .read_to_end(&mut bytes)?;
            ensure!(
                !bytes.is_empty()
                    && bytes.len() <= MAX_SAMPLES * DIM * 4
                    && bytes.len().is_multiple_of(DIM * 4),
                "invalid gallery length"
            );
            let samples = bytes
                .as_chunks::<{ DIM * 4 }>()
                .0
                .iter()
                .map(|row| {
                    let words = row.as_chunks::<4>().0;
                    let values: [f32; DIM] = std::array::from_fn(|i| f32::from_le_bytes(words[i]));
                    Feature::new(&values)
                })
                .collect::<Result<_>>()?;
            rows.insert(name.to_owned(), samples);
        }
        Ok(Self {
            path: path.to_owned(),
            directory,
            rows,
        })
    }

    pub(super) fn names(&self) -> Vec<String> {
        self.rows.keys().cloned().collect()
    }

    pub(super) fn identify(&self, feature: &Feature) -> Option<String> {
        self.rows
            .iter()
            .flat_map(|(name, rows)| rows.iter().map(move |r| (feature.similarity(r), name)))
            .filter(|(s, _)| *s >= THRESHOLD)
            .max_by(|(a, _), (b, _)| a.total_cmp(b))
            .map(|(_, name)| name.clone())
    }

    pub(super) fn add(&mut self, name: &str, feature: Feature) -> Result<()> {
        let name = key(name)?;
        let old = self.rows.get(&name);
        ensure!(
            old.is_some() || self.rows.len() < MAX_PEOPLE,
            "gallery full"
        );
        ensure!(
            old.is_none_or(|rows| rows.len() < MAX_SAMPLES),
            "person already has eight samples"
        );
        let temp = self.path.join(".pending");
        let mut out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        let result: Result<()> = (|| {
            for sample in old.into_iter().flatten().chain(std::iter::once(&feature)) {
                let bytes = sample.0.map(f32::to_le_bytes);
                out.write_all(bytes.as_flattened())?;
            }
            out.sync_all()?;
            fs::rename(&temp, self.path.join(format!("{name}.sf1")))?;
            Ok(())
        })();
        if let Err(e) = result {
            let _ = fs::remove_file(&temp);
            return Err(e);
        }
        self.rows.entry(name).or_default().push(feature);
        self.directory.sync_all()?;
        Ok(())
    }

    pub(super) fn forget(&mut self, name: &str) -> Result<()> {
        let name = key(name)?;
        ensure!(self.rows.contains_key(&name), "unknown gallery entry");
        fs::remove_file(self.path.join(format!("{name}.sf1")))?;
        self.rows.remove(&name);
        self.directory.sync_all()?;
        Ok(())
    }
}

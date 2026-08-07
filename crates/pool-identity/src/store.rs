//! Durable filesystem document store for identity state.
//!
//! Extends the S3 spike pool's fsynced-doc pattern
//! (`crates/spike/src/pool.rs`, architecture §8.2 filesystem publication
//! rule): every state document is written to a temp file, fsynced, atomically
//! renamed, and the parent directory fsynced before the write is
//! acknowledged. Multi-field commits are single atomic renames; recovery is a
//! pure read of what survived. This stands in for the relational database
//! rows the production Pool API will own (architecture §6 table, credential
//! rows).

use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::{IdentityError, IdentityErrorCode, IdentityResult};

fn storage(detail: impl std::fmt::Display) -> IdentityError {
    IdentityError::new(IdentityErrorCode::Storage, detail.to_string())
}

pub(crate) struct Store {
    root: PathBuf,
}

impl Store {
    pub(crate) fn open(root: impl Into<PathBuf>) -> IdentityResult<Store> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(storage)?;
        Ok(Store { root })
    }

    pub(crate) fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn fsync_dir(dir: &Path) -> IdentityResult<()> {
        File::open(dir)
            .and_then(|d| d.sync_all())
            .map_err(|e| storage(format!("fsync dir {}: {e}", dir.display())))
    }

    /// Atomic durable write: temp file in the same directory, fsync file,
    /// rename to the final name, fsync the parent directory. The temp name
    /// is unique per call (process ID + counter) so two concurrent writers
    /// to the same document can never truncate or interleave each other's
    /// temp file; last rename wins whole.
    pub(crate) fn write_atomic(&self, rel: &str, bytes: &[u8]) -> IdentityResult<()> {
        static TEMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = self.path(rel);
        let parent = path
            .parent()
            .ok_or_else(|| storage(format!("no parent for {}", path.display())))?;
        fs::create_dir_all(parent).map_err(storage)?;
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| storage(format!("bad file name {}", path.display())))?;
        let seq = TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = parent.join(format!(".tmp-{}-{seq}-{file_name}", std::process::id()));
        let mut f = File::create(&tmp).map_err(storage)?;
        f.write_all(bytes).map_err(storage)?;
        f.sync_all().map_err(storage)?;
        drop(f);
        fs::rename(&tmp, &path).map_err(storage)?;
        Self::fsync_dir(parent)
    }

    pub(crate) fn write_doc(&self, rel: &str, doc: &Value) -> IdentityResult<()> {
        let bytes = serde_json::to_vec_pretty(doc).map_err(storage)?;
        self.write_atomic(rel, &bytes)
    }

    pub(crate) fn read_doc(&self, rel: &str) -> IdentityResult<Option<Value>> {
        let path = self.path(rel);
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| storage(format!("corrupt doc {}: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(storage(format!("reading {}: {e}", path.display()))),
        }
    }

    /// File names (not paths) of the regular files directly under `rel`.
    pub(crate) fn list(&self, rel: &str) -> IdentityResult<Vec<String>> {
        let dir = self.path(rel);
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(storage(format!("listing {}: {e}", dir.display()))),
        };
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(storage)?;
            if let Some(name) = entry.file_name().to_str()
                && !name.starts_with(".tmp-")
            {
                names.push(name.to_owned());
            }
        }
        names.sort();
        Ok(names)
    }

    /// Append one JSON record to `<rel>.jsonl`, fsynced before returning
    /// (audit ledger; append-only). The parent directory is fsynced too, so
    /// a ledger file created by this append cannot lose its directory entry
    /// on crash — the same acknowledgement rule `write_atomic` follows.
    pub(crate) fn append_jsonl(&self, rel: &str, record: &Value) -> IdentityResult<()> {
        let path = self.path(&format!("{rel}.jsonl"));
        let parent = path
            .parent()
            .ok_or_else(|| storage(format!("no parent for {}", path.display())))?
            .to_owned();
        fs::create_dir_all(&parent).map_err(storage)?;
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(storage)?;
        let mut line = serde_json::to_string(record).map_err(storage)?;
        line.push('\n');
        f.write_all(line.as_bytes()).map_err(storage)?;
        f.sync_all().map_err(storage)?;
        Self::fsync_dir(&parent)
    }

    pub(crate) fn read_jsonl(&self, rel: &str) -> IdentityResult<Vec<Value>> {
        let path = self.path(&format!("{rel}.jsonl"));
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(storage(format!("reading {}: {e}", path.display()))),
        };
        let mut out = Vec::new();
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            out.push(
                serde_json::from_str(line)
                    .map_err(|e| storage(format!("corrupt ledger line: {e}")))?,
            );
        }
        Ok(out)
    }
}

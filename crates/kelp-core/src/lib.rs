//! Shared immutable content and the transaction model used by the CLI/remotes.
//! The object hash envelope stays stable across network protocol versions.

pub mod content;
mod delta;
pub mod paths;
pub mod storage;
pub mod transactions;
pub mod transfer;

use std::collections::BTreeMap;

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const PROTOCOL: &str = "kelp/0";
pub const OBJECT_FORMAT: &str = "kelp/0";
pub const MAX_BLOB_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_METADATA_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FileEntry {
    pub blob: String,
    pub size: u64,
    pub executable: bool,
    #[serde(default, skip_serializing_if = "EntryKind::is_regular")]
    pub kind: EntryKind,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "kebab-case")]
pub enum EntryKind {
    #[default]
    Regular,
    Chunked,
    Symlink,
    SymlinkDirectory,
}

impl EntryKind {
    fn is_regular(&self) -> bool {
        *self == Self::Regular
    }
    pub fn is_symlink(self) -> bool {
        matches!(self, Self::Symlink | Self::SymlinkDirectory)
    }
}

impl FileEntry {
    pub fn validate(&self) -> Result<()> {
        validate_hash(&self.blob)?;
        ensure!(
            self.kind == EntryKind::Chunked || self.size <= MAX_BLOB_BYTES as u64,
            "unchunked file exceeds object size limit"
        );
        ensure!(
            !self.kind.is_symlink() || !self.executable,
            "symlinks cannot carry executable mode"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub files: BTreeMap<String, FileEntry>,
}

impl Snapshot {
    pub fn validate(&self) -> Result<()> {
        for (path, entry) in &self.files {
            validate_path(path)?;
            entry.validate()?;
            let mut parent = path.as_str();
            while let Some((prefix, _)) = parent.rsplit_once('/') {
                ensure!(
                    !self.files.contains_key(prefix),
                    "file/directory collision: {prefix}"
                );
                parent = prefix;
            }
        }
        Ok(())
    }

    pub fn id(&self) -> Result<String> {
        Ok(object_id("snapshot", &serde_json::to_vec(self)?))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Revision {
    pub project: String,
    pub change: String,
    pub predecessor: Option<String>,
    pub base_snapshot: String,
    pub result_snapshot: String,
    pub message: String,
}

impl Revision {
    pub fn validate(&self) -> Result<()> {
        validate_name(&self.project)?;
        validate_name(&self.change)?;
        validate_hash(&self.base_snapshot)?;
        validate_hash(&self.result_snapshot)?;
        if let Some(parent) = &self.predecessor {
            validate_hash(parent)?;
        }
        ensure!(!self.message.trim().is_empty(), "a change needs a message");
        ensure!(self.message.len() <= 16_384, "message exceeds 16 KiB");
        Ok(())
    }

    pub fn id(&self) -> Result<String> {
        Ok(object_id("revision", &serde_json::to_vec(self)?))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Publication {
    pub request_id: String,
    pub revision: Revision,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PublicationReceipt {
    pub request_id: String,
    pub change: String,
    pub revision: String,
    pub heads: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectInfo {
    pub project: String,
    pub protocol: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeInfo {
    pub change: String,
    pub heads: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ApiError {
    pub code: String,
    pub message: String,
}

/// Object type and length are hashed along with bytes to prevent type confusion.
pub fn object_id(kind: &str, bytes: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(format!("{OBJECT_FORMAT}\0{kind}\0{}\0", bytes.len()));
    hash.update(bytes);
    format!("{:x}", hash.finalize())
}

pub fn validate_hash(id: &str) -> Result<()> {
    ensure!(
        id.len() == 64
            && id
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),
        "invalid object ID: {id}"
    );
    Ok(())
}

pub fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 128
            && name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
        "names must contain 1–128 ASCII letters, digits, hyphens, or underscores"
    );
    Ok(())
}

/// Relative native file names, excluding traversal and metadata directories.
pub fn validate_path(path: &str) -> Result<()> {
    paths::validate(path)
}

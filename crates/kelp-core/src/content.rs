//! File payloads are either a single blob or a bounded tree of chunk manifests.
//! Manifests and chunks use the existing typed blob store and transfer protocol.
use std::io::{Read, Write};

use anyhow::{Result, ensure};
use rusqlite::Connection;

use crate::{EntryKind, FileEntry, MAX_BLOB_BYTES, storage};

const CHUNK_BYTES: usize = 4 * 1024 * 1024;
const FANOUT: usize = 1024;

pub fn store(
    db: &Connection,
    namespace: &str,
    mut input: impl Read,
    size: u64,
    executable: bool,
) -> Result<FileEntry> {
    let mut remaining = size;
    let mut parts = Vec::new();
    loop {
        let length = if size <= MAX_BLOB_BYTES as u64 {
            size as usize
        } else {
            remaining.min(CHUNK_BYTES as u64) as usize
        };
        let mut bytes = vec![0; length];
        input.read_exact(&mut bytes)?;
        parts.push(FileEntry {
            blob: storage::put(db, namespace, "blob", &bytes)?,
            size: length as u64,
            executable: false,
            kind: EntryKind::Regular,
        });
        remaining -= length as u64;
        if remaining == 0 {
            break;
        }
    }
    ensure!(
        input.read(&mut [0])? == 0,
        "file changed size while being read; try again"
    );
    while parts.len() > 1 {
        parts = parts
            .chunks(FANOUT)
            .map(|children| {
                if children.len() == 1 {
                    return Ok(children[0].clone());
                }
                Ok(FileEntry {
                    blob: storage::put_json(db, namespace, "blob", &children)?,
                    size: children.iter().map(|child| child.size).sum(),
                    executable: false,
                    kind: EntryKind::Chunked,
                })
            })
            .collect::<Result<_>>()?;
    }
    let mut entry = parts.pop().unwrap();
    entry.executable = executable;
    Ok(entry)
}

/// Validate a manifest before trusting its references. Children strictly shrink,
/// so a malformed manifest cannot create a recursive content cycle.
pub fn children(entry: &FileEntry, bytes: &[u8]) -> Result<Vec<FileEntry>> {
    entry.validate()?;
    ensure!(
        entry.kind == EntryKind::Chunked,
        "expected a chunk manifest"
    );
    ensure!(bytes.len() <= MAX_BLOB_BYTES, "oversized chunk manifest");
    let children: Vec<FileEntry> = serde_json::from_slice(bytes)?;
    ensure!(
        (2..=FANOUT).contains(&children.len()),
        "invalid chunk manifest fanout"
    );
    let mut size = 0_u64;
    for child in &children {
        child.validate()?;
        ensure!(
            !child.kind.is_symlink()
                && !child.executable
                && child.size > 0
                && child.size < entry.size,
            "invalid chunk child"
        );
        size = size
            .checked_add(child.size)
            .ok_or_else(|| anyhow::anyhow!("chunk size overflow"))?;
    }
    ensure!(size == entry.size, "chunk manifest size mismatch");
    Ok(children)
}

pub fn write(
    db: &Connection,
    namespace: &str,
    entry: &FileEntry,
    output: &mut impl Write,
) -> Result<()> {
    let mut todo = vec![entry.clone()];
    while let Some(entry) = todo.pop() {
        entry.validate()?;
        let bytes = storage::get(db, namespace, "blob", &entry.blob)?;
        if entry.kind == EntryKind::Chunked {
            todo.extend(children(&entry, &bytes)?.into_iter().rev());
        } else {
            ensure!(
                bytes.len() as u64 == entry.size,
                "file content size mismatch"
            );
            output.write_all(&bytes)?;
        }
    }
    Ok(())
}

pub fn read(db: &Connection, namespace: &str, entry: &FileEntry) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    write(db, namespace, entry, &mut bytes)?;
    Ok(bytes)
}

pub fn objects(db: &Connection, namespace: &str, entry: &FileEntry) -> Result<Vec<String>> {
    let mut todo = vec![entry.clone()];
    let mut objects = std::collections::BTreeSet::new();
    while let Some(entry) = todo.pop() {
        if !objects.insert(entry.blob.clone()) {
            continue;
        }
        if entry.kind == EntryKind::Chunked {
            todo.extend(children(
                &entry,
                &storage::get(db, namespace, "blob", &entry.blob)?,
            )?);
        }
    }
    Ok(objects.into_iter().collect())
}

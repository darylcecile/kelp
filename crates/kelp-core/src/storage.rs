//! Small SQLite-backed content store. Callers own metadata transactions.

use std::{path::Path, time::Duration};

use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Serialize, de::DeserializeOwned};

use crate::{Snapshot, object_id, validate_hash};

pub fn open(path: &Path) -> Result<Connection> {
    let db = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
    db.busy_timeout(Duration::from_secs(5))?;
    db.pragma_update(None, "journal_mode", "WAL")?;
    db.pragma_update(None, "synchronous", "FULL")?;
    db.pragma_update(None, "foreign_keys", "ON")?;
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS objects (
            namespace TEXT NOT NULL,
            kind TEXT NOT NULL,
            id TEXT NOT NULL,
            bytes BLOB NOT NULL,
            PRIMARY KEY (namespace, kind, id)
        );",
    )?;
    Ok(db)
}

pub fn put(db: &Connection, namespace: &str, kind: &str, bytes: &[u8]) -> Result<String> {
    let id = object_id(kind, bytes);
    db.execute(
        "INSERT OR IGNORE INTO objects (namespace, kind, id, bytes) VALUES (?1, ?2, ?3, ?4)",
        params![namespace, kind, id, bytes],
    )?;
    Ok(id)
}

pub fn get(db: &Connection, namespace: &str, kind: &str, id: &str) -> Result<Vec<u8>> {
    validate_hash(id)?;
    let bytes: Option<Vec<u8>> = db
        .query_row(
            "SELECT bytes FROM objects WHERE namespace = ?1 AND kind = ?2 AND id = ?3",
            params![namespace, kind, id],
            |row| row.get(0),
        )
        .optional()?;
    let bytes = bytes.with_context(|| format!("missing {kind} object {id}"))?;
    ensure!(object_id(kind, &bytes) == id, "corrupt {kind} object {id}");
    Ok(bytes)
}

pub fn contains(db: &Connection, namespace: &str, kind: &str, id: &str) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM objects WHERE namespace = ?1 AND kind = ?2 AND id = ?3)",
        params![namespace, kind, id],
        |row| row.get(0),
    )?)
}

pub fn put_json<T: Serialize>(
    db: &Connection,
    namespace: &str,
    kind: &str,
    value: &T,
) -> Result<String> {
    put(db, namespace, kind, &serde_json::to_vec(value)?)
}

pub fn get_json<T: DeserializeOwned>(
    db: &Connection,
    namespace: &str,
    kind: &str,
    id: &str,
) -> Result<T> {
    Ok(serde_json::from_slice(&get(db, namespace, kind, id)?)?)
}

pub fn verify_snapshot(db: &Connection, namespace: &str, snapshot: &Snapshot) -> Result<()> {
    snapshot.validate()?;
    for (path, file) in &snapshot.files {
        let bytes = get(db, namespace, "blob", &file.blob)?;
        ensure!(bytes.len() as u64 == file.size, "size mismatch for {path}");
    }
    Ok(())
}

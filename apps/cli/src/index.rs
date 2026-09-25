//! Rebuildable current-file frontier and filesystem content cache. Neither cache
//! is user history; immutable transactions and snapshots remain authoritative.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Result;
use kelp_core::{
    FileEntry, object_id, storage,
    transactions::{Transaction, View},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Projection {
    pub view: View,
    pub roots: BTreeSet<String>,
}

impl Projection {
    pub fn apply(&mut self, id: &str, transaction: &Transaction) {
        self.view.apply(id, transaction);
        for dependency in transaction.dependencies() {
            self.roots.remove(&dependency);
        }
        self.roots.insert(id.into());
    }

    pub fn id(&self) -> Result<String> {
        Ok(object_id("view", &serde_json::to_vec(&self.roots)?))
    }
}

pub fn setup(db: &Connection, namespace: &str) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS projection_cache (
        id INTEGER PRIMARY KEY CHECK(id = 1), source TEXT NOT NULL, checksum TEXT NOT NULL, payload BLOB NOT NULL
    );
    CREATE TABLE IF NOT EXISTS file_cache_pages (
        page INTEGER PRIMARY KEY, payload BLOB NOT NULL
    );
    DROP INDEX IF EXISTS checkpoints_snapshot;")?;
    let legacy: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='file_cache' AND type='table')",
        [],
        |row| row.get(0),
    )?;
    if legacy {
        let tx = db.unchecked_transaction()?;
        let rows = tx
            .prepare("SELECT path,fingerprint,captured_at,entry FROM file_cache")?
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let entries = rows
            .into_iter()
            .filter_map(|(path, fingerprint, time, entry)| {
                Some((
                    path,
                    CachedFile {
                        fingerprint,
                        captured_at: time.parse().ok()?,
                        entry: serde_json::from_str(&entry).ok()?,
                    },
                ))
            })
            .collect();
        save_files(&tx, namespace, entries)?;
        tx.execute("DROP TABLE file_cache", [])?;
        tx.commit()?;
    }
    Ok(())
}

fn source(ids: &BTreeSet<String>) -> Result<String> {
    Ok(object_id("membership", &serde_json::to_vec(ids)?))
}

pub fn load(
    db: &Connection,
    namespace: &str,
    ids: &BTreeSet<String>,
) -> Result<Option<Projection>> {
    let record: Option<(String, String, Vec<u8>)> = db
        .query_row(
            "SELECT source, checksum, payload FROM projection_cache WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((cached_source, checksum, encoded)) = record else {
        return Ok(None);
    };
    let Ok(bytes) = storage::decode_cache(db, namespace, &encoded) else {
        return Ok(None);
    };
    if cached_source != source(ids)? || object_id("projection-cache", &bytes) != checksum {
        return Ok(None);
    }
    Ok(serde_json::from_slice(&bytes).ok())
}

pub fn save(db: &Connection, ids: &BTreeSet<String>, projection: &Projection) -> Result<()> {
    let payload = serde_json::to_vec(projection)?;
    db.execute("INSERT INTO projection_cache(id, source, checksum, payload) VALUES(1, ?1, ?2, ?3)
        ON CONFLICT(id) DO UPDATE SET source=excluded.source, checksum=excluded.checksum, payload=excluded.payload",
        params![source(ids)?, object_id("projection-cache", &payload), storage::encode_record(&payload)?])?;
    Ok(())
}

#[derive(Serialize, Deserialize)]
pub struct CachedFile {
    pub fingerprint: String,
    pub captured_at: u128,
    pub entry: FileEntry,
}

pub fn files(db: &Connection, namespace: &str) -> Result<BTreeMap<String, CachedFile>> {
    let mut statement = db.prepare("SELECT payload FROM file_cache_pages")?;
    let rows = statement.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
    let mut files = BTreeMap::new();
    let mut reader = storage::MetadataReader::new(db, namespace);
    for row in rows {
        if let Ok(bytes) = reader.decode(&row?)
            && let Ok(page) = serde_json::from_slice::<BTreeMap<String, CachedFile>>(&bytes)
        {
            files.extend(page);
        }
    }
    Ok(files)
}

pub fn save_files(
    db: &Connection,
    namespace: &str,
    entries: BTreeMap<String, CachedFile>,
) -> Result<()> {
    let mut groups: BTreeMap<u8, BTreeMap<String, CachedFile>> = BTreeMap::new();
    for (path, entry) in entries {
        let key = u8::from_str_radix(&object_id("cache-page", path.as_bytes())[..2], 16)? % 16;
        groups.entry(key).or_default().insert(path, entry);
    }
    let mut reader = storage::MetadataReader::new(db, namespace);
    for (key, entries) in groups {
        let encoded: Option<Vec<u8>> = db
            .query_row(
                "SELECT payload FROM file_cache_pages WHERE page=?1",
                [key],
                |row| row.get(0),
            )
            .optional()?;
        let mut page: BTreeMap<String, CachedFile> = encoded
            .and_then(|bytes| reader.decode(&bytes).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        page.extend(entries);
        db.execute("INSERT INTO file_cache_pages(page,payload) VALUES(?1,?2) ON CONFLICT(page) DO UPDATE SET payload=excluded.payload",
        params![key,storage::encode_record(&serde_json::to_vec(&page)?)?])?;
    }
    Ok(())
}

pub fn compact(db: &Connection, namespace: &str, base_id: &str) -> Result<()> {
    let base = storage::get(db, namespace, "snapshot", base_id)?;
    let tx = db.unchecked_transaction()?;
    let projection: Option<Vec<u8>> = tx
        .query_row(
            "SELECT payload FROM projection_cache WHERE id=1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(projection) = projection {
        let bytes = storage::decode_cache(&tx, namespace, &projection)?;
        tx.execute(
            "UPDATE projection_cache SET payload=?1 WHERE id=1",
            [storage::encode_cache(&bytes, base_id, &base)?],
        )?;
    }
    let pages = tx
        .prepare("SELECT page,payload FROM file_cache_pages")?
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for (page, payload) in pages {
        let bytes = storage::decode_cache(&tx, namespace, &payload)?;
        tx.execute(
            "UPDATE file_cache_pages SET payload=?1 WHERE page=?2",
            params![storage::encode_cache(&bytes, base_id, &base)?, page],
        )?;
    }
    tx.commit()?;
    Ok(())
}

pub fn timestamp() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

/// ctime catches same-size writes even if a tool restores the old mtime. Inode
/// and device detect replacements. A racy timestamp is always read again.
#[cfg(unix)]
pub fn fingerprint(metadata: &fs::Metadata) -> Option<(String, u128)> {
    use std::os::unix::fs::MetadataExt;
    let modified = u128::try_from(metadata.mtime()).ok()? * 1_000_000_000
        + u128::try_from(metadata.mtime_nsec()).ok()?;
    let changed = u128::try_from(metadata.ctime()).ok()? * 1_000_000_000
        + u128::try_from(metadata.ctime_nsec()).ok()?;
    Some((
        format!(
            "{}:{}:{}:{}:{}:{}",
            metadata.dev(),
            metadata.ino(),
            metadata.size(),
            metadata.mode(),
            modified,
            changed
        ),
        modified.max(changed),
    ))
}

// Stable std Windows metadata does not expose a reliable change counter.
#[cfg(not(unix))]
pub fn fingerprint(_: &fs::Metadata) -> Option<(String, u128)> {
    None
}

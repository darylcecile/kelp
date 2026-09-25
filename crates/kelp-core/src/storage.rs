//! Content-addressed storage with compact binary indexes and bounded packs.
//! Packing is entirely physical: logical object bytes and IDs never change.

use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::Path,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Serialize, de::DeserializeOwned};

use crate::{Snapshot, object_id, validate_hash};

const MAX_PACK_BYTES: usize = 8 * 1024 * 1024;
const MAX_GROUP_BYTES: usize = 16 * 1024 * 1024;
const MAX_PACK_OBJECTS: usize = 256;

type PackCache = VecDeque<(Vec<u8>, Arc<Vec<u8>>)>;
type ObjectLocation = (i64, Option<i64>, Option<i64>, Option<i64>, i64, Option<i64>);
thread_local! { static PACK_CACHE: RefCell<PackCache> = const { RefCell::new(VecDeque::new()) }; }

#[derive(Debug, Serialize)]
pub struct Compaction {
    pub objects: usize,
    pub packs: usize,
    pub payload_before: u64,
    pub payload_after: u64,
}

pub fn open(path: &Path) -> Result<Connection> {
    let db = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
    db.busy_timeout(Duration::from_secs(5))?;
    db.pragma_update(None, "journal_mode", "WAL")?;
    db.pragma_update(None, "synchronous", "FULL")?;
    db.pragma_update(None, "foreign_keys", "ON")?;
    let initialized: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='object_index' AND type='table')",
        [],
        |row| row.get(0),
    )?;
    if !initialized {
        let tx = db.unchecked_transaction()?;
        tx.execute_batch("CREATE TABLE IF NOT EXISTS storage_namespaces (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE);
            CREATE TABLE IF NOT EXISTS storage_kinds (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE);
            CREATE TABLE IF NOT EXISTS loose_objects (id INTEGER PRIMARY KEY, bytes BLOB NOT NULL, codec INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS object_packs (id INTEGER PRIMARY KEY, hash BLOB NOT NULL UNIQUE, bytes BLOB NOT NULL, raw_size INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS object_index (
                namespace INTEGER NOT NULL REFERENCES storage_namespaces(id),
                kind INTEGER NOT NULL REFERENCES storage_kinds(id),
                hash BLOB NOT NULL CHECK(length(hash)=32),
                raw_size INTEGER NOT NULL,
                sequence INTEGER NOT NULL,
                loose INTEGER REFERENCES loose_objects(id),
                pack INTEGER REFERENCES object_packs(id),
                offset INTEGER,
                encoding INTEGER NOT NULL DEFAULT 0,
                stored_size INTEGER,
                PRIMARY KEY(namespace,kind,hash),
                CHECK((loose IS NOT NULL AND pack IS NULL AND offset IS NULL) OR (loose IS NULL AND pack IS NOT NULL AND offset IS NOT NULL))
            ) WITHOUT ROWID;")?;
        let legacy: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='objects' AND type='table')",
            [],
            |row| row.get(0),
        )?;
        if legacy {
            let columns = tx
                .prepare("PRAGMA table_info(objects)")?
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<Result<Vec<_>, _>>()?;
            let sql = if columns.iter().any(|column| column == "codec") {
                "SELECT namespace,kind,id,bytes,codec,raw_size FROM objects ORDER BY rowid"
            } else {
                "SELECT namespace,kind,id,bytes,0,NULL FROM objects ORDER BY rowid"
            };
            let mut statement = tx.prepare(sql)?;
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                let (namespace, kind, id, encoded, codec, size): (
                    String,
                    String,
                    String,
                    Vec<u8>,
                    i64,
                    Option<i64>,
                ) = (
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                );
                let bytes = decode(&encoded, codec, size)?;
                ensure!(object_id(&kind, &bytes) == id, "corrupt legacy object {id}");
                put(&tx, &namespace, &kind, &bytes)?;
            }
            drop(rows);
            drop(statement);
            tx.execute("DROP TABLE objects", [])?;
        }
        tx.commit()?;
    }
    let columns = db
        .prepare("PRAGMA table_info(object_index)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    if !columns.iter().any(|name| name == "encoding") {
        db.execute_batch("ALTER TABLE object_index ADD COLUMN encoding INTEGER NOT NULL DEFAULT 0; ALTER TABLE object_index ADD COLUMN stored_size INTEGER;")?;
    }
    Ok(db)
}

pub fn hash_bytes(id: &str) -> Result<Vec<u8>> {
    validate_hash(id)?;
    (0..64)
        .step_by(2)
        .map(|offset| Ok(u8::from_str_radix(&id[offset..offset + 2], 16)?))
        .collect()
}

fn names(db: &Connection, namespace: &str, kind: &str) -> Result<(i64, i64)> {
    db.execute(
        "INSERT OR IGNORE INTO storage_namespaces(name) VALUES(?1)",
        [namespace],
    )?;
    db.execute(
        "INSERT OR IGNORE INTO storage_kinds(name) VALUES(?1)",
        [kind],
    )?;
    Ok((
        db.query_row(
            "SELECT id FROM storage_namespaces WHERE name=?1",
            [namespace],
            |row| row.get(0),
        )?,
        db.query_row(
            "SELECT id FROM storage_kinds WHERE name=?1",
            [kind],
            |row| row.get(0),
        )?,
    ))
}

pub fn put(db: &Connection, namespace: &str, kind: &str, bytes: &[u8]) -> Result<String> {
    let id = object_id(kind, bytes);
    if contains(db, namespace, kind, &id)? {
        return Ok(id);
    }
    let tx = if db.is_autocommit() {
        Some(db.unchecked_transaction()?)
    } else {
        None
    };
    let (namespace, kind) = names(db, namespace, kind)?;
    let compressed = if bytes.len() >= 512 {
        zstd::bulk::compress(bytes, 1)?
    } else {
        Vec::new()
    };
    let (codec, encoded) = if !compressed.is_empty() && compressed.len() + 16 < bytes.len() {
        (1, compressed.as_slice())
    } else {
        (0, bytes)
    };
    db.execute(
        "INSERT INTO loose_objects(bytes,codec) VALUES(?1,?2)",
        params![encoded, codec],
    )?;
    let loose = db.last_insert_rowid();
    let inserted=db.execute("INSERT OR IGNORE INTO object_index(namespace,kind,hash,raw_size,loose,sequence) VALUES(?1,?2,?3,?4,?5,?5)",
        params![namespace,kind,hash_bytes(&id)?,bytes.len() as i64,loose])?;
    if inserted == 0 {
        db.execute("DELETE FROM loose_objects WHERE id=?1", [loose])?;
    }
    if let Some(tx) = tx {
        tx.commit()?;
    }
    Ok(id)
}

fn decode(encoded: &[u8], codec: i64, size: Option<i64>) -> Result<Vec<u8>> {
    let bytes = match codec {
        0 => encoded.to_vec(),
        1 => zstd::bulk::decompress(
            encoded,
            usize::try_from(size.context("compressed object has no length")?)?,
        )?,
        _ => anyhow::bail!("unknown storage codec {codec}"),
    };
    if let Some(size) = size {
        ensure!(size == bytes.len() as i64, "corrupt object length");
    }
    Ok(bytes)
}

fn unpack(db: &Connection, id: i64) -> Result<Arc<Vec<u8>>> {
    let (hash, size): (Vec<u8>, i64) = db.query_row(
        "SELECT hash,raw_size FROM object_packs WHERE id=?1",
        [id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if let Some(bytes) = PACK_CACHE.with(|cache| {
        cache
            .borrow()
            .iter()
            .find(|(key, _)| *key == hash)
            .map(|(_, bytes)| bytes.clone())
    }) {
        return Ok(bytes);
    }
    let encoded: Vec<u8> =
        db.query_row("SELECT bytes FROM object_packs WHERE id=?1", [id], |row| {
            row.get(0)
        })?;
    ensure!(
        (0..=MAX_PACK_BYTES as i64).contains(&size),
        "invalid pack size"
    );
    let bytes = decode(&encoded, 1, Some(size))?;
    ensure!(
        hash_bytes(&object_id("storage-pack", &bytes))? == hash,
        "corrupt storage pack"
    );
    let bytes = Arc::new(bytes);
    PACK_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.len() == 2 {
            cache.pop_front();
        }
        cache.push_back((hash, bytes.clone()));
    });
    Ok(bytes)
}

pub fn get(db: &Connection, namespace: &str, kind: &str, id: &str) -> Result<Vec<u8>> {
    let record: Option<ObjectLocation>=db.query_row(
        "SELECT raw_size,loose,pack,offset,encoding,stored_size FROM object_index WHERE namespace=(SELECT id FROM storage_namespaces WHERE name=?1)
         AND kind=(SELECT id FROM storage_kinds WHERE name=?2) AND hash=?3",params![namespace,kind,hash_bytes(id)?],
        |row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?))).optional()?;
    let (size, loose, pack, offset, encoding, stored_size) =
        record.with_context(|| format!("missing {kind} object {id}"))?;
    let bytes = if let Some(loose) = loose {
        let (encoded, codec): (Vec<u8>, i64) = db.query_row(
            "SELECT bytes,codec FROM loose_objects WHERE id=?1",
            [loose],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        decode(&encoded, codec, Some(size))?
    } else {
        ensure!(
            (0..=MAX_PACK_BYTES as i64).contains(&size),
            "invalid packed object size"
        );
        let pack = unpack(db, pack.context("object has no storage location")?)?;
        let offset = usize::try_from(offset.context("packed object has no offset")?)?;
        let end = offset
            .checked_add(usize::try_from(stored_size.unwrap_or(size))?)
            .context("packed object bounds overflow")?;
        let encoded = pack
            .get(offset..end)
            .context("packed object outside pack")?;
        match encoding {
            0 => encoded.to_vec(),
            2 => {
                ensure!(
                    pack.starts_with(b"KDP1") && pack.len() >= 8,
                    "invalid delta pack"
                );
                let length = u32::from_be_bytes(pack[4..8].try_into()?) as usize;
                let base = pack.get(8..8 + length).context("invalid pack-local base")?;
                crate::delta::decode(base, encoded, usize::try_from(size)?)?
            }
            _ => anyhow::bail!("unknown packed encoding"),
        }
    };
    ensure!(bytes.len() as i64 == size, "corrupt logical object length");
    ensure!(object_id(kind, &bytes) == id, "corrupt {kind} object {id}");
    Ok(bytes)
}

pub fn size(db: &Connection, namespace: &str, kind: &str, id: &str) -> Result<Option<u64>> {
    let value: Option<i64>=db.query_row("SELECT raw_size FROM object_index WHERE namespace=(SELECT id FROM storage_namespaces WHERE name=?1)
        AND kind=(SELECT id FROM storage_kinds WHERE name=?2) AND hash=?3",params![namespace,kind,hash_bytes(id)?],|row|row.get(0)).optional()?;
    value.map(|size| Ok(u64::try_from(size)?)).transpose()
}

pub fn contains(db: &Connection, namespace: &str, kind: &str, id: &str) -> Result<bool> {
    Ok(size(db, namespace, kind, id)?.is_some())
}

pub fn count(db: &Connection, namespace: &str, kind: &str) -> Result<i64> {
    Ok(db.query_row("SELECT count(*) FROM object_index WHERE namespace=(SELECT id FROM storage_namespaces WHERE name=?1)
        AND kind=(SELECT id FROM storage_kinds WHERE name=?2)",[namespace,kind],|row|row.get(0))?)
}

pub fn rename_namespace(db: &Connection, old: &str, new: &str) -> Result<()> {
    db.execute(
        "UPDATE storage_namespaces SET name=?1 WHERE name=?2",
        [new, old],
    )?;
    Ok(())
}

pub fn find(
    db: &Connection,
    namespace: &str,
    kind: Option<&str>,
    prefix: &str,
) -> Result<Vec<(String, String)>> {
    let low = format!("{prefix:0<64}");
    let high = format!("{prefix:f<64}");
    let mut statement=db.prepare("SELECT k.name,lower(hex(o.hash)) FROM object_index o JOIN storage_kinds k ON k.id=o.kind
        WHERE namespace=(SELECT id FROM storage_namespaces WHERE name=?1) AND k.name IN ('saved-view','transaction')
        AND (?2 IS NULL OR k.name=?2) AND hash>=?3 AND hash<=?4 ORDER BY hash LIMIT 2")?;
    Ok(statement
        .query_map(
            params![namespace, kind, hash_bytes(&low)?, hash_bytes(&high)?],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?
        .collect::<Result<_, _>>()?)
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
        ensure!(
            get(db, namespace, "blob", &file.blob)?.len() as u64 == file.size,
            "size mismatch for {path}"
        );
    }
    Ok(())
}

/// Compact mutable metadata without changing its logical serialization.
pub fn encode_record(bytes: &[u8]) -> Result<Vec<u8>> {
    let compressed = zstd::bulk::compress(bytes, 1)?;
    if compressed.len() + 12 >= bytes.len() {
        return Ok(bytes.to_vec());
    }
    let mut encoded = b"KZ01".to_vec();
    encoded.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
    encoded.extend_from_slice(&compressed);
    Ok(encoded)
}

pub fn decode_record(bytes: &[u8]) -> Result<Vec<u8>> {
    if !bytes.starts_with(b"KZ01") {
        return Ok(bytes.to_vec());
    }
    ensure!(bytes.len() >= 12, "truncated metadata record");
    let length = usize::try_from(u64::from_be_bytes(bytes[4..12].try_into()?))?;
    let decoded = zstd::bulk::decompress(&bytes[12..], length)?;
    ensure!(decoded.len() == length, "metadata length mismatch");
    Ok(decoded)
}

/// Reuse a local immutable snapshot as a dictionary for rebuildable metadata.
/// The dictionary never becomes a version-control or cross-node dependency.
pub fn encode_cache(bytes: &[u8], base_id: &str, base: &[u8]) -> Result<Vec<u8>> {
    let normal = encode_record(bytes)?;
    let delta = crate::delta::Dictionary::new(base).encode(bytes);
    let compressed = zstd::bulk::compress(&delta, 3)?;
    if compressed.len() + 44 >= normal.len() {
        return Ok(normal);
    }
    let mut output = b"KCD1".to_vec();
    output.extend_from_slice(&hash_bytes(base_id)?);
    output.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
    output.extend_from_slice(&compressed);
    Ok(output)
}

pub fn decode_cache(db: &Connection, namespace: &str, bytes: &[u8]) -> Result<Vec<u8>> {
    MetadataReader::new(db, namespace).decode(bytes)
}

pub struct MetadataReader<'a> {
    db: &'a Connection,
    namespace: &'a str,
    bases: BTreeMap<String, Vec<u8>>,
}

impl<'a> MetadataReader<'a> {
    pub fn new(db: &'a Connection, namespace: &'a str) -> Self {
        Self {
            db,
            namespace,
            bases: BTreeMap::new(),
        }
    }

    pub fn decode(&mut self, bytes: &[u8]) -> Result<Vec<u8>> {
        if !bytes.starts_with(b"KCD1") {
            return decode_record(bytes);
        }
        ensure!(bytes.len() >= 44, "truncated cache record");
        let id = bytes[4..36]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let length = usize::try_from(u64::from_be_bytes(bytes[36..44].try_into()?))?;
        let budget = length
            .checked_mul(2)
            .and_then(|length| length.checked_add(64))
            .context("cache length overflow")?;
        let delta = zstd::bulk::decompress(&bytes[44..], budget)?;
        if !self.bases.contains_key(&id) {
            self.bases
                .insert(id.clone(), get(self.db, self.namespace, "snapshot", &id)?);
        }
        crate::delta::decode(&self.bases[&id], &delta, length)
    }
}

/// Compact every object, without deleting logical history. Packs are bounded,
/// self-contained, and never cross a namespace or storage database.
pub fn compact(db: &Connection, keep_loose: &BTreeSet<String>) -> Result<Compaction> {
    ensure!(
        db.is_autocommit(),
        "finish the current transaction before compaction"
    );
    let before = payload_bytes(db)?;
    let tx = db.unchecked_transaction()?;
    let records = tx
        .prepare(
            "SELECT n.name,k.name,lower(hex(o.hash)),raw_size FROM object_index o
        JOIN storage_namespaces n ON n.id=o.namespace JOIN storage_kinds k ON k.id=o.kind
        ORDER BY o.namespace,o.kind,o.sequence,o.hash",
        )?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut group = Vec::new();
    let mut bytes = Vec::new();
    let mut namespace = String::new();
    let mut kind = String::new();
    let mut packs = 0;
    for (space, object_kind, id, size) in &records {
        if group.len() == MAX_PACK_OBJECTS
            || bytes.len() + usize::try_from(*size)? > MAX_GROUP_BYTES
            || namespace != *space
            || kind != *object_kind
        {
            packs += pack_group(&tx, &namespace, &kind, &group, &bytes)?;
            group.clear();
            bytes.clear();
        }
        namespace = space.clone();
        kind = object_kind.clone();
        if keep_loose.contains(id) {
            let object = get(&tx, space, object_kind, id)?;
            let compressed = zstd::bulk::compress(&object, 1)?;
            let (codec, payload) = if compressed.len() < object.len() {
                (1, compressed.as_slice())
            } else {
                (0, object.as_slice())
            };
            tx.execute(
                "INSERT INTO loose_objects(bytes,codec) VALUES(?1,?2)",
                params![payload, codec],
            )?;
            let loose = tx.last_insert_rowid();
            tx.execute("UPDATE object_index SET loose=?1,pack=NULL,offset=NULL,encoding=0,stored_size=NULL WHERE namespace=(SELECT id FROM storage_namespaces WHERE name=?2)
                AND kind=(SELECT id FROM storage_kinds WHERE name=?3) AND hash=?4",params![loose,space,object_kind,hash_bytes(id)?])?;
            continue;
        }
        if *size > MAX_PACK_BYTES as i64 {
            continue;
        }
        let object = get(&tx, space, object_kind, id)?;
        group.push((id.clone(), bytes.len()));
        bytes.extend_from_slice(&object);
    }
    packs += pack_group(&tx, &namespace, &kind, &group, &bytes)?;
    tx.execute("DELETE FROM loose_objects WHERE id NOT IN (SELECT loose FROM object_index WHERE loose IS NOT NULL)",[])?;
    tx.execute("DELETE FROM object_packs WHERE id NOT IN (SELECT pack FROM object_index WHERE pack IS NOT NULL)",[])?;
    let after = payload_bytes(&tx)?;
    tx.commit()?;
    // VACUUM rewrites live pages only; it does not remove an object or version.
    db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM; PRAGMA wal_checkpoint(TRUNCATE);")?;
    Ok(Compaction {
        objects: records.len(),
        packs,
        payload_before: before,
        payload_after: after,
    })
}

fn pack_group(
    db: &Connection,
    namespace: &str,
    kind: &str,
    group: &[(String, usize)],
    bytes: &[u8],
) -> Result<usize> {
    if group.len() < 2 {
        return Ok(0);
    }
    let base = &bytes[group.last().expect("nonempty group").1..];
    let dictionary = crate::delta::Dictionary::new(base);
    let mut delta_bytes = b"KDP1".to_vec();
    delta_bytes.extend_from_slice(&(base.len() as u32).to_be_bytes());
    delta_bytes.extend_from_slice(base);
    let mut locations = Vec::new();
    for (index, (id, offset)) in group.iter().enumerate() {
        let end = group
            .get(index + 1)
            .map(|(_, offset)| *offset)
            .unwrap_or(bytes.len());
        let original = &bytes[*offset..end];
        if index + 1 == group.len() {
            locations.push((id, 8, original.len(), 0));
            continue;
        }
        let delta = dictionary.encode(original);
        let (encoded, codec) = if delta.len() < original.len() {
            (delta.as_slice(), 2)
        } else {
            (original, 0)
        };
        if codec == 2 {
            ensure!(
                crate::delta::decode(base, encoded, original.len())? == original,
                "delta verification failed"
            );
        }
        locations.push((id, delta_bytes.len(), encoded.len(), codec));
        delta_bytes.extend_from_slice(encoded);
    }
    if bytes.len() > MAX_PACK_BYTES && delta_bytes.len() > MAX_PACK_BYTES {
        let middle = group.len() / 2;
        let boundary = group[middle].1;
        let tail: Vec<_> = group[middle..]
            .iter()
            .map(|(id, offset)| (id.clone(), offset - boundary))
            .collect();
        return Ok(
            pack_group(db, namespace, kind, &group[..middle], &bytes[..boundary])?
                + pack_group(db, namespace, kind, &tail, &bytes[boundary..])?,
        );
    }
    let plain_packed = if bytes.len() <= MAX_PACK_BYTES {
        zstd::bulk::compress(bytes, 9)?
    } else {
        Vec::new()
    };
    let delta_packed = if delta_bytes.len() <= MAX_PACK_BYTES {
        zstd::bulk::compress(&delta_bytes, 9)?
    } else {
        Vec::new()
    };
    let (payload, packed) = if !delta_packed.is_empty()
        && (plain_packed.is_empty() || delta_packed.len() < plain_packed.len())
    {
        (delta_bytes.as_slice(), delta_packed)
    } else {
        locations = group
            .iter()
            .enumerate()
            .map(|(index, (id, offset))| {
                let end = group
                    .get(index + 1)
                    .map(|(_, offset)| *offset)
                    .unwrap_or(bytes.len());
                (id, *offset, end - *offset, 0)
            })
            .collect();
        (bytes, plain_packed)
    };
    ensure!(
        zstd::bulk::decompress(&packed, payload.len())? == payload,
        "pack verification failed"
    );
    let hash = hash_bytes(&object_id("storage-pack", payload))?;
    db.execute(
        "INSERT OR IGNORE INTO object_packs(hash,bytes,raw_size) VALUES(?1,?2,?3)",
        params![hash, packed, payload.len() as i64],
    )?;
    let pack: i64 = db.query_row("SELECT id FROM object_packs WHERE hash=?1", [hash], |row| {
        row.get(0)
    })?;
    for (id, offset, length, encoding) in locations {
        db.execute("UPDATE object_index SET loose=NULL,pack=?1,offset=?2,stored_size=?6,encoding=?7 WHERE namespace=(SELECT id FROM storage_namespaces WHERE name=?3)
            AND kind=(SELECT id FROM storage_kinds WHERE name=?4) AND hash=?5",params![pack,offset as i64,namespace,kind,hash_bytes(id)?,length as i64,encoding])?;
    }
    Ok(1)
}

fn payload_bytes(db: &Connection) -> Result<u64> {
    Ok(db.query_row("SELECT COALESCE((SELECT sum(length(bytes)) FROM loose_objects),0)+COALESCE((SELECT sum(length(bytes)) FROM object_packs),0)",[],|row|row.get::<_,i64>(0))? as u64)
}

//! Bounded, compressed object batches. Each object is still verified and stored
//! by its original content hash; batching does not affect transaction atomicity.
use std::io::{Cursor, Read};

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

use crate::{MAX_BLOB_BYTES, MAX_METADATA_BYTES, object_id, validate_hash};

pub const MAX_OBJECTS: usize = 128;
pub const MAX_PACK_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Key {
    pub kind: String,
    pub id: String,
}

impl Key {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            matches!(self.kind.as_str(), "blob" | "transaction"),
            "unsupported batch object kind"
        );
        validate_hash(&self.id)
    }
    pub fn limit(&self) -> usize {
        if self.kind == "blob" {
            MAX_BLOB_BYTES
        } else {
            MAX_METADATA_BYTES
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub objects: Vec<Key>,
}

impl Request {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.objects.len() <= MAX_OBJECTS,
            "object batch exceeds {MAX_OBJECTS} items"
        );
        let mut unique = std::collections::BTreeSet::new();
        for key in &self.objects {
            key.validate()?;
            ensure!(unique.insert(key), "duplicate object in batch");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Info {
    pub object: Key,
    pub size: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct Object {
    pub key: Key,
    pub bytes: Vec<u8>,
}

pub fn encode(objects: &[Object]) -> Result<Vec<u8>> {
    Request {
        objects: objects.iter().map(|object| object.key.clone()).collect(),
    }
    .validate()?;
    let mut raw = b"KLP1".to_vec();
    for object in objects {
        ensure!(
            object.bytes.len() <= object.key.limit(),
            "object exceeds type size limit"
        );
        ensure!(
            object_id(&object.key.kind, &object.bytes) == object.key.id,
            "object hash mismatch"
        );
        ensure!(
            raw.len() + 69 + object.bytes.len() <= MAX_PACK_BYTES,
            "object batch exceeds byte limit"
        );
        raw.push(if object.key.kind == "blob" { 0 } else { 1 });
        raw.extend_from_slice(object.key.id.as_bytes());
        raw.extend_from_slice(&(object.bytes.len() as u32).to_be_bytes());
        raw.extend_from_slice(&object.bytes);
    }
    Ok(zstd::bulk::compress(&raw, 1)?)
}

pub fn decode(bytes: &[u8]) -> Result<Vec<Object>> {
    let raw = zstd::bulk::decompress(bytes, MAX_PACK_BYTES)?;
    ensure!(raw.starts_with(b"KLP1"), "invalid object batch header");
    let mut input = Cursor::new(&raw[4..]);
    let mut objects = Vec::new();
    while (input.position() as usize) < raw.len() - 4 {
        ensure!(objects.len() < MAX_OBJECTS, "too many objects in batch");
        let mut kind = [0];
        input.read_exact(&mut kind)?;
        let kind = match kind[0] {
            0 => "blob",
            1 => "transaction",
            _ => anyhow::bail!("invalid object kind"),
        };
        let mut hash = [0; 64];
        input.read_exact(&mut hash)?;
        let key = Key {
            kind: kind.into(),
            id: String::from_utf8(hash.to_vec())?,
        };
        key.validate()?;
        let mut length = [0; 4];
        input.read_exact(&mut length)?;
        let length = u32::from_be_bytes(length) as usize;
        ensure!(
            length <= key.limit() && length <= raw.len() - 4 - input.position() as usize,
            "invalid object length"
        );
        let mut data = vec![0; length];
        input.read_exact(&mut data)?;
        ensure!(
            object_id(kind, &data) == key.id,
            "batch object hash mismatch"
        );
        objects.push(Object { key, bytes: data });
    }
    Request {
        objects: objects.iter().map(|object| object.key.clone()).collect(),
    }
    .validate()?;
    Ok(objects)
}

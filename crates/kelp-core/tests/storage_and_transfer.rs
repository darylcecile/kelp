use kelp_core::{
    object_id, storage,
    transfer::{self, Key, Object},
};

#[test]
fn compressed_storage_preserves_hashes_and_reads_legacy_rows() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("objects.sqlite3");
    let db = rusqlite::Connection::open(&path)?;
    db.execute_batch("CREATE TABLE objects(namespace TEXT,kind TEXT,id TEXT,bytes BLOB,PRIMARY KEY(namespace,kind,id));")?;
    let id = object_id("blob", b"legacy");
    db.execute(
        "INSERT INTO objects VALUES('demo','blob',?1,?2)",
        rusqlite::params![id, b"legacy".as_slice()],
    )?;
    drop(db);
    let db = storage::open(&path)?;
    assert_eq!(storage::get(&db, "demo", "blob", &id)?, b"legacy");
    let bytes = b"repeated source content\n".repeat(5000);
    let id = storage::put(&db, "demo", "blob", &bytes)?;
    assert_eq!(id, object_id("blob", &bytes));
    assert_eq!(storage::get(&db, "demo", "blob", &id)?, bytes);
    let length: i64 = db.query_row(
        "SELECT length(bytes) FROM loose_objects WHERE id=(SELECT loose FROM object_index WHERE hash=?1)",
        [storage::hash_bytes(&id)?],
        |row| row.get(0),
    )?;
    assert!(length < bytes.len() as i64 / 2);
    db.execute(
        "UPDATE loose_objects SET bytes = ?1 WHERE id=(SELECT loose FROM object_index WHERE hash=?2)",
        rusqlite::params![b"invalid".as_slice(), storage::hash_bytes(&id)?],
    )?;
    assert!(storage::get(&db, "demo", "blob", &id).is_err());
    Ok(())
}

#[test]
fn object_batches_preserve_binary_content_and_reject_malformed_or_corrupt_frames()
-> anyhow::Result<()> {
    let bytes = vec![0, 255, 1, 0, 42];
    let object = Object {
        key: Key {
            kind: "blob".into(),
            id: object_id("blob", &bytes),
        },
        bytes,
    };
    let encoded = transfer::encode(std::slice::from_ref(&object))?;
    let decoded = transfer::decode(&encoded)?;
    assert_eq!(decoded[0].bytes, object.bytes);
    assert_eq!(decoded[0].key, object.key);
    assert!(transfer::encode(&[object.clone(), object.clone()]).is_err());
    let mut raw = zstd::bulk::decompress(&encoded, transfer::MAX_PACK_BYTES)?;
    *raw.last_mut().unwrap() ^= 1;
    assert!(transfer::decode(&zstd::bulk::compress(&raw, 1)?).is_err());
    raw.truncate(20);
    assert!(transfer::decode(&zstd::bulk::compress(&raw, 1)?).is_err());
    Ok(())
}

use std::collections::{BTreeMap, BTreeSet};

use kelp_core::{
    object_id, storage,
    transactions::{Edit, Graph, Transaction},
};

#[test]
fn packing_preserves_every_object_hash_and_conflict_and_supports_more_writes() -> anyhow::Result<()>
{
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("store.sqlite3");
    let db = storage::open(&path)?;
    let mut objects = Vec::new();
    for index in 0..200 {
        let bytes = format!(
            "shared content\n{}variant {index}",
            "repeated line\n".repeat(1000)
        )
        .into_bytes();
        let id = storage::put(&db, "demo", "blob", &bytes)?;
        objects.push((id, bytes));
    }
    let mut graph = Graph::default();
    for (index, (blob, bytes)) in objects.iter().take(2).enumerate() {
        let transaction = Transaction {
            provenance: None,
            format: 1,
            nonce: format!("n{index}"),
            message: "Competing edit".into(),
            edits: BTreeMap::from([(
                "file".into(),
                Edit {
                    parents: BTreeSet::new(),
                    value: Some(kelp_core::FileEntry {
                        blob: blob.clone(),
                        size: bytes.len() as u64,
                        executable: false,
                        kind: Default::default(),
                    }),
                },
            )]),
        };
        let id = storage::put_json(&db, "demo", "transaction", &transaction)?;
        graph.transactions.insert(id, transaction);
    }
    let original_view = graph.view_id()?;
    let report = storage::compact(&db, &BTreeSet::from([objects.last().unwrap().0.clone()]))?;
    assert_eq!(report.objects, 202);
    assert!(report.packs > 0 && report.payload_after < report.payload_before);
    drop(db);
    let db = storage::open(&path)?;
    for (id, bytes) in &objects {
        assert_eq!(storage::get(&db, "demo", "blob", id)?, *bytes);
        assert_eq!(
            storage::size(&db, "demo", "blob", id)?,
            Some(bytes.len() as u64)
        );
    }
    let loaded = Graph {
        transactions: graph
            .transactions
            .keys()
            .map(|id| {
                Ok((
                    id.clone(),
                    storage::get_json(&db, "demo", "transaction", id)?,
                ))
            })
            .collect::<anyhow::Result<_>>()?,
    };
    assert_eq!(loaded.view_id()?, original_view);
    assert_eq!(loaded.view()?.conflicts(), BTreeSet::from(["file".into()]));
    let new = storage::put(&db, "demo", "blob", b"new write after compaction")?;
    storage::compact(&db, &BTreeSet::new())?;
    assert_eq!(
        storage::get(&db, "demo", "blob", &new)?,
        b"new write after compaction"
    );
    assert_eq!(storage::count(&db, "demo", "blob")?, 201);
    assert!(storage::get(&db, "other", "blob", &new).is_err());
    Ok(())
}

#[test]
fn corrupt_input_aborts_compaction_without_replacing_other_objects() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let db = storage::open(&directory.path().join("store.sqlite3"))?;
    let good = storage::put(&db, "demo", "blob", b"good")?;
    let bad = storage::put(&db, "demo", "blob", b"bad")?;
    db.execute(
        "UPDATE loose_objects SET bytes=?1 WHERE id=(SELECT loose FROM object_index WHERE hash=?2)",
        rusqlite::params![b"wrong".as_slice(), storage::hash_bytes(&bad)?],
    )?;
    assert!(storage::compact(&db, &BTreeSet::new()).is_err());
    assert_eq!(storage::get(&db, "demo", "blob", &good)?, b"good");
    assert_eq!(storage::count(&db, "demo", "blob")?, 2);
    Ok(())
}

#[test]
fn corrupted_pack_is_rejected_and_large_objects_remain_independently_readable() -> anyhow::Result<()>
{
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("store.sqlite3");
    let db = storage::open(&path)?;
    let a = storage::put(&db, "demo", "blob", b"aaa")?;
    storage::put(&db, "demo", "blob", b"bbb")?;
    let large = vec![42; 9 * 1024 * 1024];
    let id = storage::put(&db, "demo", "blob", &large)?;
    storage::compact(&db, &BTreeSet::new())?;
    assert_eq!(storage::get(&db, "demo", "blob", &id)?, large);
    db.execute(
        "UPDATE object_packs SET bytes=?1",
        [b"broken pack".as_slice()],
    )?;
    drop(db);
    std::thread::spawn(move || -> anyhow::Result<()> {
        let db = storage::open(&path)?;
        assert!(storage::get(&db, "demo", "blob", &a).is_err());
        Ok(())
    })
    .join()
    .unwrap()?;
    Ok(())
}

#[test]
fn compact_metadata_roundtrips_legacy_and_compressed_records() -> anyhow::Result<()> {
    let plain = br#"{"data":"legacy"}"#;
    assert_eq!(storage::decode_record(plain)?, plain);
    let data = b"large repeated metadata".repeat(1000);
    let encoded = storage::encode_record(&data)?;
    assert!(encoded.len() < data.len());
    assert_eq!(
        object_id("metadata", &storage::decode_record(&encoded)?),
        object_id("metadata", &data)
    );
    assert!(storage::decode_record(b"KZ01").is_err());
    Ok(())
}

#[test]
fn snapshot_dictionary_caches_survive_packing_their_base() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let db = storage::open(&directory.path().join("cache.sqlite3"))?;
    let base = (0..1000)
        .map(|index| {
            format!(
                "path-{index}: {}\n",
                object_id("entry", index.to_string().as_bytes())
            )
        })
        .collect::<String>()
        .into_bytes();
    let id = storage::put(&db, "demo", "snapshot", &base)?;
    let mut edited = base.clone();
    edited.extend_from_slice(b"one extra field");
    let cache = storage::encode_cache(&edited, &id, &base)?;
    storage::put(&db, "demo", "snapshot", &edited)?;
    storage::compact(&db, &BTreeSet::new())?;
    assert_eq!(storage::decode_cache(&db, "demo", &cache)?, edited);
    assert!(storage::decode_cache(&db, "another-project", &cache).is_err());
    Ok(())
}

#[test]
fn incompressible_groups_split_within_the_decode_budget() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let db = storage::open(&directory.path().join("binary.sqlite3"))?;
    let mut originals = Vec::new();
    let mut random = 0x12345678_u32;
    for _ in 0..3 {
        let mut bytes = Vec::with_capacity(4 * 1024 * 1024);
        for _ in 0..(4 * 1024 * 1024 / 4) {
            random ^= random << 13;
            random ^= random >> 17;
            random ^= random << 5;
            bytes.extend_from_slice(&random.to_le_bytes());
        }
        let id = storage::put(&db, "demo", "blob", &bytes)?;
        originals.push((id, bytes));
    }
    storage::compact(&db, &BTreeSet::new())?;
    let largest: Option<i64> =
        db.query_row("SELECT max(raw_size) FROM object_packs", [], |row| {
            row.get(0)
        })?;
    assert!(largest.is_none_or(|size| size <= 8 * 1024 * 1024));
    for (id, bytes) in originals {
        assert_eq!(storage::get(&db, "demo", "blob", &id)?, bytes);
    }
    Ok(())
}

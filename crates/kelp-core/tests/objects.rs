use std::collections::BTreeMap;

use kelp_core::{FileEntry, Snapshot, object_id, storage};

#[test]
fn snapshots_are_reproducible_and_corrupt_content_is_rejected() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let db = storage::open(&directory.path().join("store.sqlite3"))?;
    let bytes = b"hello\0binary\xff";
    let blob = storage::put(&db, "demo", "blob", bytes)?;
    let file = FileEntry {
        blob: blob.clone(),
        size: bytes.len() as u64,
        executable: false,
        kind: Default::default(),
    };
    let mut first = Snapshot::default();
    first.files.insert("z.txt".into(), file.clone());
    first.files.insert("a.txt".into(), file.clone());
    let second = Snapshot {
        files: BTreeMap::from([("a.txt".into(), file.clone()), ("z.txt".into(), file)]),
    };
    assert_eq!(first.id()?, second.id()?);
    assert_ne!(blob, object_id("snapshot", bytes));
    storage::verify_snapshot(&db, "demo", &first)?;
    assert!(storage::get(&db, "another-project", "blob", &blob).is_err());
    db.execute(
        "UPDATE loose_objects SET bytes = ?1 WHERE id = (SELECT loose FROM object_index WHERE hash=?2)",
        rusqlite::params![b"corrupt".as_slice(), storage::hash_bytes(&blob)?],
    )?;
    assert!(storage::verify_snapshot(&db, "demo", &first).is_err());
    Ok(())
}

#[test]
fn a_snapshot_cannot_escape_a_checkout_or_replace_its_metadata() {
    let file = FileEntry {
        blob: object_id("blob", b"data"),
        size: 4,
        executable: false,
        kind: Default::default(),
    };
    for path in [
        "../outside",
        "/absolute",
        "a/../../escape",
        ".kelp/workspace.sqlite3",
        ".git/config",
        "a/\0ff00",
        "a/\0ff2f",
    ] {
        let snapshot = Snapshot {
            files: BTreeMap::from([(path.into(), file.clone())]),
        };
        assert!(snapshot.validate().is_err(), "accepted {path}");
    }
    let snapshot = Snapshot {
        files: BTreeMap::from([("file".into(), file.clone()), ("file/child".into(), file)]),
    };
    assert!(snapshot.validate().is_err());
}

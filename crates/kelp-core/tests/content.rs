use kelp_core::{EntryKind, FileEntry, content, object_id, storage};

#[test]
fn native_path_encoding_is_lossless_and_cannot_alias_traversal_or_metadata() -> anyhow::Result<()> {
    let encoded = kelp_core::paths::from_bytes(b"dir/raw-\xff")?;
    assert_eq!(encoded, "dir/\x007261772dff");
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        assert_eq!(
            kelp_core::paths::to_native(&encoded)?
                .as_os_str()
                .as_bytes(),
            b"dir/raw-\xff"
        );
    }
    for bad in [
        "\x002e2e/file",
        "\x002e676974/config",
        "\0ff2f/file",
        "\0ff00",
        "dir/../file",
    ] {
        assert!(kelp_core::validate_path(bad).is_err());
    }
    Ok(())
}

#[test]
fn chunk_trees_preserve_small_hashes_reuse_unchanged_chunks_and_validate_closure()
-> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let db = storage::open(&temp.path().join("db"))?;
    let small = content::store(&db, "test", &b"small"[..], 5, true)?;
    assert_eq!(small.blob, object_id("blob", b"small"));
    assert_eq!(
        serde_json::to_string(&small)?,
        format!(r#"{{"blob":"{}","size":5,"executable":true}}"#, small.blob)
    );
    let mut bytes = vec![1; 17 * 1024 * 1024];
    let first = content::store(&db, "test", bytes.as_slice(), bytes.len() as u64, false)?;
    bytes[0] = 2;
    let second = content::store(&db, "test", bytes.as_slice(), bytes.len() as u64, false)?;
    let a = content::objects(&db, "test", &first)?;
    let b = content::objects(&db, "test", &second)?;
    assert_eq!(
        b.iter().filter(|id| !a.contains(id)).count(),
        2,
        "only one chunk and the root change"
    );
    assert_eq!(content::read(&db, "test", &second)?, bytes);
    let mut invalid = second.clone();
    invalid.size += 1;
    assert!(content::write(&db, "test", &invalid, &mut std::io::sink()).is_err());
    let leaf = FileEntry {
        blob: object_id("blob", b"missing"),
        size: 7,
        ..Default::default()
    };
    let children = vec![leaf.clone(), leaf];
    let incomplete = FileEntry {
        blob: storage::put_json(&db, "test", "blob", &children)?,
        size: 14,
        kind: EntryKind::Chunked,
        executable: false,
    };
    assert!(content::read(&db, "test", &incomplete).is_err());
    Ok(())
}

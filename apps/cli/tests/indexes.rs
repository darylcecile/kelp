use std::{
    collections::BTreeSet,
    fs::{self, File, FileTimes},
};

use kelp_cli::workspace::Workspace;
use kelp_core::{
    storage,
    transactions::{SavedView, Transaction},
};

#[test]
fn view_hashes_restore_full_state_and_commit_hashes_only_identify_edits() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let mut workspace = Workspace::init(directory.path(), "demo", None)?;
    fs::write(directory.path().join("file"), "first")?;
    let first = workspace.commit("First")?;
    fs::write(directory.path().join("file"), "second")?;
    let second = workspace.commit("Second")?;
    assert_ne!(first.view, first.transaction.clone().unwrap());
    assert!(
        workspace
            .restore_reference(second.transaction.as_ref().unwrap())
            .is_err()
    );
    assert_eq!(fs::read(directory.path().join("file"))?, b"second");
    let (restored, backup) = workspace.restore_reference(&format!("view:{}", &first.view[..12]))?;
    assert_eq!(restored, first.view);
    assert_eq!(fs::read(directory.path().join("file"))?, b"first");
    workspace.restore_reference(&backup)?;
    assert_eq!(fs::read(directory.path().join("file"))?, b"second");
    let clone = tempfile::tempdir()?;
    let mut other = Workspace::init(clone.path(), "demo", None)?;
    // The descriptor identity does not depend on a local SQLite row number.
    let descriptor: SavedView =
        storage::get_json(&workspace.db, "demo", "saved-view", &first.view)?;
    other.state.transactions = descriptor.roots.clone();
    for id in &other.state.transactions {
        storage::put_json(
            &other.db,
            "demo",
            "transaction",
            &workspace.transaction(id)?,
        )?;
    }
    let snapshot = workspace.checkpoint_snapshot(first.id)?;
    storage::put_json(&other.db, "demo", "snapshot", &snapshot)?;
    let saved = other.remember_snapshot(first.snapshot.clone(), Some("From another machine"))?;
    assert_eq!(saved.view, first.view);
    Ok(())
}

#[test]
fn hash_prefixes_are_rejected_when_ambiguous_instead_of_selecting_a_version() -> anyhow::Result<()>
{
    let directory = tempfile::tempdir()?;
    let workspace = Workspace::init(directory.path(), "demo", None)?;
    let mut prefixes = std::collections::BTreeMap::new();
    for nonce in 0..2000 {
        let transaction = Transaction {
            format: 1,
            nonce: format!("n-{nonce}"),
            message: "Empty commit".into(),
            edits: Default::default(),
        };
        let id = storage::put_json(&workspace.db, "demo", "transaction", &transaction)?;
        let prefix = id[..4].to_owned();
        if let Some(other) = prefixes.insert(prefix.clone(), id.clone()) {
            assert!(workspace.resolve(&prefix).is_err());
            assert_eq!(workspace.resolve(&id)?.1, id);
            assert_eq!(workspace.resolve(&other)?.1, other);
            return Ok(());
        }
    }
    anyhow::bail!("fixture did not produce a prefix collision")
}

#[test]
fn incremental_projection_matches_rebuilt_graph_and_recovers_a_stale_cache() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let mut workspace = Workspace::init(directory.path(), "demo", None)?;
    for n in 0..12 {
        fs::write(
            directory.path().join(format!("file-{}", n % 3)),
            n.to_string(),
        )?;
        workspace.commit(&format!("Edit {n}"))?;
        let graph = workspace.graph()?;
        assert_eq!(workspace.projection()?.view, graph.view()?);
        assert_eq!(workspace.projection()?.id()?, graph.view_id()?);
    }
    let expected = workspace.projection()?.id()?;
    workspace
        .db
        .execute("UPDATE projection_cache SET checksum = 'corrupt'", [])?;
    assert_eq!(workspace.projection()?.id()?, expected);
    workspace.db.execute("DELETE FROM projection_cache", [])?;
    drop(workspace);
    assert_eq!(
        Workspace::open(directory.path())?.projection()?.id()?,
        expected
    );
    Ok(())
}

#[test]
fn file_cache_detects_same_size_rewrites_restored_mtime_deletes_and_replacements()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("file");
    let mut workspace = Workspace::init(directory.path(), "demo", None)?;
    fs::write(&path, "first")?;
    workspace.commit("Initial")?;
    let modified = fs::metadata(&path)?.modified()?;
    assert!(workspace.status()?.changed.is_empty());
    fs::write(&path, "other")?;
    File::options()
        .write(true)
        .open(&path)?
        .set_times(FileTimes::new().set_modified(modified))?;
    assert_eq!(workspace.status()?.changed, ["file"]);
    let saved = workspace.commit("Same-size rewrite")?;
    let snapshot = workspace.checkpoint_snapshot(saved.id)?;
    assert_eq!(
        storage::get(&workspace.db, "demo", "blob", &snapshot.files["file"].blob)?,
        b"other"
    );
    fs::remove_file(&path)?;
    assert_eq!(workspace.status()?.changed, ["file"]);
    fs::write(&path, "third")?;
    assert_eq!(workspace.status()?.changed, ["file"]);
    assert!(workspace.graph()?.view()?.conflicts() == BTreeSet::new());
    Ok(())
}

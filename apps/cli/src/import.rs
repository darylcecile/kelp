use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    process::{Command, Stdio},
};

use anyhow::{Context, Result, ensure};
use kelp_core::{EntryKind, FileEntry, MAX_METADATA_BYTES, Snapshot, content, storage};

use crate::{index::Projection, workspace::Workspace};

fn command(source: &Path) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(source);
    command
}

fn git(source: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = command(source)
        .args(args)
        .output()
        .context("Git is required to import a repository")?;
    ensure!(
        output.status.success(),
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output.stdout)
}

/// Import the selected Git revision and its complete ancestry into immutable
/// edit transactions. Git merge parents are composed before recording their
/// resolved tree, rather than flattening side branches into a linear history.
pub fn repository(
    source: &Path,
    destination: &Path,
    project: &str,
    revision: &str,
) -> Result<Workspace> {
    ensure!(
        fs::symlink_metadata(destination).is_err(),
        "destination already exists"
    );
    let head = String::from_utf8(git(
        source,
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{revision}^{{commit}}"),
        ],
    )?)?;
    let history = String::from_utf8(git(
        source,
        &[
            "rev-list",
            "--reverse",
            "--topo-order",
            "--parents",
            head.trim(),
        ],
    )?)?;
    let parent = destination
        .parent()
        .context("destination needs a parent directory")?;
    fs::create_dir_all(parent)?;
    let staging = tempfile::tempdir_in(parent)?;
    let checkout = staging.path().join("checkout");
    let mut workspace = Workspace::init(&checkout, project, None)?;
    let mut roots: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut views = BTreeMap::new();
    let mut contents: BTreeMap<(String, String), FileEntry> = BTreeMap::new();
    for line in history.lines() {
        let mut ids = line.split_whitespace();
        let commit = ids.next().context("empty Git commit record")?;
        let parents: BTreeSet<_> = ids.flat_map(|id| roots[id].iter().cloned()).collect();
        let graph = workspace.graph_from_roots(&parents)?;
        let mut projection = Projection::default();
        for id in graph.ordered()? {
            projection.apply(&id, &graph.transactions[&id]);
        }
        let listing = git(source, &["ls-tree", "-rzl", "--full-tree", commit])?;
        let mut snapshot = Snapshot::default();
        for entry in listing
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
        {
            let tab = entry
                .iter()
                .position(|byte| *byte == b'\t')
                .context("invalid Git tree entry")?;
            let header = std::str::from_utf8(&entry[..tab])?
                .split_whitespace()
                .collect::<Vec<_>>();
            ensure!(
                header.len() == 4 && header[1] == "blob",
                "Git submodules must be imported as their own Kelp projects"
            );
            let path = kelp_core::paths::from_bytes(&entry[tab + 1..])?;
            kelp_core::validate_path(&path)?;
            let (mode, id, size) = (header[0], header[2], header[3].parse::<u64>()?);
            let key = (id.to_owned(), mode.to_owned());
            let value = if let Some(value) = contents.get(&key) {
                value.clone()
            } else {
                let mut child = command(source)
                    .args(["cat-file", "blob", id])
                    .stdout(Stdio::piped())
                    .stderr(Stdio::inherit())
                    .spawn()?;
                let value = content::store(
                    &workspace.db,
                    project,
                    child.stdout.take().context("Git blob stream missing")?,
                    size,
                    mode == "100755",
                );
                if value.is_err() {
                    let _ = child.kill();
                }
                let status = child.wait()?;
                ensure!(status.success(), "Git could not read blob {id}");
                let mut value = value?;
                if mode == "120000" {
                    value.kind = EntryKind::Symlink;
                }
                contents.insert(key, value.clone());
                value
            };
            snapshot.files.insert(path, value);
        }
        let raw_commit = git(source, &["cat-file", "commit", commit])?;
        let provenance = content::store(
            &workspace.db,
            project,
            raw_commit.as_slice(),
            raw_commit.len() as u64,
            false,
        )?;
        let message = String::from_utf8(git(
            source,
            &[
                "show",
                "-s",
                "--format=%B%nGit commit: %H%nAuthor: %an <%ae> %aI%nCommitter: %cn <%ce> %cI",
                commit,
            ],
        )?)?;
        let mut transaction =
            projection
                .view
                .record(&snapshot, message.trim_end().into(), commit.into())?;
        transaction.format = 2;
        transaction.provenance = Some(provenance);
        let bytes = serde_json::to_vec(&transaction)?;
        ensure!(
            bytes.len() <= MAX_METADATA_BYTES,
            "Git commit {commit} exceeds the transaction metadata limit"
        );
        let id = storage::put(&workspace.db, project, "transaction", &bytes)?;
        projection.apply(&id, &transaction);
        workspace.state.transactions = graph.transactions.into_keys().chain([id.clone()]).collect();
        workspace.state.outbox.insert(id.clone());
        workspace.cache_projection(&projection)?;
        workspace.state.base_snapshot =
            storage::put_json(&workspace.db, project, "snapshot", &snapshot)?;
        let saved = workspace.remember_snapshot(
            workspace.state.base_snapshot.clone(),
            Some(transaction.message.trim_end()),
        )?;
        let timestamp: i64 =
            String::from_utf8(git(source, &["show", "-s", "--format=%ct", commit])?)?
                .trim()
                .parse()?;
        workspace.db.execute(
            "UPDATE checkpoints SET transaction_id=?1, created_at=?2 WHERE id=?3",
            rusqlite::params![storage::hash_bytes(&id)?, timestamp, saved.id],
        )?;
        roots.insert(commit.into(), projection.roots);
        views.insert(commit.to_owned(), saved.view);
    }
    let tags = String::from_utf8(git(
        source,
        &["for-each-ref", "--format=%(refname)", "refs/tags/"],
    )?)?;
    for tag in tags.lines() {
        if let Ok(id) = git(
            source,
            &["rev-parse", "--verify", &format!("{tag}^{{commit}}")],
        ) {
            let id = String::from_utf8(id)?;
            if let Some(view) = views.get(id.trim()) {
                workspace.tag(tag.trim_start_matches("refs/tags/").into(), Some(view))?;
            }
        }
    }
    let snapshot = workspace.baseline()?;
    workspace.apply_snapshot(&Snapshot::default(), &snapshot)?;
    workspace.state.tracked = snapshot.files.keys().cloned().collect();
    workspace.save_state()?;
    drop(workspace);
    fs::rename(checkout, destination)?;
    Workspace::open(destination)
}

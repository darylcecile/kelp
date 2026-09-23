use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use ignore::WalkBuilder;
use kelp_core::{
    FileEntry, MAX_BLOB_BYTES, MAX_METADATA_BYTES, Publication, Revision, Snapshot, object_id,
    storage, validate_name, validate_path,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceState {
    pub version: u32,
    pub project: String,
    pub remote: Option<String>,
    pub tracked: BTreeSet<String>,
    pub base_snapshot: String,
    pub change: Option<String>,
    pub head: Option<String>,
    pub message: Option<String>,
    pub pending: Option<Publication>,
}

#[derive(Debug, Serialize)]
pub struct Checkpoint {
    pub id: i64,
    pub snapshot: String,
    pub message: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Serialize)]
pub struct Status {
    pub project: String,
    pub remote: Option<String>,
    pub change: Option<String>,
    pub message: Option<String>,
    pub head: Option<String>,
    pub pending: bool,
    pub changed: Vec<String>,
    pub untracked: Vec<String>,
    pub checkpoint: Option<i64>,
    pub watcher_running: bool,
}

pub struct Workspace {
    pub root: PathBuf,
    pub db: Connection,
    pub state: WorkspaceState,
    _lock: File,
}

impl Workspace {
    pub fn init(path: &Path, project: &str, remote: Option<String>) -> Result<Self> {
        validate_name(project)?;
        fs::create_dir_all(path)?;
        let root = path.canonicalize()?;
        let tracked = discover_files(&root)?;
        let metadata = root.join(".kelp");
        fs::create_dir(&metadata).context("this directory already has Kelp metadata")?;
        let lock = lock_workspace(&root)?;
        let db = storage::open(&metadata.join("workspace.sqlite3"))?;
        db.execute_batch(
            "CREATE TABLE workspace (id INTEGER PRIMARY KEY CHECK (id = 1), state TEXT NOT NULL);
             CREATE TABLE checkpoints (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                snapshot TEXT NOT NULL,
                message TEXT,
                created_at INTEGER NOT NULL
             );
             CREATE TABLE watcher (id INTEGER PRIMARY KEY CHECK (id = 1), enabled INTEGER NOT NULL, heartbeat INTEGER NOT NULL);
             INSERT INTO watcher VALUES (1, 0, 0);",
        )?;
        let base_snapshot = storage::put_json(&db, project, "snapshot", &Snapshot::default())?;
        let state = WorkspaceState {
            version: 0,
            project: project.into(),
            remote,
            tracked,
            base_snapshot,
            change: None,
            head: None,
            message: None,
            pending: None,
        };
        let workspace = Self {
            root,
            db,
            state,
            _lock: lock,
        };
        workspace.save_state()?;
        workspace.capture(Some("Workspace created"))?;
        Ok(workspace)
    }

    pub fn open(path: &Path) -> Result<Self> {
        let root = find_root(path)?;
        let lock = lock_workspace(&root)?;
        let db = storage::open(&root.join(".kelp/workspace.sqlite3"))?;
        let state: String =
            db.query_row("SELECT state FROM workspace WHERE id = 1", [], |r| r.get(0))?;
        let state: WorkspaceState = serde_json::from_str(&state)?;
        ensure!(
            state.version == 0,
            "unsupported workspace version {}",
            state.version
        );
        Ok(Self {
            root,
            db,
            state,
            _lock: lock,
        })
    }

    pub fn save_state(&self) -> Result<()> {
        self.db.execute(
            "INSERT INTO workspace(id, state) VALUES (1, ?1) ON CONFLICT(id) DO UPDATE SET state = excluded.state",
            [serde_json::to_string(&self.state)?],
        )?;
        Ok(())
    }

    pub fn capture(&self, message: Option<&str>) -> Result<Checkpoint> {
        let tx = self.db.unchecked_transaction()?;
        let snapshot = self.scan(Some(&tx))?;
        let encoded = serde_json::to_vec(&snapshot)?;
        ensure!(
            encoded.len() <= MAX_METADATA_BYTES,
            "snapshot metadata exceeds the v0 2 MiB limit"
        );
        let id = storage::put(&tx, &self.state.project, "snapshot", &encoded)?;
        if message.is_none()
            && let Some(last) = self.checkpoints(1)?.pop()
            && last.snapshot == id
        {
            return Ok(last);
        }
        let created_at = now();
        tx.execute(
            "INSERT INTO checkpoints(snapshot, message, created_at) VALUES (?1, ?2, ?3)",
            params![id, message, created_at],
        )?;
        let checkpoint = Checkpoint {
            id: tx.last_insert_rowid(),
            snapshot: id,
            message: message.map(str::to_owned),
            created_at,
        };
        tx.commit()?;
        Ok(checkpoint)
    }

    pub fn checkpoints(&self, limit: usize) -> Result<Vec<Checkpoint>> {
        let mut stmt = self.db.prepare(
            "SELECT id, snapshot, message, created_at FROM checkpoints ORDER BY id DESC LIMIT ?1",
        )?;
        Ok(stmt
            .query_map([limit as i64], |r| {
                Ok(Checkpoint {
                    id: r.get(0)?,
                    snapshot: r.get(1)?,
                    message: r.get(2)?,
                    created_at: r.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?)
    }

    pub fn checkpoint_snapshot(&self, checkpoint: i64) -> Result<Snapshot> {
        let id: Option<String> = self
            .db
            .query_row(
                "SELECT snapshot FROM checkpoints WHERE id = ?1",
                [checkpoint],
                |r| r.get(0),
            )
            .optional()?;
        storage::get_json(
            &self.db,
            &self.state.project,
            "snapshot",
            &id.context("checkpoint does not exist")?,
        )
    }

    pub fn track(&mut self, paths: &[PathBuf], cwd: &Path) -> Result<usize> {
        let candidates = discover_files(&self.root)?;
        let before = self.state.tracked.len();
        for path in paths {
            let path = cwd
                .join(path)
                .canonicalize()
                .with_context(|| format!("cannot track {}", path.display()))?;
            ensure!(
                path.starts_with(&self.root),
                "path is outside this workspace"
            );
            let selected: Vec<_> = candidates
                .iter()
                .filter(|candidate| self.root.join(candidate).starts_with(&path))
                .cloned()
                .collect();
            ensure!(
                !selected.is_empty(),
                "no supported, non-ignored files at {}",
                path.display()
            );
            self.state.tracked.extend(selected);
        }
        self.save_state()?;
        self.capture(None)?;
        Ok(self.state.tracked.len() - before)
    }

    pub fn status(&self) -> Result<Status> {
        let current = self.scan(None)?;
        let baseline = self.baseline()?;
        let paths: BTreeSet<_> = current.files.keys().chain(baseline.files.keys()).collect();
        let changed = paths
            .into_iter()
            .filter(|path| current.files.get(*path) != baseline.files.get(*path))
            .cloned()
            .collect();
        let untracked = discover_files(&self.root)?
            .difference(&self.state.tracked)
            .cloned()
            .collect();
        let (enabled, heartbeat): (bool, i64) = self.db.query_row(
            "SELECT enabled, heartbeat FROM watcher WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok(Status {
            project: self.state.project.clone(),
            remote: self.state.remote.clone(),
            change: self.state.change.clone(),
            message: self.state.message.clone(),
            head: self.state.head.clone(),
            pending: self.state.pending.is_some(),
            changed,
            untracked,
            checkpoint: self.checkpoints(1)?.first().map(|c| c.id),
            watcher_running: enabled && now().saturating_sub(heartbeat) < 5,
        })
    }

    /// Prepare and persist exactly one revision before any network operation.
    pub fn prepare_publication(&mut self, message: Option<&str>) -> Result<Option<Publication>> {
        ensure!(
            self.state.pending.is_none(),
            "finish the pending publication first"
        );
        let message = message
            .map(str::to_owned)
            .or_else(|| self.state.message.clone())
            .context("name this change with `kelp publish -m \"Describe the work\"`")?;
        ensure!(
            !message.trim().is_empty(),
            "change message must not be empty"
        );
        let checkpoint = self.capture(None)?;
        if let Some(head) = &self.state.head {
            let previous: Revision =
                storage::get_json(&self.db, &self.state.project, "revision", head)?;
            if previous.result_snapshot == checkpoint.snapshot && previous.message == message {
                return Ok(None);
            }
        }
        let change = self
            .state
            .change
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let revision = Revision {
            project: self.state.project.clone(),
            change: change.clone(),
            predecessor: self.state.head.clone(),
            base_snapshot: self.state.base_snapshot.clone(),
            result_snapshot: checkpoint.snapshot,
            message: message.clone(),
        };
        revision.validate()?;
        let publication = Publication {
            request_id: Uuid::new_v4().to_string(),
            revision,
        };
        let tx = self.db.unchecked_transaction()?;
        storage::put_json(&tx, &self.state.project, "revision", &publication.revision)?;
        self.state.change = Some(change);
        self.state.message = Some(message);
        self.state.pending = Some(publication.clone());
        self.save_state()?;
        tx.commit()?;
        Ok(Some(publication))
    }

    pub fn acknowledge(&mut self, publication: &Publication) -> Result<()> {
        ensure!(
            self.state.pending.as_ref().map(|p| &p.request_id) == Some(&publication.request_id),
            "publication state changed"
        );
        self.state.head = Some(publication.revision.id()?);
        self.state.pending = None;
        self.save_state()
    }

    pub fn new_change(&mut self, message: String) -> Result<()> {
        ensure!(
            self.state.pending.is_none(),
            "finish the pending publication before starting another change"
        );
        ensure!(
            !message.trim().is_empty(),
            "change message must not be empty"
        );
        let checkpoint = self.capture(Some("Before starting another change"))?;
        self.state.base_snapshot = checkpoint.snapshot;
        self.state.change = None;
        self.state.head = None;
        self.state.message = Some(message);
        self.save_state()
    }

    fn baseline(&self) -> Result<Snapshot> {
        let id = if let Some(head) = &self.state.head {
            let revision: Revision =
                storage::get_json(&self.db, &self.state.project, "revision", head)?;
            revision.result_snapshot
        } else {
            self.state.base_snapshot.clone()
        };
        storage::get_json(&self.db, &self.state.project, "snapshot", &id)
    }

    fn scan(&self, store: Option<&Connection>) -> Result<Snapshot> {
        #[cfg(not(unix))]
        let baseline = self.baseline()?;
        let mut files = BTreeMap::new();
        for path in &self.state.tracked {
            validate_path(path)?;
            check_ancestors(&self.root, path)?;
            let full = self.root.join(path);
            let metadata = match fs::symlink_metadata(&full) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            ensure!(
                metadata.is_file(),
                "only regular files are supported in v0: {path}"
            );
            ensure!(
                metadata.len() <= MAX_BLOB_BYTES as u64,
                "{path} exceeds the v0 16 MiB file limit"
            );
            let bytes = fs::read(&full).with_context(|| format!("read {path}"))?;
            ensure!(
                bytes.len() <= MAX_BLOB_BYTES,
                "{path} exceeds the v0 16 MiB file limit"
            );
            let blob = match store {
                Some(db) => storage::put(db, &self.state.project, "blob", &bytes)?,
                None => object_id("blob", &bytes),
            };
            #[cfg(unix)]
            let executable = executable(&metadata);
            #[cfg(not(unix))]
            let executable = baseline
                .files
                .get(path)
                .is_some_and(|entry| entry.executable);
            files.insert(
                path.clone(),
                FileEntry {
                    blob,
                    size: bytes.len() as u64,
                    executable,
                },
            );
        }
        Ok(Snapshot { files })
    }
}

pub fn find_root(path: &Path) -> Result<PathBuf> {
    for candidate in path.canonicalize()?.ancestors() {
        if candidate.join(".kelp/workspace.sqlite3").is_file() {
            return Ok(candidate.to_owned());
        }
    }
    bail!("not in a Kelp workspace; use `kelp init` to create one")
}

fn lock_workspace(root: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join(".kelp/workspace.lock"))?;
    file.lock_exclusive()?;
    Ok(file)
}

pub fn discover_files(root: &Path) -> Result<BTreeSet<String>> {
    let mut files = BTreeSet::new();
    let mut walker = WalkBuilder::new(root);
    walker
        .hidden(false)
        .require_git(false)
        .parents(false)
        .git_global(false)
        .follow_links(false)
        .add_custom_ignore_filename(".kelpignore")
        .filter_entry(|entry| !matches!(entry.file_name().to_str(), Some(".kelp" | ".git")));
    for entry in walker.build() {
        let entry = entry?;
        if entry.file_type().is_some_and(|kind| kind.is_dir()) {
            continue;
        }
        let path = entry
            .path()
            .strip_prefix(root)?
            .to_str()
            .context("v0 requires UTF-8 file paths")?
            .replace(std::path::MAIN_SEPARATOR, "/");
        validate_path(&path)?;
        ensure!(
            entry.file_type().is_some_and(|kind| kind.is_file()),
            "v0 does not support symlinks or special files: {path}"
        );
        files.insert(path);
    }
    Ok(files)
}

fn check_ancestors(root: &Path, path: &str) -> Result<()> {
    let mut parent = root.to_owned();
    let parts: Vec<_> = path.split('/').collect();
    for part in &parts[..parts.len() - 1] {
        parent.push(part);
        match fs::symlink_metadata(&parent) {
            Ok(metadata) => ensure!(
                metadata.is_dir() && !metadata.is_symlink(),
                "non-directory or symlink ancestor: {}",
                parent.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

/// Restore into a fresh directory so existing user files are never overwritten.
pub fn materialize(
    db: &Connection,
    project: &str,
    snapshot: &Snapshot,
    destination: &Path,
) -> Result<()> {
    storage::verify_snapshot(db, project, snapshot)?;
    ensure!(
        fs::symlink_metadata(destination).is_err(),
        "destination already exists: {}",
        destination.display()
    );
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let staging = tempfile::tempdir_in(parent)?;
    for (path, entry) in &snapshot.files {
        let full = staging.path().join(path);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&full)
            .with_context(|| format!("cannot materialize {path} on this filesystem"))?;
        file.write_all(&storage::get(db, project, "blob", &entry.blob)?)?;
        set_executable(&file, entry.executable)?;
        file.sync_all()?;
    }
    fs::rename(staging.path(), destination)?;
    Ok(())
}

#[cfg(unix)]
fn executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(unix)]
fn set_executable(file: &File, executable: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(if executable {
        0o755
    } else {
        0o644
    }))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_: &File, _: bool) -> Result<()> {
    Ok(())
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

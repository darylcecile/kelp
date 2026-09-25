use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::index::{self, Projection};
use crate::selection;
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use ignore::WalkBuilder;
use kelp_core::{
    FileEntry, MAX_BLOB_BYTES, MAX_METADATA_BYTES, Snapshot, storage,
    transactions::{Graph, SavedView, Transaction, View},
    validate_name, validate_path,
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
    #[serde(default)]
    pub transactions: BTreeSet<String>,
    #[serde(default)]
    pub outbox: BTreeSet<String>,
    #[serde(default)]
    pub cursors: Vec<i64>,
    #[serde(default)]
    pub layout: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct Checkpoint {
    #[serde(skip_serializing)]
    pub id: i64,
    pub view: String,
    pub snapshot: String,
    pub message: Option<String>,
    pub created_at: i64,
    pub transaction: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Status {
    pub project: String,
    pub remote: Option<String>,
    pub view: String,
    pub pending_commits: usize,
    pub changed: Vec<String>,
    pub conflicts: BTreeSet<String>,
    pub version: Option<String>,
    pub paths: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct FileChange {
    pub path: String,
    pub kind: &'static str,
}

#[derive(Debug, Serialize)]
pub struct FileDiff {
    #[serde(flatten)]
    pub file: FileChange,
    pub diff: String,
}

#[derive(Debug, Serialize)]
pub struct HistoryEntry {
    #[serde(flatten)]
    pub version: Checkpoint,
    pub files: Vec<FileChange>,
}

pub fn file_changes(before: &Snapshot, after: &Snapshot) -> Vec<FileChange> {
    let paths: BTreeSet<_> = before.files.keys().chain(after.files.keys()).collect();
    paths
        .into_iter()
        .filter_map(|path| {
            let (old, new) = (before.files.get(path), after.files.get(path));
            if old == new {
                return None;
            }
            Some(FileChange {
                path: path.clone(),
                kind: match (old, new) {
                    (None, Some(_)) => "added",
                    (Some(_), None) => "deleted",
                    _ => "modified",
                },
            })
        })
        .collect()
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
                snapshot BLOB NOT NULL,
                message TEXT,
                created_at INTEGER NOT NULL,
                transaction_id BLOB
             );",
        )?;
        let base_snapshot = storage::put_json(&db, project, "snapshot", &Snapshot::default())?;
        let state = WorkspaceState {
            version: 1,
            project: project.into(),
            remote,
            tracked,
            base_snapshot,
            transactions: BTreeSet::new(),
            outbox: BTreeSet::new(),
            cursors: Vec::new(),
            layout: None,
            paths: Vec::new(),
        };
        let workspace = Self {
            root,
            db,
            state,
            _lock: lock,
        };
        workspace.save_state()?;
        workspace.setup_indexes()?;
        Ok(workspace)
    }

    pub fn open(path: &Path) -> Result<Self> {
        let root = find_root(path)?;
        let lock = lock_workspace(&root)?;
        let db = storage::open(&root.join(".kelp/workspace.sqlite3"))?;
        let has_watcher: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'watcher')",
            [],
            |row| row.get(0),
        )?;
        if has_watcher {
            db.execute("UPDATE watcher SET enabled = 0 WHERE id = 1", [])?;
        }
        let encoded: Vec<u8> = db.query_row(
            "SELECT CAST(state AS BLOB) FROM workspace WHERE id = 1",
            [],
            |r| r.get(0),
        )?;
        let bytes = storage::decode_record(&encoded)?;
        let mut state: WorkspaceState = serde_json::from_slice(&bytes)?;
        ensure!(
            state.version <= 2,
            "unsupported workspace version {}",
            state.version
        );
        ensure!(
            selection::normalize(state.paths.clone())? == state.paths,
            "invalid checkout path selection"
        );
        ensure!(
            state.paths.is_empty() || state.version == 2,
            "partial checkout requires workspace format 2"
        );
        let columns = db
            .prepare("PRAGMA table_info(checkpoints)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<Result<Vec<_>, _>>()?;
        if !columns.iter().any(|name| name == "transaction_id") {
            db.execute("ALTER TABLE checkpoints ADD COLUMN transaction_id TEXT", [])?;
        }
        if state.version == 0 {
            let tx = db.unchecked_transaction()?;
            tx.execute("CREATE TABLE IF NOT EXISTS previous_workspace (id INTEGER PRIMARY KEY, state TEXT NOT NULL)", [])?;
            tx.execute(
                "INSERT OR IGNORE INTO previous_workspace(id, state) VALUES (0, ?1)",
                [String::from_utf8(bytes)?],
            )?;
            state.base_snapshot =
                storage::put_json(&tx, &state.project, "snapshot", &Snapshot::default())?;
            state.version = 1;
            tx.execute(
                "UPDATE workspace SET state = ?1 WHERE id = 1",
                [storage::encode_record(&serde_json::to_vec(&state)?)?],
            )?;
            tx.commit()?;
        }
        let workspace = Self {
            root,
            db,
            state,
            _lock: lock,
        };
        workspace.setup_indexes()?;
        Ok(workspace)
    }

    fn setup_indexes(&self) -> Result<()> {
        index::setup(&self.db, &self.state.project)?;
        let columns = self
            .db
            .prepare("PRAGMA table_info(checkpoints)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<Result<Vec<_>, _>>()?;
        if !columns.iter().any(|column| column == "view_id") {
            let tx = self.db.unchecked_transaction()?;
            tx.execute("ALTER TABLE checkpoints ADD COLUMN view_id BLOB", [])?;
            // Old snapshots have no recorded causal set. Preserve their exact
            // bytes and mark the unknown context with an empty root set.
            let rows = tx
                .prepare("SELECT id, snapshot FROM checkpoints")?
                .query_map([], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            for (id, snapshot) in rows {
                let view = SavedView {
                    format: 1,
                    snapshot,
                    roots: BTreeSet::new(),
                    paths: Vec::new(),
                };
                let hash = storage::put_json(&tx, &self.state.project, "saved-view", &view)?;
                tx.execute(
                    "UPDATE checkpoints SET view_id = ?1 WHERE id = ?2",
                    params![storage::hash_bytes(&hash)?, id],
                )?;
            }
            tx.commit()?;
        }
        self.compact_history_keys()?;
        self.db.execute(
            "CREATE INDEX IF NOT EXISTS checkpoints_view ON checkpoints(view_id)",
            [],
        )?;
        Ok(())
    }

    fn compact_history_keys(&self) -> Result<()> {
        let kind: String = self.db.query_row(
            "SELECT type FROM pragma_table_info('checkpoints') WHERE name='snapshot'",
            [],
            |row| row.get(0),
        )?;
        if kind.eq_ignore_ascii_case("BLOB") {
            return Ok(());
        }
        let tx = self.db.unchecked_transaction()?;
        tx.execute_batch("CREATE TABLE checkpoints_compact (
            id INTEGER PRIMARY KEY AUTOINCREMENT,snapshot BLOB NOT NULL,message TEXT,created_at INTEGER NOT NULL,transaction_id BLOB,view_id BLOB
        );")?;
        let mut statement=tx.prepare("SELECT id,snapshot,message,created_at,transaction_id,
            CASE WHEN typeof(view_id)='blob' THEN lower(hex(view_id)) ELSE view_id END FROM checkpoints ORDER BY id")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let (id, snapshot, message, time, transaction, view): (
                i64,
                String,
                Option<String>,
                i64,
                Option<String>,
                String,
            ) = (
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            );
            tx.execute(
                "INSERT INTO checkpoints_compact VALUES(?1,?2,?3,?4,?5,?6)",
                params![
                    id,
                    storage::hash_bytes(&snapshot)?,
                    message,
                    time,
                    transaction
                        .as_deref()
                        .map(storage::hash_bytes)
                        .transpose()?,
                    storage::hash_bytes(&view)?
                ],
            )?;
        }
        drop(rows);
        drop(statement);
        tx.execute_batch(
            "DROP TABLE checkpoints; ALTER TABLE checkpoints_compact RENAME TO checkpoints;",
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn projection(&self) -> Result<Projection> {
        if let Some(projection) =
            index::load(&self.db, &self.state.project, &self.state.transactions)?
        {
            return Ok(projection);
        }
        let graph = self.graph()?;
        let projection = Projection {
            view: graph.view()?,
            roots: graph.roots(),
        };
        index::save(&self.db, &self.state.transactions, &projection)?;
        Ok(projection)
    }

    pub fn cache_projection(&self, projection: &Projection) -> Result<()> {
        index::save(&self.db, &self.state.transactions, projection)
    }

    pub fn transaction(&self, id: &str) -> Result<Transaction> {
        storage::get_json(&self.db, &self.state.project, "transaction", id)
    }

    pub fn extend_projection(&self, additions: &Graph) -> Result<Projection> {
        let mut projection = self.projection()?;
        let mut boundary = BTreeMap::new();
        for transaction in additions.transactions.values() {
            for parent in transaction.dependencies() {
                if !additions.transactions.contains_key(&parent) && !boundary.contains_key(&parent)
                {
                    ensure!(
                        self.state.transactions.contains(&parent),
                        "missing dependency {parent}"
                    );
                    boundary.insert(parent.clone(), self.transaction(&parent)?);
                }
            }
        }
        for id in additions.ordered_after(&self.state.transactions)? {
            let transaction = &additions.transactions[&id];
            let parents = transaction
                .dependencies()
                .into_iter()
                .map(|parent| {
                    let value = additions
                        .transactions
                        .get(&parent)
                        .or_else(|| boundary.get(&parent))
                        .expect("validated boundary");
                    (parent, value.clone())
                })
                .collect();
            ensure!(transaction.id()? == id, "transaction ID mismatch");
            transaction.validate_parents(&parents)?;
            projection.apply(&id, transaction);
        }
        Ok(projection)
    }

    pub fn save_state(&self) -> Result<()> {
        self.db.execute(
            "INSERT INTO workspace(id, state) VALUES (1, ?1) ON CONFLICT(id) DO UPDATE SET state = excluded.state",
            [storage::encode_record(&serde_json::to_vec(&self.state)?)?],
        )?;
        Ok(())
    }

    pub fn capture(&self, message: Option<&str>) -> Result<Checkpoint> {
        let tx = self.db.unchecked_transaction()?;
        let snapshot = self.scan(Some(&tx))?;
        let encoded = serde_json::to_vec(&snapshot)?;
        let id = storage::put(&tx, &self.state.project, "snapshot", &encoded)?;
        if message.is_none()
            && let Some(last) = self.checkpoints(1)?.pop()
            && last.snapshot == id
        {
            return Ok(last);
        }
        let summary = match message {
            Some(message) => message.to_owned(),
            None => {
                let files = file_changes(&self.saved_snapshot()?, &snapshot);
                if files.len() == 1 {
                    format!("{} {}", files[0].kind, files[0].path)
                } else {
                    format!("Update {} files", files.len())
                }
            }
        };
        ensure!(
            !summary.trim().is_empty(),
            "describe this version with a non-empty message"
        );
        let checkpoint = self.remember_snapshot(id, Some(&summary))?;
        tx.commit()?;
        Ok(checkpoint)
    }

    pub fn remember_snapshot(&self, snapshot: String, message: Option<&str>) -> Result<Checkpoint> {
        let view = SavedView {
            format: if self.state.paths.is_empty() { 1 } else { 2 },
            snapshot: snapshot.clone(),
            roots: self.projection()?.roots,
            paths: self.state.paths.clone(),
        };
        let hash = storage::put_json(&self.db, &self.state.project, "saved-view", &view)?;
        let created_at = now();
        self.db.execute(
            "INSERT INTO checkpoints(snapshot, message, created_at, view_id) VALUES (?1, ?2, ?3, ?4)",
            params![storage::hash_bytes(&snapshot)?, message, created_at, storage::hash_bytes(&hash)?],
        )?;
        let checkpoint = Checkpoint {
            id: self.db.last_insert_rowid(),
            view: hash,
            snapshot,
            message: message.map(str::to_owned),
            created_at,
            transaction: None,
        };
        Ok(checkpoint)
    }

    pub fn checkpoints(&self, limit: usize) -> Result<Vec<Checkpoint>> {
        let mut stmt = self.db.prepare(
            "SELECT id, lower(hex(snapshot)), message, created_at, CASE WHEN transaction_id IS NULL THEN NULL ELSE lower(hex(transaction_id)) END, lower(hex(view_id)) FROM checkpoints ORDER BY id DESC LIMIT ?1",
        )?;
        Ok(stmt
            .query_map([limit as i64], |r| {
                Ok(Checkpoint {
                    id: r.get(0)?,
                    snapshot: r.get(1)?,
                    message: r.get(2)?,
                    created_at: r.get(3)?,
                    transaction: r.get(4)?,
                    view: r.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?)
    }

    pub fn checkpoint_snapshot(&self, checkpoint: i64) -> Result<Snapshot> {
        let id: Option<String> = self
            .db
            .query_row(
                "SELECT lower(hex(snapshot)) FROM checkpoints WHERE id = ?1",
                [checkpoint],
                |r| r.get(0),
            )
            .optional()?;
        storage::get_json(
            &self.db,
            &self.state.project,
            "snapshot",
            &id.context("saved version does not exist; run `kelp log` to see your history")?,
        )
    }

    pub fn saved_snapshot(&self) -> Result<Snapshot> {
        self.baseline()
    }

    fn previous_snapshot(&self, version: i64) -> Result<Snapshot> {
        let id: Option<String> = self
            .db
            .query_row(
                "SELECT lower(hex(snapshot)) FROM checkpoints WHERE id < ?1 ORDER BY id DESC LIMIT 1",
                [version],
                |row| row.get(0),
            )
            .optional()?;
        match id {
            Some(id) => storage::get_json(&self.db, &self.state.project, "snapshot", &id),
            None => Ok(Snapshot::default()),
        }
    }

    pub fn history(&self, limit: usize) -> Result<Vec<HistoryEntry>> {
        self.checkpoints(limit)?
            .into_iter()
            .map(|version| {
                let files = file_changes(
                    &self.previous_snapshot(version.id)?,
                    &self.checkpoint_snapshot(version.id)?,
                );
                Ok(HistoryEntry { version, files })
            })
            .collect()
    }

    pub fn diff(&self) -> Result<Vec<FileDiff>> {
        let tx = self.db.unchecked_transaction()?;
        let current = self.scan(Some(&tx))?;
        self.compare(&self.saved_snapshot()?, &current)
    }

    pub fn current_snapshot(&self) -> Result<Snapshot> {
        self.scan(None)
    }

    pub fn show_transaction(&self, id: &str) -> Result<Vec<FileDiff>> {
        let (_, id) = self.resolve(&format!("commit:{id}"))?;
        let transaction = self.transaction(&id)?;
        let parent_transactions = transaction
            .dependencies()
            .into_iter()
            .map(|id| Ok((id.clone(), self.transaction(&id)?)))
            .collect::<Result<BTreeMap<_, _>>>()?;
        let mut result = Vec::new();
        for (path, edit) in &transaction.edits {
            if !self.includes(path) {
                result.push(FileDiff {
                    file: FileChange { path: path.clone(), kind: "not downloaded" },
                    diff: "Outside this checkout's path selection. The complete commit metadata is retained.\n".into(),
                });
                continue;
            }
            let mut after = Snapshot::default();
            if let Some(value) = &edit.value {
                after.files.insert(path.clone(), value.clone());
            }
            let parents: Vec<_> = if edit.parents.is_empty() {
                vec![None]
            } else {
                edit.parents.iter().map(Some).collect()
            };
            for parent in parents {
                let mut before = Snapshot::default();
                if let Some(parent) = parent
                    && let Some(value) = &parent_transactions[parent].edits[path].value
                {
                    before.files.insert(path.clone(), value.clone());
                }
                let mut diffs = self.compare(&before, &after)?;
                if edit.parents.len() > 1 {
                    for diff in &mut diffs {
                        diff.diff.insert_str(
                            0,
                            &format!("Compared with {}\n", parent.expect("multiple parents")),
                        );
                    }
                }
                result.extend(diffs);
            }
        }
        Ok(result)
    }

    pub fn show(&self, version: i64) -> Result<Vec<FileDiff>> {
        self.compare(
            &self.previous_snapshot(version)?,
            &self.checkpoint_snapshot(version)?,
        )
    }

    fn compare(&self, before: &Snapshot, after: &Snapshot) -> Result<Vec<FileDiff>> {
        file_changes(before, after)
            .into_iter()
            .map(|file| {
                let bytes = |snapshot: &Snapshot| -> Result<Vec<u8>> {
                    match snapshot.files.get(&file.path) {
                        Some(entry) => {
                            storage::get(&self.db, &self.state.project, "blob", &entry.blob)
                        }
                        None => Ok(Vec::new()),
                    }
                };
                let (old, new) = (bytes(before)?, bytes(after)?);
                let mut diff = match (std::str::from_utf8(&old), std::str::from_utf8(&new)) {
                    (Ok(old), Ok(new)) if !old.contains('\0') && !new.contains('\0') => {
                        similar::TextDiff::from_lines(old, new)
                            .unified_diff()
                            .header(
                                &format!("before/{}", file.path),
                                &format!("after/{}", file.path),
                            )
                            .to_string()
                    }
                    _ => format!("Binary contents: {} → {} bytes\n", old.len(), new.len()),
                };
                if before
                    .files
                    .get(&file.path)
                    .is_some_and(|entry| entry.executable)
                    != after
                        .files
                        .get(&file.path)
                        .is_some_and(|entry| entry.executable)
                {
                    diff.push_str(&format!(
                        "Executable: {} → {}\n",
                        before
                            .files
                            .get(&file.path)
                            .is_some_and(|entry| entry.executable),
                        after
                            .files
                            .get(&file.path)
                            .is_some_and(|entry| entry.executable)
                    ));
                }
                Ok(FileDiff { file, diff })
            })
            .collect()
    }

    pub fn status(&self) -> Result<Status> {
        let current = self.scan(None)?;
        let latest = self.checkpoints(1)?.pop();
        let baseline = self.saved_snapshot()?;
        let paths: BTreeSet<_> = current.files.keys().chain(baseline.files.keys()).collect();
        let changed = paths
            .into_iter()
            .filter(|path| current.files.get(*path) != baseline.files.get(*path))
            .cloned()
            .collect();
        let projection = self.projection()?;
        Ok(Status {
            project: self.state.project.clone(),
            remote: self.state.remote.clone(),
            view: projection.id()?,
            pending_commits: self.state.outbox.len(),
            changed,
            conflicts: projection
                .view
                .conflicts()
                .into_iter()
                .filter(|path| self.includes(path))
                .collect(),
            version: latest.map(|c| c.view),
            paths: self.state.paths.clone(),
        })
    }

    pub fn graph(&self) -> Result<Graph> {
        let transactions = self
            .state
            .transactions
            .iter()
            .map(|id| {
                Ok((
                    id.clone(),
                    storage::get_json(&self.db, &self.state.project, "transaction", id)?,
                ))
            })
            .collect::<Result<_>>()?;
        let graph = Graph { transactions };
        graph.validate()?;
        Ok(graph)
    }

    /// Save one atomic edit transaction; push will send these exact bytes later.
    pub fn commit(&mut self, message: &str) -> Result<Checkpoint> {
        let mut projection = self.projection()?;
        let tx = self.db.unchecked_transaction()?;
        let snapshot = self.scan(Some(&tx))?;
        let transaction = self.selected_view(&projection.view).record(
            &snapshot,
            message.into(),
            Uuid::new_v4().to_string(),
        )?;
        let bytes = serde_json::to_vec(&transaction)?;
        ensure!(
            bytes.len() <= MAX_METADATA_BYTES,
            "commit metadata exceeds 2 MiB"
        );
        let id = storage::put(&tx, &self.state.project, "transaction", &bytes)?;
        let snapshot_id = storage::put_json(&tx, &self.state.project, "snapshot", &snapshot)?;
        projection.apply(&id, &transaction);
        self.state.transactions.insert(id.clone());
        self.cache_projection(&projection)?;
        let mut saved = self.remember_snapshot(snapshot_id.clone(), Some(message))?;
        tx.execute(
            "UPDATE checkpoints SET transaction_id = ?1 WHERE id = ?2",
            params![storage::hash_bytes(&id)?, saved.id],
        )?;
        saved.transaction = Some(id.clone());
        self.state.outbox.insert(id);
        self.state.base_snapshot = snapshot_id;
        self.state.tracked = snapshot.files.keys().cloned().collect();
        self.save_state()?;
        tx.commit()?;
        Ok(saved)
    }

    pub fn acknowledge(&mut self, transaction: &str) -> Result<()> {
        self.state.outbox.remove(transaction);
        self.save_state()
    }

    pub fn set_remote(&mut self, url: String, project: String) -> Result<()> {
        validate_name(&project)?;
        let changed = self.state.remote.as_deref() != Some(&url) || project != self.state.project;
        if project != self.state.project {
            let tx = self.db.unchecked_transaction()?;
            storage::rename_namespace(&tx, &self.state.project, &project)?;
            self.state.project = project;
            self.state.remote = Some(url);
            self.save_state()?;
            tx.commit()?;
        } else {
            self.state.remote = Some(url);
            self.save_state()?;
        }
        if changed {
            self.state.outbox = self.state.transactions.clone();
            self.state.cursors.clear();
            self.state.layout = None;
            self.save_state()?;
        }
        Ok(())
    }

    pub fn baseline(&self) -> Result<Snapshot> {
        storage::get_json(
            &self.db,
            &self.state.project,
            "snapshot",
            &self.state.base_snapshot,
        )
    }

    pub fn includes(&self, path: &str) -> bool {
        selection::includes(&self.state.paths, path)
    }

    pub fn selected_view(&self, view: &View) -> View {
        View {
            files: view
                .files
                .iter()
                .filter(|(path, _)| self.includes(path))
                .map(|(path, heads)| (path.clone(), heads.clone()))
                .collect(),
        }
    }

    pub fn checkout_view(&self, view: &View) -> Result<View> {
        let selected = self.selected_view(view);
        let local_conflicts = selected.conflicts();
        let external: Vec<_> = view
            .conflicts()
            .into_iter()
            .filter(|path| self.includes(path) && !local_conflicts.contains(path))
            .collect();
        ensure!(
            external.is_empty(),
            "selected paths collide with files outside the checkout: {}",
            external.join(", ")
        );
        Ok(selected)
    }

    fn selected_snapshot(&self, snapshot: &Snapshot) -> Snapshot {
        Snapshot {
            files: snapshot
                .files
                .iter()
                .filter(|(path, _)| self.includes(path))
                .map(|(path, file)| (path.clone(), file.clone()))
                .collect(),
        }
    }

    fn scan(&self, store: Option<&Connection>) -> Result<Snapshot> {
        let transaction = if store.is_none() {
            Some(self.db.unchecked_transaction()?)
        } else {
            None
        };
        let cache = index::files(&self.db, &self.state.project)?;
        #[cfg(not(unix))]
        let baseline = self.baseline()?;
        #[cfg(not(unix))]
        let previous = self
            .checkpoints(1)?
            .pop()
            .map(|saved| self.checkpoint_snapshot(saved.id))
            .transpose()?
            .unwrap_or_default();
        let mut paths = discover_selected_files(&self.root, &self.state.paths)?;
        paths.extend(self.state.tracked.iter().cloned());
        paths.extend(self.baseline()?.files.into_keys());
        if let Some(last) = self.checkpoints(1)?.pop() {
            paths.extend(self.checkpoint_snapshot(last.id)?.files.into_keys());
        }
        paths.retain(|path| self.includes(path));
        let mut files = BTreeMap::new();
        let mut missing = Vec::new();
        let mut cache_updates = BTreeMap::new();
        for path in &paths {
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
                "only regular files are supported: {path}"
            );
            ensure!(
                metadata.len() <= MAX_BLOB_BYTES as u64,
                "{path} exceeds the 16 MiB file limit"
            );
            let fingerprint = index::fingerprint(&metadata);
            if let Some((fingerprint, changed_at)) = &fingerprint
                && let Some(cached) = cache.get(path)
                && cached.fingerprint == *fingerprint
                && *changed_at < cached.captured_at
                && storage::contains(&self.db, &self.state.project, "blob", &cached.entry.blob)?
            {
                files.insert(path.clone(), cached.entry.clone());
                continue;
            }
            #[cfg(unix)]
            let executable = executable(&metadata);
            #[cfg(not(unix))]
            let executable = baseline
                .files
                .get(path)
                .or_else(|| previous.files.get(path))
                .is_some_and(|entry| entry.executable);
            missing.push((path.clone(), full, metadata, executable));
        }
        // Overlap cold file-open latency, bounded independently of project size.
        // Two maximum-sized blobs or up to sixteen small files fit one batch.
        let mut offset = 0;
        while offset < missing.len() {
            let mut end = offset;
            let mut expected_bytes = 0;
            while end < missing.len() && end - offset < 16 {
                let bytes = missing[end].2.len() as usize;
                if end > offset && expected_bytes + bytes > 32 * 1024 * 1024 {
                    break;
                }
                expected_bytes += bytes;
                end += 1;
            }
            let results = std::thread::scope(|scope| {
                let jobs: Vec<_> = missing[offset..end]
                    .iter()
                    .map(|(path, full, metadata, executable)| {
                        scope.spawn(move || -> Result<_> {
                            let captured_at = index::timestamp();
                            let file = File::open(full).with_context(|| format!("read {path}"))?;
                            let mut bytes = Vec::with_capacity(metadata.len() as usize);
                            file.take(metadata.len() + 1).read_to_end(&mut bytes)?;
                            ensure!(
                                bytes.len() as u64 == metadata.len(),
                                "{path} changed size while being read; try again"
                            );
                            let fingerprint = index::fingerprint(metadata);
                            let stable =
                                fingerprint == index::fingerprint(&fs::symlink_metadata(full)?);
                            Ok((
                                path,
                                bytes,
                                *executable,
                                captured_at,
                                fingerprint.filter(|_| stable),
                            ))
                        })
                    })
                    .collect();
                jobs.into_iter()
                    .map(|job| {
                        job.join()
                            .map_err(|_| anyhow::anyhow!("file reader failed"))?
                    })
                    .collect::<Result<Vec<_>>>()
            })?;
            for (path, bytes, executable, captured_at, fingerprint) in results {
                let blob = storage::put(&self.db, &self.state.project, "blob", &bytes)?;
                let entry = FileEntry {
                    blob,
                    size: bytes.len() as u64,
                    executable,
                };
                if let Some((fingerprint, _)) = fingerprint {
                    cache_updates.insert(
                        path.clone(),
                        index::CachedFile {
                            fingerprint,
                            captured_at,
                            entry: entry.clone(),
                        },
                    );
                }
                files.insert(path.clone(), entry);
            }
            offset = end;
        }
        index::save_files(&self.db, &self.state.project, cache_updates)?;
        if let Some(transaction) = transaction {
            transaction.commit()?;
        }
        Ok(Snapshot { files })
    }

    pub fn resolve(&self, reference: &str) -> Result<(String, String)> {
        let (kind, prefix) = if let Some(prefix) = reference.strip_prefix("view:") {
            (Some("saved-view"), prefix)
        } else if let Some(prefix) = reference.strip_prefix("commit:") {
            (Some("transaction"), prefix)
        } else {
            (None, reference)
        };
        ensure!(
            (4..=64).contains(&prefix.len())
                && prefix
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "use a hash or at least four lowercase hexadecimal characters from `kelp log`"
        );
        let matches = storage::find(&self.db, &self.state.project, kind, prefix)?;
        ensure!(
            !matches.is_empty(),
            "hash {reference} is not available locally"
        );
        ensure!(
            matches.len() == 1,
            "hash {reference} is ambiguous; use more characters or a view:/commit: prefix"
        );
        Ok(matches.into_iter().next().unwrap())
    }

    pub fn short_hash(&self, hash: &str) -> Result<String> {
        for length in 12..=64 {
            if self.resolve(&hash[..length]).is_ok() {
                return Ok(hash[..length].into());
            }
        }
        Ok(hash.into())
    }

    pub fn show_reference(&self, reference: &str) -> Result<Vec<FileDiff>> {
        let (kind, hash) = self.resolve(reference)?;
        if kind == "transaction" {
            return self.show_transaction(&hash);
        }
        let version: Option<i64> = self
            .db
            .query_row(
                "SELECT id FROM checkpoints WHERE view_id = ?1 ORDER BY id LIMIT 1",
                [storage::hash_bytes(&hash)?],
                |row| row.get(0),
            )
            .optional()?;
        match version {
            Some(version) => self.show(version),
            None => {
                let view: SavedView =
                    storage::get_json(&self.db, &self.state.project, "saved-view", &hash)?;
                let snapshot =
                    storage::get_json(&self.db, &self.state.project, "snapshot", &view.snapshot)?;
                self.compare(&Snapshot::default(), &snapshot)
            }
        }
    }

    pub fn restore_reference(&mut self, reference: &str) -> Result<(String, String)> {
        let (kind, hash) = self.resolve(reference)?;
        ensure!(
            kind == "saved-view",
            "{reference} identifies edits, not a complete project view; choose a view hash from `kelp log`"
        );
        let view: SavedView =
            storage::get_json(&self.db, &self.state.project, "saved-view", &hash)?;
        ensure!(
            matches!(view.format, 1 | 2) && view.paths == self.state.paths,
            "this saved view covers a different path selection; restore it in a checkout with the same selection"
        );
        let target = storage::get_json(&self.db, &self.state.project, "snapshot", &view.snapshot)?;
        let backup = self.capture(Some(&format!(
            "Before restoring {}",
            self.short_hash(&hash)?
        )))?;
        self.apply_snapshot(&self.checkpoint_snapshot(backup.id)?, &target)?;
        self.state.tracked = target.files.keys().cloned().collect();
        self.save_state()?;
        self.capture(Some(&format!("Restored {}", self.short_hash(&hash)?)))?;
        Ok((hash, backup.view))
    }

    pub fn compact(&self) -> Result<storage::Compaction> {
        self.projection()?;
        index::compact(&self.db, &self.state.project, &self.state.base_snapshot)?;
        let mut keep = BTreeSet::new();
        if let Some(latest) = self.checkpoints(1)?.pop() {
            keep.insert(latest.view);
            if let Some(transaction) = latest.transaction {
                keep.insert(transaction);
            }
        }
        storage::compact(&self.db, &keep)
    }

    pub fn restore(&mut self, version: i64) -> Result<i64> {
        let target = self.checkpoint_snapshot(version)?;
        let backup = self.capture(Some(&format!("Before restoring version {version}")))?;
        let current = self.checkpoint_snapshot(backup.id)?;
        self.apply_snapshot(&current, &target)?;
        self.state.tracked = target.files.keys().cloned().collect();
        self.save_state()?;
        self.capture(Some(&format!("Restored version {version}")))?;
        Ok(backup.id)
    }

    /// Apply verified file bytes in place, retaining ignored files and .kelp.
    /// The caller records the current snapshot before replacing any files.
    pub fn apply_snapshot(&self, current: &Snapshot, target: &Snapshot) -> Result<()> {
        let selected_current = self.selected_snapshot(current);
        let selected_target = self.selected_snapshot(target);
        let (current, target) = (&selected_current, &selected_target);
        storage::verify_snapshot(&self.db, &self.state.project, target)?;
        // Check the target's names on this filesystem before changing real files.
        let shape = tempfile::tempdir_in(self.root.join(".kelp"))?;
        let mut directories = BTreeSet::new();
        for path in target.files.keys() {
            let mut prefix = PathBuf::new();
            let parts: Vec<_> = path.split('/').collect();
            for part in &parts[..parts.len() - 1] {
                prefix.push(part);
                if directories.insert(prefix.clone()) {
                    fs::create_dir(shape.path().join(&prefix)).with_context(|| {
                        format!(
                            "this filesystem cannot represent directory {}",
                            prefix.display()
                        )
                    })?;
                }
            }
            File::create_new(shape.path().join(path))
                .with_context(|| format!("this filesystem cannot represent file {path}"))?;
        }
        ensure!(
            self.scan(None)? == *current,
            "files changed while preparing the update; try again"
        );
        for path in target.files.keys() {
            check_ancestors(&self.root, path)?;
            if !current.files.contains_key(path)
                && fs::symlink_metadata(self.root.join(path)).is_ok()
            {
                bail!("cannot replace an ignored file or directory at {path}");
            }
        }
        for path in current
            .files
            .keys()
            .filter(|path| !target.files.contains_key(*path))
        {
            fs::remove_file(self.root.join(path))?;
        }
        for (path, entry) in &target.files {
            if current.files.get(path) == Some(entry) {
                continue;
            }
            let full = self.root.join(path);
            let parent = full.parent().context("file has no parent directory")?;
            fs::create_dir_all(parent)?;
            let mut file = tempfile::NamedTempFile::new_in(parent)?;
            file.write_all(&storage::get(
                &self.db,
                &self.state.project,
                "blob",
                &entry.blob,
            )?)?;
            set_executable(file.as_file(), entry.executable)?;
            file.as_file().sync_all()?;
            file.persist(&full)
                .with_context(|| format!("write {path}"))?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Default)]
pub enum Resolution {
    #[default]
    Stop,
    Local,
    Remote,
}

pub fn merge_snapshots(
    base: &Snapshot,
    local: &Snapshot,
    remote: &Snapshot,
    resolution: Resolution,
) -> Result<Snapshot> {
    let paths: BTreeSet<_> = base
        .files
        .keys()
        .chain(local.files.keys())
        .chain(remote.files.keys())
        .collect();
    let mut files = BTreeMap::new();
    let mut conflicts = Vec::new();
    for path in paths {
        let (before, ours, theirs) = (
            base.files.get(path),
            local.files.get(path),
            remote.files.get(path),
        );
        let selected = if ours == before {
            theirs
        } else if theirs == before || ours == theirs {
            ours
        } else {
            match resolution {
                Resolution::Local => ours,
                Resolution::Remote => theirs,
                Resolution::Stop => {
                    conflicts.push(path.as_str());
                    continue;
                }
            }
        };
        if let Some(entry) = selected {
            files.insert(path.clone(), entry.clone());
        }
    }
    ensure!(
        conflicts.is_empty(),
        "both you and the remote edited: {}. Your files are unchanged. Use `kelp pull --keep-local` or `kelp pull --keep-remote` to choose those files' versions",
        conflicts.join(", ")
    );
    let snapshot = Snapshot { files };
    snapshot.validate()?;
    Ok(snapshot)
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
    discover_selected_files(root, &[])
}

fn discover_selected_files(root: &Path, paths: &[String]) -> Result<BTreeSet<String>> {
    let mut files = BTreeSet::new();
    let mut walker = WalkBuilder::new(root);
    let selected = paths.to_vec();
    let directory = root.to_owned();
    walker
        .hidden(false)
        .require_git(false)
        .parents(false)
        .git_global(false)
        .follow_links(false)
        .add_custom_ignore_filename(".kelpignore")
        .filter_entry(move |entry| {
            if matches!(entry.file_name().to_str(), Some(".kelp" | ".git")) {
                return false;
            }
            if entry.depth() == 0 || selected.is_empty() {
                return true;
            }
            let Some(path) = entry
                .path()
                .strip_prefix(&directory)
                .ok()
                .and_then(|path| path.to_str())
            else {
                return false;
            };
            let path = path.replace(std::path::MAIN_SEPARATOR, "/");
            if entry.file_type().is_some_and(|kind| kind.is_dir()) {
                selection::intersects(&selected, &path)
            } else {
                selection::includes(&selected, &path)
            }
        });
    for entry in walker.build() {
        let entry = entry?;
        if entry.file_type().is_some_and(|kind| kind.is_dir()) {
            continue;
        }
        let path = entry
            .path()
            .strip_prefix(root)?
            .to_str()
            .context("file paths must be UTF-8")?
            .replace(std::path::MAIN_SEPARATOR, "/");
        validate_path(&path)?;
        ensure!(
            entry.file_type().is_some_and(|kind| kind.is_file()),
            "symlinks and special files are not supported: {path}"
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

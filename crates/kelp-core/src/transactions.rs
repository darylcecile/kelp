//! Atomic, file-scoped changes. Project state is a dependency-closed set, not a
//! chain of repository-wide commits. Concurrent values are retained explicitly.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::{
    FileEntry, MAX_BLOB_BYTES, Snapshot, object_id, validate_hash, validate_name, validate_path,
};

pub const PROTOCOL: &str = "kelp/1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Edit {
    /// Only the preceding versions of this file, including deletion tombstones.
    pub parents: BTreeSet<String>,
    pub value: Option<FileEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Transaction {
    pub format: u32,
    pub nonce: String,
    pub message: String,
    pub edits: BTreeMap<String, Edit>,
}

impl Transaction {
    pub fn id(&self) -> Result<String> {
        Ok(object_id("transaction", &serde_json::to_vec(self)?))
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(self.format == 1, "unsupported transaction format");
        validate_name(&self.nonce)?;
        ensure!(
            !self.message.trim().is_empty() && self.message.len() <= 16_384,
            "describe the commit in 1–16384 bytes"
        );
        for (path, edit) in &self.edits {
            validate_path(path)?;
            for parent in &edit.parents {
                validate_hash(parent)?;
            }
            if let Some(value) = &edit.value {
                validate_hash(&value.blob)?;
                ensure!(
                    value.size <= MAX_BLOB_BYTES as u64,
                    "file too large: {path}"
                );
            }
        }
        Ok(())
    }

    pub fn dependencies(&self) -> BTreeSet<String> {
        self.edits
            .values()
            .flat_map(|edit| edit.parents.iter().cloned())
            .collect()
    }

    pub fn validate_parents(&self, parents: &BTreeMap<String, Transaction>) -> Result<()> {
        self.validate()?;
        for id in self.dependencies() {
            let parent = parents
                .get(&id)
                .with_context(|| format!("missing dependency {id}"))?;
            ensure!(parent.id()? == id, "dependency ID mismatch");
        }
        for (path, edit) in &self.edits {
            for id in &edit.parents {
                let parent = parents
                    .get(id)
                    .with_context(|| format!("missing dependency {id}"))?;
                ensure!(
                    parent.edits.contains_key(path),
                    "dependency {id} did not write {path}"
                );
            }
        }
        Ok(())
    }
}

/// A path can retain several concurrent values. None is a real deletion value.
pub type FileHeads = BTreeMap<String, Option<FileEntry>>;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct View {
    pub files: BTreeMap<String, FileHeads>,
}

impl View {
    /// Call only after dependencies have been validated and applied. A consumed
    /// parent stays absent even when a concurrent sibling arrives later.
    pub fn apply(&mut self, id: &str, transaction: &Transaction) {
        for (path, edit) in &transaction.edits {
            let heads = self.files.entry(path.clone()).or_default();
            for parent in &edit.parents {
                heads.remove(parent);
            }
            heads.insert(id.into(), edit.value.clone());
        }
    }

    pub fn record(
        &self,
        snapshot: &Snapshot,
        message: String,
        nonce: String,
    ) -> Result<Transaction> {
        snapshot.validate()?;
        let conflicts = self.conflicts();
        let paths: BTreeSet<_> = self.files.keys().chain(snapshot.files.keys()).collect();
        let edits = paths
            .into_iter()
            .filter_map(|path| {
                let heads = self.files.get(path);
                let old = heads
                    .and_then(|heads| heads.values().next())
                    .and_then(Option::as_ref);
                let value = snapshot.files.get(path);
                if old == value && !conflicts.contains(path) {
                    return None;
                }
                Some((
                    path.clone(),
                    Edit {
                        parents: heads
                            .map(|heads| heads.keys().cloned().collect())
                            .unwrap_or_default(),
                        value: value.cloned(),
                    },
                ))
            })
            .collect();
        let transaction = Transaction {
            format: 1,
            nonce,
            message,
            edits,
        };
        transaction.validate()?;
        Ok(transaction)
    }
    pub fn conflicts(&self) -> BTreeSet<String> {
        let mut conflicts = BTreeSet::new();
        for (path, heads) in &self.files {
            if let Some(first) = heads.values().next()
                && heads.values().any(|value| value != first)
            {
                conflicts.insert(path.clone());
            }
            if heads.values().any(Option::is_some) {
                let mut prefix = path.as_str();
                while let Some((parent, _)) = prefix.rsplit_once('/') {
                    if self
                        .files
                        .get(parent)
                        .is_some_and(|values| values.values().any(Option::is_some))
                    {
                        conflicts.insert(parent.into());
                        conflicts.insert(path.clone());
                    }
                    prefix = parent;
                }
            }
        }
        conflicts
    }

    pub fn snapshot(&self) -> Result<Snapshot> {
        let conflicts = self.conflicts();
        ensure!(
            conflicts.is_empty(),
            "conflicting files: {}",
            conflicts.into_iter().collect::<Vec<_>>().join(", ")
        );
        let snapshot = Snapshot {
            files: self
                .files
                .iter()
                .filter_map(|(path, heads)| {
                    heads
                        .values()
                        .next()
                        .and_then(|value| value.clone())
                        .map(|value| (path.clone(), value))
                })
                .collect(),
        };
        snapshot.validate()?;
        Ok(snapshot)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Graph {
    pub transactions: BTreeMap<String, Transaction>,
}

impl Graph {
    pub fn validate(&self) -> Result<()> {
        for (id, transaction) in &self.transactions {
            ensure!(transaction.id()? == *id, "transaction ID mismatch");
            transaction.validate_parents(&self.transactions)?;
        }
        self.ordered()?;
        Ok(())
    }

    /// Dependencies first; hashes only break ties between independent changes.
    pub fn ordered(&self) -> Result<Vec<String>> {
        self.ordered_after(&BTreeSet::new())
    }

    /// Sort only new transactions when the dependency boundary is already known.
    pub fn ordered_after(&self, known: &BTreeSet<String>) -> Result<Vec<String>> {
        let mut remaining = BTreeMap::new();
        let mut children: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut ready = BTreeSet::new();
        for (id, transaction) in &self.transactions {
            let parents: BTreeSet<_> = transaction
                .dependencies()
                .difference(known)
                .cloned()
                .collect();
            remaining.insert(id.clone(), parents.len());
            if parents.is_empty() {
                ready.insert(id.clone());
            }
            for parent in parents {
                children.entry(parent).or_default().push(id.clone());
            }
        }
        let mut ordered = Vec::new();
        while let Some(id) = ready.pop_first() {
            if let Some(dependents) = children.get(&id) {
                for child in dependents {
                    let count = remaining.get_mut(child).expect("known child");
                    *count -= 1;
                    if *count == 0 {
                        ready.insert(child.clone());
                    }
                }
            }
            ordered.push(id);
        }
        ensure!(
            ordered.len() == self.transactions.len(),
            "transaction graph has missing dependencies or a cycle"
        );
        Ok(ordered)
    }

    /// Two passes make projection independent of arrival or traversal order.
    pub fn view(&self) -> Result<View> {
        self.validate()?;
        let mut view = View::default();
        for (id, tx) in &self.transactions {
            for (path, edit) in &tx.edits {
                view.files
                    .entry(path.clone())
                    .or_default()
                    .insert(id.clone(), edit.value.clone());
            }
        }
        for tx in self.transactions.values() {
            for (path, edit) in &tx.edits {
                let heads = view
                    .files
                    .get_mut(path)
                    .expect("path inserted in first pass");
                for parent in &edit.parents {
                    heads.remove(parent);
                }
            }
        }
        Ok(view)
    }

    pub fn roots(&self) -> BTreeSet<String> {
        let dependencies: BTreeSet<_> = self
            .transactions
            .values()
            .flat_map(Transaction::dependencies)
            .collect();
        self.transactions
            .keys()
            .filter(|id| !dependencies.contains(*id))
            .cloned()
            .collect()
    }

    pub fn view_id(&self) -> Result<String> {
        Ok(object_id("view", &serde_json::to_vec(&self.roots())?))
    }

    pub fn record(
        &self,
        snapshot: &Snapshot,
        message: String,
        nonce: String,
    ) -> Result<Transaction> {
        self.view()?.record(snapshot, message, nonce)
    }
}

/// An exact, restorable local state, including its causal context. Recovery
/// snapshots can contain draft bytes without inventing new transactions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedView {
    pub format: u32,
    pub snapshot: String,
    pub roots: BTreeSet<String>,
}

impl SavedView {
    pub fn id(&self) -> Result<String> {
        Ok(object_id("saved-view", &serde_json::to_vec(self)?))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub project: String,
    pub protocol: String,
    pub layout: String,
    pub storage_nodes: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncRequest {
    pub cursors: Vec<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncPage {
    pub transactions: BTreeSet<String>,
    pub cursors: Vec<i64>,
    pub more: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalPage {
    pub transactions: Vec<String>,
    pub cursor: i64,
    pub more: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Receipt {
    pub transaction: String,
}

/// A fixed topology maps content and its journal receipt to the same owner.
pub fn partition(project: &str, kind: &str, id: &str, nodes: usize) -> usize {
    assert!(nodes > 0);
    let key = object_id("placement", format!("{project}\0{kind}\0{id}").as_bytes());
    (u64::from_str_radix(&key[..16], 16).expect("hash is hexadecimal") % nodes as u64) as usize
}

use std::collections::BTreeSet;

use anyhow::Result;
use kelp_core::{
    EntryKind, FileEntry, MAX_BLOB_BYTES, Snapshot, content,
    transactions::{FileHeads, Graph},
};

use crate::workspace::{Resolution, Workspace, merge_snapshots};

pub fn text(
    workspace: &Workspace,
    base: Option<&FileEntry>,
    ours: Option<&FileEntry>,
    theirs: Option<&FileEntry>,
) -> Result<Option<FileEntry>> {
    let (Some(ours), Some(theirs)) = (ours, theirs) else {
        return Ok(None);
    };
    if base
        .into_iter()
        .chain([ours, theirs])
        .any(|entry| entry.kind != EntryKind::Regular || entry.size > MAX_BLOB_BYTES as u64)
    {
        return Ok(None);
    }
    let read = |entry: Option<&FileEntry>| -> Result<Option<String>> {
        let bytes = entry
            .map(|entry| content::read(&workspace.db, &workspace.state.project, entry))
            .transpose()?
            .unwrap_or_default();
        Ok(String::from_utf8(bytes)
            .ok()
            .filter(|text| !text.contains('\0')))
    };
    let (Some(before), Some(local), Some(remote)) =
        (read(base)?, read(Some(ours))?, read(Some(theirs))?)
    else {
        return Ok(None);
    };
    let Ok(merged) = diffy::merge(&before, &local, &remote) else {
        return Ok(None);
    };
    let executable = if base.is_some_and(|entry| entry.executable == ours.executable) {
        theirs.executable
    } else {
        ours.executable
    };
    Ok(Some(content::store(
        &workspace.db,
        &workspace.state.project,
        merged.as_bytes(),
        merged.len() as u64,
        executable,
    )?))
}

fn ancestors(graph: &Graph, path: &str, id: &str) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    let mut todo = vec![id.to_owned()];
    while let Some(id) = todo.pop() {
        if seen.insert(id.clone())
            && let Some(edit) = graph
                .transactions
                .get(&id)
                .and_then(|transaction| transaction.edits.get(path))
        {
            todo.extend(edit.parents.iter().cloned());
        }
    }
    seen
}

pub fn heads(
    workspace: &Workspace,
    graph: &Graph,
    path: &str,
    heads: &FileHeads,
) -> Result<Option<FileEntry>> {
    let mut ancestry = heads.keys().map(|id| ancestors(graph, path, id));
    let mut common = ancestry.next().unwrap_or_default();
    for ancestors in ancestry {
        common = common.intersection(&ancestors).cloned().collect();
    }
    let mut nearest = common.clone();
    for id in &common {
        for ancestor in ancestors(graph, path, id)
            .into_iter()
            .filter(|parent| parent != id)
        {
            nearest.remove(&ancestor);
        }
    }
    // Multiple merge bases need a virtual ancestor; retain alternatives instead
    // of guessing which base represents both sides.
    if nearest.len() > 1 {
        return Ok(None);
    }
    let base = nearest
        .first()
        .and_then(|id| graph.transactions[id].edits[path].value.as_ref());
    let mut values = heads.values();
    let mut merged = values.next().cloned().flatten();
    for value in values {
        if merged == *value {
            continue;
        }
        merged = text(workspace, base, merged.as_ref(), value.as_ref())?;
        if merged.is_none() {
            return Ok(None);
        }
    }
    Ok(merged)
}

pub fn working(
    workspace: &Workspace,
    base: &Snapshot,
    local: &Snapshot,
    incoming: &Snapshot,
    resolution: Resolution,
) -> Result<Snapshot> {
    let mut ours = local.clone();
    let mut theirs = incoming.clone();
    for (path, local) in &local.files {
        let remote = incoming.files.get(path);
        let before = base.files.get(path);
        if Some(local) != before
            && remote != before
            && Some(local) != remote
            && let Some(merged) = text(workspace, before, Some(local), remote)?
        {
            ours.files.insert(path.clone(), merged.clone());
            theirs.files.insert(path.clone(), merged);
        }
    }
    merge_snapshots(base, &ours, &theirs, resolution)
}

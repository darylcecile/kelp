use std::collections::BTreeSet;

use anyhow::{Result, ensure};

/// Project-relative files or directory prefixes, never filesystem globs.
/// An empty selection means the whole project.
pub fn normalize(paths: Vec<String>) -> Result<Vec<String>> {
    let mut all = false;
    let mut selected = BTreeSet::new();
    for path in paths {
        if path == "." || path == "./" {
            all = true;
            continue;
        }
        let path = path
            .strip_prefix("./")
            .unwrap_or(&path)
            .trim_end_matches('/');
        if path == "." {
            all = true;
            continue;
        }
        ensure!(
            !path.is_empty(),
            "--paths needs a project-relative file or directory"
        );
        kelp_core::validate_path(path)?;
        selected.insert(path.to_owned());
    }
    if all {
        return Ok(Vec::new());
    }
    let mut result = Vec::new();
    for path in selected {
        if !result.iter().any(|parent: &String| within(parent, &path)) {
            result.push(path);
        }
    }
    Ok(result)
}

fn within(parent: &str, path: &str) -> bool {
    path == parent
        || path
            .strip_prefix(parent)
            .is_some_and(|rest| rest.starts_with('/'))
}

pub fn includes(selection: &[String], path: &str) -> bool {
    selection.is_empty() || selection.iter().any(|parent| within(parent, path))
}

pub fn intersects(selection: &[String], path: &str) -> bool {
    includes(selection, path) || selection.iter().any(|child| within(path, child))
}

use std::collections::{BTreeMap, BTreeSet};

use kelp_core::{
    FileEntry, Snapshot, object_id,
    transactions::{Graph, Transaction},
};

fn file(value: &str) -> FileEntry {
    FileEntry {
        blob: object_id("blob", value.as_bytes()),
        size: value.len() as u64,
        executable: false,
    }
}

fn record(graph: &Graph, files: &[(&str, &str)], nonce: &str) -> Transaction {
    graph
        .record(
            &Snapshot {
                files: files
                    .iter()
                    .map(|(path, value)| ((*path).into(), file(value)))
                    .collect(),
            },
            nonce.into(),
            nonce.into(),
        )
        .unwrap()
}

#[test]
fn independent_changes_have_no_repository_parent_and_commute() -> anyhow::Result<()> {
    let empty = Graph::default();
    let a = record(&empty, &[("a", "one")], "a");
    let b = record(&empty, &[("b", "two")], "b");
    assert!(a.dependencies().is_empty() && b.dependencies().is_empty());
    let graph = Graph {
        transactions: [(b.id()?, b.clone()), (a.id()?, a.clone())].into(),
    };
    let reverse = Graph {
        transactions: [(a.id()?, a), (b.id()?, b)].into(),
    };
    assert_eq!(graph.view_id()?, reverse.view_id()?);
    assert_eq!(graph.view()?.snapshot()?, reverse.view()?.snapshot()?);
    assert_eq!(graph.view()?.snapshot()?.files.len(), 2);
    let next = record(&graph, &[("a", "updated"), ("b", "two")], "next");
    assert_eq!(next.dependencies().len(), 1);
    assert_eq!(next.edits.keys().collect::<Vec<_>>(), ["a"]);
    Ok(())
}

#[test]
fn concurrent_values_and_deletions_survive_until_explicit_resolution() -> anyhow::Result<()> {
    let start = record(&Graph::default(), &[("file", "base")], "start");
    let base = Graph {
        transactions: [(start.id()?, start)].into(),
    };
    let edited = record(&base, &[("file", "edit")], "edited");
    let deleted = record(&base, &[], "deleted");
    let mut graph = base;
    graph.transactions.insert(edited.id()?, edited.clone());
    graph.transactions.insert(deleted.id()?, deleted.clone());
    assert_eq!(graph.view()?.conflicts(), BTreeSet::from(["file".into()]));
    assert!(graph.view()?.snapshot().is_err());
    assert_eq!(graph.view()?.files["file"].len(), 2);
    let resolution = record(&graph, &[("file", "resolved")], "resolution");
    assert_eq!(
        resolution.edits["file"].parents,
        BTreeSet::from([edited.id()?, deleted.id()?])
    );
    graph.transactions.insert(resolution.id()?, resolution);
    assert_eq!(graph.view()?.snapshot()?.files["file"], file("resolved"));
    Ok(())
}

#[test]
fn multi_file_changes_are_indivisible_and_require_complete_dependencies() -> anyhow::Result<()> {
    let pair = record(
        &Graph::default(),
        &[("api", "v2"), ("caller", "v2")],
        "pair",
    );
    let base = Graph {
        transactions: [(pair.id()?, pair.clone())].into(),
    };
    let next = record(&base, &[("api", "v3"), ("caller", "v2")], "next");
    let mut graph = Graph {
        transactions: [(next.id()?, next)].into(),
    };
    assert!(graph.view().is_err());
    graph.transactions.insert(pair.id()?, pair);
    let snapshot = graph.view()?.snapshot()?;
    assert_eq!(snapshot.files["api"], file("v3"));
    assert_eq!(snapshot.files["caller"], file("v2"));
    Ok(())
}

#[test]
fn path_collisions_are_conflicts_and_missing_dependency_writes_are_rejected() -> anyhow::Result<()>
{
    let a = record(&Graph::default(), &[("a", "file")], "a");
    let b = record(&Graph::default(), &[("a/b", "child")], "b");
    let graph = Graph {
        transactions: [(a.id()?, a.clone()), (b.id()?, b)].into(),
    };
    assert_eq!(graph.view()?.conflicts().len(), 2);
    let mut invalid = record(&Graph::default(), &[("other", "bad")], "bad");
    invalid
        .edits
        .get_mut("other")
        .unwrap()
        .parents
        .insert(a.id()?);
    assert!(
        invalid
            .validate_parents(&BTreeMap::from([(a.id()?, a)]))
            .is_err()
    );
    Ok(())
}

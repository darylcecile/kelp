use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    process::Command,
};

use kelp_core::{
    FileEntry, object_id,
    transactions::{Edit, Receipt, SyncPage, SyncRequest, Transaction, partition},
};
use serde_json::Value;

struct Servers(Vec<tokio::task::JoinHandle<std::io::Result<()>>>);
impl Drop for Servers {
    fn drop(&mut self) {
        for server in &self.0 {
            server.abort();
        }
    }
}

async fn start(app: axum::Router, servers: &mut Servers) -> anyhow::Result<String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    servers.0.push(tokio::spawn(
        async move { axum::serve(listener, app).await },
    ));
    Ok(url)
}

fn cli(path: &Path, args: &[&str]) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_kelp"))
        .arg("-C")
        .arg(path)
        .arg("--json")
        .args(args)
        .env("KELP_TOKEN", "client-token")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replicas_survive_node_loss_and_retry_committed_work() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let mut servers = Servers(Vec::new());
    let mut nodes = Vec::new();
    for index in 0..3 {
        nodes.push(
            start(
                kelp_remote::storage_app(
                    &root.path().join(format!("node{index}")),
                    "storage-token".into(),
                )?,
                &mut servers,
            )
            .await?,
        );
    }
    let gateway = start(
        kelp_remote::replicated_app(nodes, "client-token".into(), "storage-token".into(), 2)?,
        &mut servers,
    )
    .await?;
    let url = format!("{gateway}/demo");
    cli(root.path(), &["init", "alice", "--remote", &url]);
    let alice = root.path().join("alice");
    fs::write(alice.join("file"), "initial")?;
    let saved = cli(&alice, &["commit", "-m", "Initial"]);
    cli(&alice, &["tag", "v1.0"]);
    cli(&alice, &["push"]);
    servers.0[0].abort();
    tokio::task::yield_now().await;
    cli(root.path(), &["clone", &url, "bob"]);
    assert_eq!(fs::read(root.path().join("bob/file"))?, b"initial");
    assert_eq!(
        cli(&root.path().join("bob"), &["tag"])[0]["view"],
        saved["view"]
    );
    fs::write(alice.join("file"), "after one node disappeared")?;
    cli(&alice, &["commit", "-m", "While degraded"]);
    cli(&alice, &["push"]);
    cli(&root.path().join("bob"), &["pull"]);
    assert_eq!(
        fs::read(root.path().join("bob/file"))?,
        b"after one node disappeared"
    );
    cli(&root.path().join("bob"), &["restore", "v1.0"]);
    assert_eq!(fs::read(root.path().join("bob/file"))?, b"initial");
    servers.0[1].abort();
    tokio::task::yield_now().await;
    let response = reqwest::Client::new()
        .post(format!("{gateway}/v1/projects/demo/sync"))
        .bearer_auth("client-token")
        .json(&SyncRequest::default())
        .send()
        .await?;
    assert!(
        !response.status().is_success(),
        "losing too many replicas must not look like an empty view"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn evacuation_drains_and_preserves_history_blobs_and_release_pins() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let mut servers = Servers(Vec::new());
    let mut nodes = Vec::new();
    for index in 0..4 {
        nodes.push(
            start(
                kelp_remote::storage_app(
                    &root.path().join(format!("node{index}")),
                    "storage-token".into(),
                )?,
                &mut servers,
            )
            .await?,
        );
    }
    let gateway = start(
        kelp_remote::replicated_app(
            nodes.clone(),
            "client-token".into(),
            "storage-token".into(),
            2,
        )?,
        &mut servers,
    )
    .await?;
    let url = format!("{gateway}/demo");
    cli(root.path(), &["init", "alice", "--remote", &url]);
    let alice = root.path().join("alice");
    for i in 0..8 {
        fs::write(alice.join(format!("file{i}")), format!("version {i}"))?;
        cli(&alice, &["commit", "-m", "Add a file"]);
    }
    cli(&alice, &["tag", "v1.0"]);
    cli(&alice, &["push"]);
    let report = kelp_remote::maintenance::maintain(
        nodes.clone(),
        "storage-token".into(),
        2,
        Some(nodes[0].clone()),
    )
    .await?;
    assert!(report.objects > 0);
    assert_eq!(report.nodes.len(), 3);
    let bytes = b"must not enter drained node";
    let response = reqwest::Client::new()
        .put(format!(
            "{}/storage/projects/demo/objects/blob/{}",
            nodes[0],
            object_id("blob", bytes)
        ))
        .bearer_auth("storage-token")
        .body(bytes.as_slice())
        .send()
        .await?;
    assert!(!response.status().is_success());
    servers.0[0].abort();
    let new_gateway = start(
        kelp_remote::replicated_app(
            report.nodes.clone(),
            "client-token".into(),
            "storage-token".into(),
            2,
        )?,
        &mut servers,
    )
    .await?;
    cli(
        root.path(),
        &["clone", &format!("{new_gateway}/demo"), "after"],
    );
    let after = root.path().join("after");
    assert_eq!(cli(&after, &["tag"])[0]["name"], "v1.0");
    for i in 0..8 {
        assert_eq!(
            fs::read(after.join(format!("file{i}")))?,
            format!("version {i}").as_bytes()
        );
    }
    kelp_remote::maintenance::maintain(report.nodes, "storage-token".into(), 2, None).await?;
    fs::write(after.join("new"), "after evacuation")?;
    cli(&after, &["commit", "-m", "New topology"]);
    cli(&after, &["push"]);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_project_spans_three_stores_and_two_stateless_gateways() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let mut servers = Servers(Vec::new());
    let mut nodes = Vec::new();
    let mut directories = Vec::new();
    for index in 0..3 {
        let directory = root.path().join(format!("node-{index}"));
        nodes.push(
            start(
                kelp_remote::storage_app(&directory, "storage-token".into())?,
                &mut servers,
            )
            .await?,
        );
        directories.push(directory);
    }
    let a = start(
        kelp_remote::cluster_app(nodes.clone(), "client-token".into(), "storage-token".into())?,
        &mut servers,
    )
    .await?;
    let b = start(
        kelp_remote::cluster_app(nodes.clone(), "client-token".into(), "storage-token".into())?,
        &mut servers,
    )
    .await?;
    let a_project = format!("{a}/demo");
    let b_project = format!("{b}/demo");
    cli(root.path(), &["init", "alice", "--remote", &a_project]);
    let alice = root.path().join("alice");
    fs::write(alice.join("api"), "v1")?;
    fs::write(alice.join("caller"), "v1")?;
    let initial = cli(&alice, &["commit", "-m", "Initial API and caller"])["transaction"]
        .as_str()
        .unwrap()
        .to_owned();
    cli(&alice, &["push"]);
    cli(root.path(), &["clone", &b_project, "bob"]);
    let bob = root.path().join("bob");
    fs::write(alice.join("alice.txt"), "Alice")?;
    fs::write(bob.join("bob.txt"), "Bob")?;
    let a_commit = cli(&alice, &["commit", "-m", "Alice's independent work"])["transaction"]
        .as_str()
        .unwrap()
        .to_owned();
    let b_commit = cli(&bob, &["commit", "-m", "Bob's independent work"])["transaction"]
        .as_str()
        .unwrap()
        .to_owned();
    std::thread::scope(|scope| {
        let a = scope.spawn(|| cli(&alice, &["push"]));
        let b = scope.spawn(|| cli(&bob, &["push"]));
        assert_eq!(a.join().unwrap()["pushed"], 1);
        assert_eq!(b.join().unwrap()["pushed"], 1);
    });
    let client = reqwest::Client::new();
    assert_eq!(
        client
            .get(format!("{}/storage/projects/demo", nodes[0]))
            .bearer_auth("client-token")
            .send()
            .await?
            .status(),
        401
    );
    // Fixed, independent batches exercise every journal/content partition.
    let mut tasks = tokio::task::JoinSet::new();
    for index in 0..36 {
        let client = client.clone();
        let gateway = if index % 2 == 0 { a.clone() } else { b.clone() };
        tasks.spawn(async move {
            let bytes = format!("partitioned payload {index}").into_bytes();
            let blob = object_id("blob", &bytes);
            client
                .put(format!("{gateway}/v1/projects/demo/objects/blob/{blob}"))
                .bearer_auth("client-token")
                .body(bytes.clone())
                .send()
                .await?
                .error_for_status()?;
            let transaction = Transaction {
                provenance: None,
                format: 1,
                nonce: format!("batch-{index}"),
                message: format!("Independent batch {index}"),
                edits: BTreeMap::from([
                    (
                        format!("batch/{index}/api"),
                        Edit {
                            parents: BTreeSet::new(),
                            value: Some(FileEntry {
                                blob: blob.clone(),
                                size: bytes.len() as u64,
                                executable: false,
                                kind: Default::default(),
                            }),
                        },
                    ),
                    (
                        format!("batch/{index}/caller"),
                        Edit {
                            parents: BTreeSet::new(),
                            value: Some(FileEntry {
                                blob,
                                size: bytes.len() as u64,
                                executable: false,
                                kind: Default::default(),
                            }),
                        },
                    ),
                ]),
            };
            let receipt: Receipt = client
                .post(format!("{gateway}/v1/projects/demo/transactions"))
                .bearer_auth("client-token")
                .json(&transaction)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            assert_eq!(receipt.transaction, transaction.id()?);
            Ok::<_, anyhow::Error>(receipt)
        });
    }
    while let Some(result) = tasks.join_next().await {
        result??;
    }
    let bytes = b"v2";
    let blob = object_id("blob", bytes);
    client
        .put(format!("{b}/v1/projects/demo/objects/blob/{blob}"))
        .bearer_auth("client-token")
        .body(bytes.to_vec())
        .send()
        .await?
        .error_for_status()?;
    let mut dependent = Transaction {
        provenance: None,
        format: 1,
        nonce: "dependent".into(),
        message: "Update both halves across a shard dependency".into(),
        edits: ["api", "caller"]
            .into_iter()
            .map(|path| {
                (
                    path.into(),
                    Edit {
                        parents: BTreeSet::from([initial.clone()]),
                        value: Some(FileEntry {
                            blob: blob.clone(),
                            size: 2,
                            executable: false,
                            kind: Default::default(),
                        }),
                    },
                )
            })
            .collect(),
    };
    let parent_owner = partition("demo", "transaction", &initial, 3);
    let mut nonce = 0;
    while partition("demo", "transaction", &dependent.id()?, 3) == parent_owner {
        nonce += 1;
        dependent.nonce = format!("dependent-{nonce}");
    }
    client
        .post(format!("{b}/v1/projects/demo/transactions"))
        .bearer_auth("client-token")
        .json(&dependent)
        .send()
        .await?
        .error_for_status()?;
    let mut sorted = nodes.clone();
    sorted.sort();
    let mut counts = Vec::new();
    let mut total = 0;
    for (index, url) in nodes.iter().enumerate() {
        let db = kelp_core::storage::open(&directories[index].join("kelp.sqlite3"))?;
        let transactions: Vec<String> = db
            .prepare("SELECT transaction_id FROM tx_journal WHERE project = 'demo'")?
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        let actual_partition = sorted.iter().position(|node| node == url).unwrap();
        for id in &transactions {
            assert_eq!(partition("demo", "transaction", id, 3), actual_partition);
        }
        let blobs = kelp_core::storage::count(&db, "demo", "blob")?;
        assert!(!transactions.is_empty() && blobs > 0);
        total += transactions.len();
        counts.push((transactions.len(), blobs));
    }
    assert_eq!(total, 40);
    for directory in &directories {
        let db = kelp_core::storage::open(&directory.join("kelp.sqlite3"))?;
        kelp_core::storage::compact(&db, &BTreeSet::new())?;
    }
    // A third gateway has no project database to copy or warm up.
    let fresh = start(
        kelp_remote::cluster_app(nodes.clone(), "client-token".into(), "storage-token".into())?,
        &mut servers,
    )
    .await?;
    cli(
        root.path(),
        &["clone", &format!("{fresh}/demo"), "observer"],
    );
    let observer = root.path().join("observer");
    assert_eq!(fs::read_to_string(observer.join("alice.txt"))?, "Alice");
    assert_eq!(fs::read_to_string(observer.join("bob.txt"))?, "Bob");
    assert_eq!(fs::read(observer.join("api"))?, b"v2");
    assert_eq!(fs::read(observer.join("caller"))?, b"v2");
    for index in 0..36 {
        assert_eq!(
            fs::read(observer.join(format!("batch/{index}/api")))?,
            fs::read(observer.join(format!("batch/{index}/caller")))?
        );
    }
    let workspace = kelp_cli::workspace::Workspace::open(&observer)?;
    let graph = workspace.graph()?;
    assert!(graph.transactions[&a_commit].dependencies().is_empty());
    assert!(graph.transactions[&b_commit].dependencies().is_empty());
    assert_eq!(graph.transactions.len(), 40);
    println!(
        "one project / 3 stores: (journal entries, blobs) = {counts:?}; independent pushes through 2 gateways converged"
    );
    drop(workspace);
    let fourth_directory = root.path().join("node-3");
    let fourth = start(
        kelp_remote::storage_app(&fourth_directory, "storage-token".into())?,
        &mut servers,
    )
    .await?;
    nodes.push(fourth.clone());
    let expanded = start(
        kelp_remote::cluster_app(nodes.clone(), "client-token".into(), "storage-token".into())?,
        &mut servers,
    )
    .await?;
    // Adding capacity preserves old content without rewriting any transaction.
    cli(
        root.path(),
        &["clone", &format!("{expanded}/demo"), "expanded-copy"],
    );
    assert_eq!(
        fs::read(root.path().join("expanded-copy/alice.txt"))?,
        b"Alice"
    );
    nodes.sort();
    let fourth_index = nodes.iter().position(|node| *node == fourth).unwrap();
    let bytes = b"new capacity";
    let blob = object_id("blob", bytes);
    client
        .put(format!("{expanded}/v1/projects/demo/objects/blob/{blob}"))
        .bearer_auth("client-token")
        .body(bytes.to_vec())
        .send()
        .await?
        .error_for_status()?;
    let mut transaction = Transaction {
        provenance: None,
        format: 1,
        nonce: "scale".into(),
        message: "Use added capacity".into(),
        edits: BTreeMap::from([(
            "scale.txt".into(),
            Edit {
                parents: BTreeSet::new(),
                value: Some(FileEntry {
                    blob,
                    size: bytes.len() as u64,
                    executable: false,
                    kind: Default::default(),
                }),
            },
        )]),
    };
    let mut counter = 0;
    while partition("demo", "transaction", &transaction.id()?, 4) != fourth_index {
        counter += 1;
        transaction.nonce = format!("scale-{counter}");
    }
    client
        .post(format!("{expanded}/v1/projects/demo/transactions"))
        .bearer_auth("client-token")
        .json(&transaction)
        .send()
        .await?
        .error_for_status()?;
    let fourth_db = kelp_core::storage::open(&fourth_directory.join("kelp.sqlite3"))?;
    let entries: i64 = fourth_db.query_row(
        "SELECT count(*) FROM tx_journal WHERE project = 'demo'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(entries, 1);
    cli(
        root.path(),
        &[
            "clone",
            &format!("{expanded}/demo"),
            "partial",
            "--paths",
            "batch/0/api",
        ],
    );
    let partial = root.path().join("partial");
    assert!(!partial.join("batch/0/caller").exists());
    assert!(!partial.join("alice.txt").exists());
    fs::write(
        partial.join("batch/0/api"),
        b"partial edit across sharded storage",
    )?;
    cli(&partial, &["commit", "-m", "Edit selected file"]);
    cli(&partial, &["push"]);
    let full = root.path().join("expanded-copy");
    cli(&full, &["pull"]);
    assert_eq!(
        fs::read(full.join("batch/0/api"))?,
        b"partial edit across sharded storage"
    );
    assert_eq!(
        fs::read(full.join("batch/0/caller"))?,
        b"partitioned payload 0"
    );
    println!(
        "expanded the same project to 4 stores; old commits remained readable and the new store accepted a write"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn incomplete_transactions_are_invisible_and_a_missing_shard_is_not_an_empty_view()
-> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let mut servers = Servers(Vec::new());
    let mut nodes = Vec::new();
    for index in 0..3 {
        nodes.push(
            start(
                kelp_remote::storage_app(
                    &root.path().join(index.to_string()),
                    "storage-token".into(),
                )?,
                &mut servers,
            )
            .await?,
        );
    }
    let gateway = start(
        kelp_remote::cluster_app(nodes, "client-token".into(), "storage-token".into())?,
        &mut servers,
    )
    .await?;
    let client = reqwest::Client::new();
    client
        .put(format!("{gateway}/v1/projects/demo"))
        .bearer_auth("client-token")
        .send()
        .await?
        .error_for_status()?;
    let bytes = b"available";
    let blob = object_id("blob", bytes);
    client
        .put(format!("{gateway}/v1/projects/demo/objects/blob/{blob}"))
        .bearer_auth("client-token")
        .body(bytes.to_vec())
        .send()
        .await?
        .error_for_status()?;
    let missing = object_id("blob", b"not uploaded");
    let transaction = Transaction {
        provenance: None,
        format: 1,
        nonce: "incomplete".into(),
        message: "Atomic pair".into(),
        edits: BTreeMap::from([
            (
                "a".into(),
                Edit {
                    parents: BTreeSet::new(),
                    value: Some(FileEntry {
                        blob,
                        size: bytes.len() as u64,
                        executable: false,
                        kind: Default::default(),
                    }),
                },
            ),
            (
                "b".into(),
                Edit {
                    parents: BTreeSet::new(),
                    value: Some(FileEntry {
                        blob: missing,
                        size: 12,
                        executable: false,
                        kind: Default::default(),
                    }),
                },
            ),
        ]),
    };
    let response = client
        .post(format!("{gateway}/v1/projects/demo/transactions"))
        .bearer_auth("client-token")
        .json(&transaction)
        .send()
        .await?;
    assert!(!response.status().is_success());
    let page: SyncPage = client
        .post(format!("{gateway}/v1/projects/demo/sync"))
        .bearer_auth("client-token")
        .json(&SyncRequest::default())
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert!(page.transactions.is_empty());
    let missing_bytes = b"not uploaded";
    let missing_id = object_id("blob", missing_bytes);
    client
        .put(format!(
            "{gateway}/v1/projects/demo/objects/blob/{missing_id}"
        ))
        .bearer_auth("client-token")
        .body(missing_bytes.to_vec())
        .send()
        .await?
        .error_for_status()?;
    for _ in 0..2 {
        client
            .post(format!("{gateway}/v1/projects/demo/transactions"))
            .bearer_auth("client-token")
            .json(&transaction)
            .send()
            .await?
            .error_for_status()?;
    }
    let page: SyncPage = client
        .post(format!("{gateway}/v1/projects/demo/sync"))
        .bearer_auth("client-token")
        .json(&SyncRequest::default())
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(page.transactions.len(), 1);
    servers.0[0].abort();
    let response = client
        .post(format!("{gateway}/v1/projects/demo/sync"))
        .bearer_auth("client-token")
        .json(&SyncRequest::default())
        .send()
        .await?;
    assert_eq!(response.status(), 503);
    Ok(())
}

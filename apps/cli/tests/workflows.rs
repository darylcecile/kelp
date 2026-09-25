use std::{
    fs,
    path::Path,
    process::{Command, Output},
    time::Duration,
};

use kelp_cli::workspace::Workspace;
use kelp_core::storage;
use serde_json::Value;

fn command(directory: &Path, args: &[&str]) -> Output {
    let directory = directory.to_owned();
    let arguments: Vec<_> = args.iter().map(|arg| (*arg).to_owned()).collect();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = Command::new(env!("CARGO_BIN_EXE_kelp"))
            .arg("-C")
            .arg(directory)
            .arg("--json")
            .args(arguments)
            .env("KELP_TOKEN", "test-token")
            .output();
        let _ = sender.send(result);
    });
    receiver
        .recv_timeout(Duration::from_secs(30))
        .unwrap_or_else(|_| panic!("kelp {args:?} did not return within 30 seconds"))
        .expect("start kelp")
}

fn run(directory: &Path, args: &[&str]) -> Value {
    let output = command(directory, args);
    assert!(
        output.status.success(),
        "kelp {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("structured CLI output")
}

#[test]
fn explicit_versions_are_inspectable_and_restored_in_place() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    fs::write(root.join("file.txt"), "first\n")?;
    fs::write(root.join(".gitignore"), "ignored.txt\n")?;
    fs::write(root.join("ignored.txt"), "keep this local")?;
    run(root, &["init", "--project", "demo"]);
    assert_eq!(run(root, &["log"]), serde_json::json!([]));
    assert!(
        run(root, &["diff"])
            .as_array()
            .unwrap()
            .iter()
            .any(|file| file["diff"].as_str().unwrap().contains("+first"))
    );
    let first = run(root, &["commit", "-m", "Initial working version"])["view"]
        .as_str()
        .unwrap()
        .to_owned();
    fs::write(root.join("file.txt"), "second\n")?;
    fs::write(root.join("new.txt"), "included by commit")?;
    let second = run(root, &["commit", "-m", "Improve the example"])["view"]
        .as_str()
        .unwrap()
        .to_owned();
    let history = run(root, &["log"]);
    assert_eq!(history[0]["message"], "Improve the example");
    assert!(
        history[0]["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|file| file["path"] == "new.txt" && file["kind"] == "added")
    );
    assert!(
        run(root, &["show", &second[..12]])
            .as_array()
            .unwrap()
            .iter()
            .any(|file| file["diff"].as_str().unwrap().contains("-first\n+second"))
    );
    fs::write(root.join("draft.txt"), "unfinished work")?;
    let restore = run(root, &["restore", &first[..12]]);
    assert_eq!(fs::read_to_string(root.join("file.txt"))?, "first\n");
    assert!(!root.join("new.txt").exists() && !root.join("draft.txt").exists());
    assert_eq!(
        fs::read_to_string(root.join("ignored.txt"))?,
        "keep this local"
    );
    assert!(root.join(".kelp/workspace.sqlite3").exists());
    run(root, &["gc"]);
    run(root, &["restore", restore["backup"].as_str().unwrap()]);
    assert_eq!(
        fs::read_to_string(root.join("draft.txt"))?,
        "unfinished work"
    );
    assert_eq!(run(root, &["status"])["pending_commits"], 2);
    run(root, &["remote", "set", "http://127.0.0.1:9999/demo"]);
    run(root, &["remote", "remove"]);
    assert!(run(root, &["remote"])["remote"].is_null());
    Ok(())
}

#[test]
fn snapshot_only_workspaces_keep_their_history_and_can_enter_transaction_sync() -> anyhow::Result<()>
{
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    fs::create_dir(root.join(".kelp"))?;
    let db = storage::open(&root.join(".kelp/workspace.sqlite3"))?;
    db.execute_batch("CREATE TABLE workspace(id INTEGER PRIMARY KEY, state TEXT NOT NULL);
        CREATE TABLE checkpoints(id INTEGER PRIMARY KEY AUTOINCREMENT, snapshot TEXT NOT NULL, message TEXT, created_at INTEGER NOT NULL);")?;
    let blob = storage::put(&db, "demo", "blob", b"saved")?;
    let snapshot = kelp_core::Snapshot {
        files: [(
            "file".into(),
            kelp_core::FileEntry {
                blob,
                size: 5,
                executable: false,
            },
        )]
        .into(),
    };
    let saved = storage::put_json(&db, "demo", "snapshot", &snapshot)?;
    let state = serde_json::json!({"version":0,"project":"demo","remote":null,"tracked":["file"],"base_snapshot":saved,"head":null,"change":null,"message":null,"pending":null});
    db.execute("INSERT INTO workspace VALUES (1, ?1)", [state.to_string()])?;
    db.execute(
        "INSERT INTO checkpoints VALUES (1, ?1, 'Previous saved work', 1)",
        [&saved],
    )?;
    drop(db);
    fs::write(root.join("file"), "unfinished")?;
    let mut workspace = Workspace::open(root)?;
    assert!(workspace.graph()?.transactions.is_empty());
    assert_eq!(workspace.checkpoint_snapshot(1)?, snapshot);
    let backup = workspace.restore(1)?;
    assert_eq!(fs::read(root.join("file"))?, b"saved");
    let backup_snapshot = workspace.checkpoint_snapshot(backup)?;
    assert_eq!(
        storage::get(
            &workspace.db,
            "demo",
            "blob",
            &backup_snapshot.files["file"].blob
        )?,
        b"unfinished"
    );
    workspace.commit("Introduce the recovered files")?;
    assert_eq!(workspace.graph()?.view()?.snapshot()?, snapshot);
    assert_eq!(workspace.state.outbox.len(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn independent_commits_push_without_pulling_or_rewriting_each_other() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/demo", listener.local_addr()?);
    let app = kelp_remote::app(&root.join("server"), "test-token".into())?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    run(root, &["init", "alice"]);
    let alice = root.join("alice");
    let bob = root.join("bob");
    fs::write(alice.join("left"), "base-left")?;
    fs::write(alice.join("right"), "base-right")?;
    run(&alice, &["commit", "-m", "Initial pair"]);
    run(&alice, &["push", &url]);
    run(root, &["clone", &url, "bob"]);
    fs::write(alice.join("left"), "Alice's edit")?;
    fs::write(bob.join("right"), "Bob's edit")?;
    let a = run(&alice, &["commit", "-m", "Update left"]);
    let b = run(&bob, &["commit", "-m", "Update right"]);
    assert_eq!(run(&alice, &["push"])["pushed"], 1);
    assert_eq!(run(&bob, &["push"])["pushed"], 1);
    // Neither push changed its original commit ID or needed the other commit.
    assert_eq!(run(&alice, &["log"])[0]["transaction"], a["transaction"]);
    assert_eq!(run(&bob, &["log"])[0]["transaction"], b["transaction"]);
    fs::write(bob.join("unfinished"), "not committed")?;
    assert_eq!(run(&bob, &["push"])["pushed"], 0);
    run(&bob, &["pull"]);
    assert_eq!(fs::read_to_string(bob.join("left"))?, "Alice's edit");
    assert_eq!(fs::read_to_string(bob.join("right"))?, "Bob's edit");
    assert_eq!(fs::read_to_string(bob.join("unfinished"))?, "not committed");
    assert_eq!(
        run(&bob, &["status"])["changed"],
        serde_json::json!(["unfinished"])
    );
    run(root, &["clone", &url, "observer"]);
    let observer = root.join("observer");
    assert!(!observer.join("unfinished").exists());
    assert_eq!(fs::read_to_string(observer.join("left"))?, "Alice's edit");
    assert_eq!(
        run(&observer, &["log", "--commits"])
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert_eq!(
        run(&observer, &["status"])["view"],
        run(&bob, &["status"])["view"]
    );
    server.abort();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_edits_survive_and_resolution_consumes_both_parents() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/demo", listener.local_addr()?);
    let app = kelp_remote::app(&root.join("server"), "test-token".into())?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    run(root, &["init", "alice"]);
    let alice = root.join("alice");
    let bob = root.join("bob");
    fs::write(alice.join("file"), "base")?;
    run(&alice, &["commit", "-m", "Base"]);
    run(&alice, &["push", &url]);
    run(root, &["clone", &url, "bob"]);
    fs::write(alice.join("file"), "Alice")?;
    let a = run(&alice, &["commit", "-m", "Alice's edit"])["transaction"]
        .as_str()
        .unwrap()
        .to_owned();
    fs::write(bob.join("file"), "Bob")?;
    let b = run(&bob, &["commit", "-m", "Bob's edit"])["transaction"]
        .as_str()
        .unwrap()
        .to_owned();
    run(&alice, &["push"]);
    run(&bob, &["push"]);
    run(&alice, &["gc"]);
    run(&bob, &["gc"]);
    let conflict = command(&bob, &["pull"]);
    assert!(!conflict.status.success());
    assert!(
        String::from_utf8_lossy(&conflict.stderr).contains("both versions")
            || String::from_utf8_lossy(&conflict.stderr).contains("Both versions")
    );
    assert_eq!(fs::read_to_string(bob.join("file"))?, "Bob");
    run(&bob, &["pull", "--keep-remote"]);
    assert_eq!(fs::read_to_string(bob.join("file"))?, "Alice");
    assert_eq!(
        run(&bob, &["status"])["conflicts"],
        serde_json::json!(["file"])
    );
    let resolved = run(&bob, &["commit", "-m", "Resolve both edits"])["transaction"]
        .as_str()
        .unwrap()
        .to_owned();
    {
        let workspace = Workspace::open(&bob)?;
        let graph = workspace.graph()?;
        assert_eq!(
            graph.transactions[&resolved].edits["file"].parents,
            [a, b.clone()].into()
        );
        assert!(graph.view()?.conflicts().is_empty());
    }
    run(&bob, &["push"]);
    run(&alice, &["pull"]);
    assert_eq!(fs::read_to_string(alice.join("file"))?, "Alice");
    assert!(
        run(&alice, &["show", &b])
            .as_array()
            .unwrap()
            .iter()
            .any(|file| file["diff"].as_str().unwrap().contains("+Bob"))
    );
    server.abort();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_push_retries_only_the_original_committed_bytes() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    drop(listener);
    let url = format!("http://{address}/demo");
    run(root, &["init", "--project", "demo"]);
    fs::write(root.join("file"), "committed")?;
    let commit = run(root, &["commit", "-m", "Retry this"])["transaction"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(!command(root, &["push", &url]).status.success());
    assert!(Workspace::open(root)?.state.outbox.contains(&commit));
    fs::write(root.join("file"), "unfinished later edit")?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    let data = tempfile::tempdir()?;
    let app = kelp_remote::app(data.path(), "test-token".into())?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    assert_eq!(run(root, &["push"])["pushed"], 1);
    assert_eq!(run(root, &["push"])["pushed"], 0);
    assert_eq!(
        run(root, &["status"])["changed"],
        serde_json::json!(["file"])
    );
    let copy = tempfile::tempdir()?;
    run(copy.path(), &["clone", &url]);
    assert_eq!(
        fs::read_to_string(copy.path().join("demo/file"))?,
        "committed"
    );
    server.abort();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clone_rejects_an_older_protocol_instead_of_returning_an_empty_project()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/demo", listener.local_addr()?);
    let app = axum::Router::new().route("/v1/projects/demo", axum::routing::get(|| async { axum::Json(serde_json::json!({"project":"demo", "protocol":"kelp/0", "layout":"local", "storage_nodes":1})) }));
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let output = command(directory.path(), &["clone", &url]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unsupported remote protocol"));
    assert!(!directory.path().join("demo").exists());
    server.abort();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_small_files_use_bounded_batches_and_incremental_pull_keeps_old_history_cached()
-> anyhow::Result<()> {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let root = tempfile::tempdir()?;
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let app = kelp_remote::app(&root.path().join("server"), "test-token".into())?.layer(
        axum::middleware::from_fn(
            move |request: axum::extract::Request, next: axum::middleware::Next| {
                let calls = observed.clone();
                async move {
                    calls.fetch_add(1, Ordering::Relaxed);
                    next.run(request).await
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/demo", listener.local_addr()?);
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    run(root.path(), &["init", "source"]);
    let source = root.path().join("source");
    for index in 0..300 {
        fs::write(
            source.join(format!("file-{index}")),
            format!("unique contents {index}\n").repeat(100),
        )?;
    }
    run(&source, &["commit", "-m", "Many files"]);
    calls.store(0, Ordering::Relaxed);
    run(&source, &["push", &url]);
    assert!(
        calls.load(Ordering::Relaxed) < 20,
        "push issued one request per file"
    );
    calls.store(0, Ordering::Relaxed);
    run(root.path(), &["clone", &url, "copy"]);
    assert!(
        calls.load(Ordering::Relaxed) < 20,
        "clone issued one request per file"
    );
    fs::write(source.join("file-0"), "changed")?;
    run(&source, &["commit", "-m", "One file"]);
    run(&source, &["push"]);
    calls.store(0, Ordering::Relaxed);
    run(&root.path().join("copy"), &["pull"]);
    assert!(
        calls.load(Ordering::Relaxed) < 12,
        "pull refetched historical objects"
    );
    assert_eq!(fs::read(root.path().join("copy/file-0"))?, b"changed");
    server.abort();
    Ok(())
}

#[cfg(not(unix))]
#[test]
fn commits_preserve_imported_executable_metadata() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("script.sh"), "echo hello")?;
    let mut workspace = Workspace::init(directory.path(), "demo", None)?;
    let first = workspace.commit("Initial script")?;
    let mut imported = workspace.checkpoint_snapshot(first.id)?;
    imported.files.get_mut("script.sh").unwrap().executable = true;
    workspace.state.base_snapshot =
        storage::put_json(&workspace.db, "demo", "snapshot", &imported)?;
    workspace.save_state()?;
    let version = workspace.commit("Imported executable")?;
    assert_eq!(
        workspace.checkpoint_snapshot(version.id)?.id()?,
        imported.id()?
    );
    Ok(())
}

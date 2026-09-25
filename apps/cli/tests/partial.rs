use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

use kelp_cli::workspace::Workspace;
use kelp_core::{storage, transactions::SavedView};
use serde_json::Value;

struct Server(tokio::task::JoinHandle<std::io::Result<()>>);
impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn remote(root: &Path) -> anyhow::Result<(Server, String)> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/demo", listener.local_addr()?);
    let app = kelp_remote::app(&root.join("remote"), "test-token".into())?;
    Ok((
        Server(tokio::spawn(
            async move { axum::serve(listener, app).await },
        )),
        url,
    ))
}

fn command(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kelp"))
        .arg("-C")
        .arg(root)
        .arg("--json")
        .args(args)
        .env("KELP_TOKEN", "test-token")
        .output()
        .expect("start kelp")
}

fn run(root: &Path, args: &[&str]) -> Value {
    let output = command(root, args);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn write(root: &Path, path: &str, bytes: &[u8]) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

fn assert_not_cached(root: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let workspace = Workspace::open(root)?;
    assert!(!storage::contains(
        &workspace.db,
        "demo",
        "blob",
        &kelp_core::object_id("blob", bytes)
    )?);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn partial_clone_preserves_atomic_metadata_and_pushes_only_selected_edits()
-> anyhow::Result<()> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path();
    let (_server, url) = remote(root).await?;
    run(root, &["init", "full", "--project", "demo"]);
    let full = root.join("full");
    write(&full, "services/api/main.txt", b"API v1");
    write(&full, "services/api/delete.txt", b"delete me");
    write(&full, "services/api-other/file.txt", b"boundary sibling");
    let asset = vec![211; 128 * 1024];
    write(&full, "assets/model.bin", &asset);
    let initial = run(&full, &["commit", "-m", "Atomic API and asset"]);
    run(&full, &["push", &url]);
    run(
        root,
        &[
            "clone",
            &url,
            "partial",
            "--paths",
            "./services/api/",
            "--paths",
            "services/api/main.txt",
        ],
    );
    let partial = root.join("partial");
    assert_eq!(
        run(&partial, &["status"])["paths"],
        serde_json::json!(["services/api"])
    );
    assert!(!partial.join("assets").exists());
    assert!(!partial.join("services/api-other").exists());
    assert_not_cached(&partial, &asset)?;
    assert_not_cached(&partial, b"boundary sibling")?;
    let saved = run(&partial, &["log"])[0]["view"]
        .as_str()
        .unwrap()
        .to_owned();
    {
        let workspace = Workspace::open(&partial)?;
        let transaction = workspace.transaction(initial["transaction"].as_str().unwrap())?;
        assert_eq!(
            transaction.edits.len(),
            4,
            "the atomic batch must not be projected into a different transaction"
        );
        let view: SavedView = storage::get_json(&workspace.db, "demo", "saved-view", &saved)?;
        assert_eq!(view.format, 2);
        assert_eq!(view.paths, ["services/api"]);
    }
    let inspected = run(
        &partial,
        &["show", initial["transaction"].as_str().unwrap()],
    );
    assert!(
        inspected
            .as_array()
            .unwrap()
            .iter()
            .any(|file| file["path"] == "assets/model.bin" && file["kind"] == "not downloaded")
    );

    // Unseen independent outside changes must not force a pull before pushing.
    write(&full, "assets/new.bin", b"new outside bytes");
    run(&full, &["commit", "-m", "Outside update"]);
    run(&full, &["push"]);
    write(&partial, "services/api/main.txt", b"API v2");
    fs::remove_file(partial.join("services/api/delete.txt"))?;
    write(&partial, "services/api/new.txt", b"new endpoint");
    write(&partial, "assets/local.txt", b"leave this alone");
    let edited = run(&partial, &["commit", "-m", "API edits only"]);
    {
        let workspace = Workspace::open(&partial)?;
        let transaction = workspace.transaction(edited["transaction"].as_str().unwrap())?;
        assert_eq!(transaction.edits.len(), 3);
        assert!(
            transaction
                .edits
                .keys()
                .all(|path| path.starts_with("services/api/"))
        );
        assert!(transaction.edits["services/api/delete.txt"].value.is_none());
    }
    assert_eq!(run(&partial, &["push"])["pushed"], 1);
    run(&partial, &["pull"]);
    assert_not_cached(&partial, b"new outside bytes")?;
    assert_eq!(
        fs::read(partial.join("assets/local.txt"))?,
        b"leave this alone"
    );
    run(&full, &["pull"]);
    assert_eq!(fs::read(full.join("assets/model.bin"))?, asset);
    assert_eq!(fs::read(full.join("assets/new.bin"))?, b"new outside bytes");
    assert_eq!(fs::read(full.join("services/api/main.txt"))?, b"API v2");
    assert!(!full.join("services/api/delete.txt").exists());
    assert!(!full.join("assets/local.txt").exists());

    run(&partial, &["gc"]);
    run(&partial, &["restore", &saved]);
    assert_eq!(fs::read(partial.join("services/api/main.txt"))?, b"API v1");
    assert_eq!(
        fs::read(partial.join("assets/local.txt"))?,
        b"leave this alone"
    );
    run(&partial, &["commit", "-m", "Restore the selected API"]);
    run(&partial, &["push"]);
    run(&full, &["pull"]);
    assert_eq!(fs::read(full.join("services/api/main.txt"))?, b"API v1");
    assert_eq!(fs::read(full.join("assets/new.bin"))?, b"new outside bytes");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn selection_supports_multiple_paths_empty_directories_and_rejects_invalid_paths()
-> anyhow::Result<()> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path();
    let (_server, url) = remote(root).await?;
    run(root, &["init", "full"]);
    let full = root.join("full");
    write(&full, "src/main.txt", b"source");
    write(&full, "docs/guide.md", b"guide");
    write(&full, "docs/private.md", b"other docs");
    run(&full, &["commit", "-m", "Files"]);
    run(&full, &["push", &url]);
    run(
        root,
        &[
            "clone",
            &url,
            "multiple",
            "--paths",
            "src,docs/guide.md",
            "--paths",
            "src/main.txt",
        ],
    );
    let multiple = root.join("multiple");
    assert_eq!(
        run(&multiple, &["status"])["paths"],
        serde_json::json!(["docs/guide.md", "src"])
    );
    assert_eq!(fs::read(multiple.join("docs/guide.md"))?, b"guide");
    assert!(!multiple.join("docs/private.md").exists());
    run(root, &["clone", &url, "new-area", "--paths", "new-service"]);
    let new_area = root.join("new-area");
    write(&new_area, "new-service/main.txt", b"new service");
    run(&new_area, &["commit", "-m", "Add a new selected directory"]);
    run(&new_area, &["push"]);
    run(&full, &["pull"]);
    assert_eq!(fs::read(full.join("new-service/main.txt"))?, b"new service");
    assert_eq!(fs::read(full.join("src/main.txt"))?, b"source");
    for path in [
        "../outside",
        "/absolute",
        "src/../docs",
        "src/*",
        ".kelp",
        ",",
    ] {
        let output = command(root, &["clone", &url, "invalid", "--paths", path]);
        assert!(!output.status.success(), "accepted {path}");
        assert!(!root.join("invalid").exists());
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn outside_conflicts_do_not_block_selected_work_and_inside_conflicts_remain_resolvable()
-> anyhow::Result<()> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path();
    let (_server, url) = remote(root).await?;
    run(root, &["init", "alice"]);
    let alice = root.join("alice");
    write(&alice, "api/file", b"base API");
    write(&alice, "docs/file", b"base docs");
    run(&alice, &["commit", "-m", "Base"]);
    run(&alice, &["push", &url]);
    run(root, &["clone", &url, "bob"]);
    let bob = root.join("bob");
    write(&alice, "docs/file", b"Alice docs");
    write(&bob, "docs/file", b"Bob docs");
    run(&alice, &["commit", "-m", "Alice docs"]);
    run(&bob, &["commit", "-m", "Bob docs"]);
    run(&alice, &["push"]);
    run(&bob, &["push"]);
    run(root, &["clone", &url, "api-only", "--paths", "api"]);
    let partial = root.join("api-only");
    assert_eq!(
        run(&partial, &["status"])["conflicts"],
        serde_json::json!([])
    );
    assert_not_cached(&partial, b"Alice docs")?;
    assert_not_cached(&partial, b"Bob docs")?;
    write(&alice, "api/file", b"Alice API");
    run(&alice, &["commit", "-m", "Alice API"]);
    run(&alice, &["push"]);
    write(&partial, "api/file", b"Partial API");
    run(&partial, &["commit", "-m", "Partial API"]);
    run(&partial, &["push"]);
    assert!(!command(&partial, &["pull"]).status.success());
    assert_eq!(fs::read(partial.join("api/file"))?, b"Partial API");
    run(&partial, &["pull", "--keep-remote"]);
    let resolution = run(&partial, &["commit", "-m", "Resolve API only"]);
    {
        let workspace = Workspace::open(&partial)?;
        let transaction = workspace.transaction(resolution["transaction"].as_str().unwrap())?;
        assert_eq!(transaction.edits.keys().collect::<Vec<_>>(), ["api/file"]);
        assert_eq!(transaction.edits["api/file"].parents.len(), 2);
        assert_eq!(
            workspace.graph()?.view()?.conflicts(),
            ["docs/file".into()].into()
        );
    }
    run(&partial, &["push"]);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn partial_saved_views_cannot_restore_a_full_checkout_or_copy_uncached_history()
-> anyhow::Result<()> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path();
    let (_server, url) = remote(root).await?;
    run(root, &["init", "full"]);
    let full = root.join("full");
    write(&full, "api/file", b"API");
    write(&full, "assets/file", b"asset only on full remote");
    run(&full, &["commit", "-m", "Cross-directory batch"]);
    run(&full, &["push", &url]);
    run(root, &["clone", &url, "partial", "--paths", "api"]);
    let partial = root.join("partial");
    let view_hash = run(&partial, &["log"])[0]["view"]
        .as_str()
        .unwrap()
        .to_owned();
    {
        let partial = Workspace::open(&partial)?;
        let full = Workspace::open(&full)?;
        let view: SavedView = storage::get_json(&partial.db, "demo", "saved-view", &view_hash)?;
        storage::put_json(&full.db, "demo", "saved-view", &view)?;
    }
    let error = command(&full, &["restore", &view_hash]);
    assert!(!error.status.success());
    assert!(String::from_utf8_lossy(&error.stderr).contains("different path selection"));
    assert_eq!(
        fs::read(full.join("assets/file"))?,
        b"asset only on full remote"
    );
    let (_another, other_url) = remote(&root.join("other")).await?;
    let error = command(&partial, &["push", &other_url]);
    assert!(!error.status.success());
    assert!(String::from_utf8_lossy(&error.stderr).contains("partial checkout"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn structural_conflicts_across_the_selection_boundary_are_not_hidden() -> anyhow::Result<()> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path();
    let (_server, url) = remote(root).await?;
    run(root, &["init", "alice"]);
    let alice = root.join("alice");
    write(&alice, "seed", b"base");
    run(&alice, &["commit", "-m", "Base"]);
    run(&alice, &["push", &url]);
    run(root, &["clone", &url, "bob"]);
    let bob = root.join("bob");
    write(&alice, "tree", b"a file");
    write(&bob, "tree/leaf", b"a child file");
    run(&alice, &["commit", "-m", "Add a file"]);
    run(&bob, &["commit", "-m", "Add a directory"]);
    run(&alice, &["push"]);
    run(&bob, &["push"]);
    let result = command(root, &["clone", &url, "partial", "--paths", "tree/leaf"]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("collide"));
    assert!(!root.join("partial").exists());
    Ok(())
}

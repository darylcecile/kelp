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
    for path in ["../outside", "/absolute", "src/../docs", ".kelp", ","] {
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
        assert!(workspace.graph()?.view()?.conflicts().is_empty());
    }
    run(&partial, &["push"]);
    assert!(
        !command(root, &["clone", &url, "still-conflicted"])
            .status
            .success(),
        "outside conflicts must remain on the remote"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn partial_sync_skips_independent_metadata_and_expansion_fetches_it() -> anyhow::Result<()> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path();
    let (_server, url) = remote(root).await?;
    run(root, &["init", "full"]);
    let full = root.join("full");
    write(&full, "api/file", b"API");
    run(&full, &["commit", "-m", "API"]);
    let mut outside = Vec::new();
    for i in 0..8 {
        write(&full, "docs/file", format!("docs {i}").as_bytes());
        outside.push(
            run(&full, &["commit", "-m", "Docs"])["transaction"]
                .as_str()
                .unwrap()
                .to_owned(),
        );
    }
    run(&full, &["push", &url]);
    run(root, &["clone", &url, "partial", "--paths", "api"]);
    let partial = root.join("partial");
    {
        let workspace = Workspace::open(&partial)?;
        assert_eq!(workspace.state.transactions.len(), 1);
        for id in &outside {
            assert!(!storage::contains(
                &workspace.db,
                "demo",
                "transaction",
                id
            )?);
        }
    }
    run(&partial, &["pull", "--paths", "docs"]);
    assert_eq!(fs::read(partial.join("docs/file"))?, b"docs 7");
    assert_eq!(Workspace::open(&partial)?.state.transactions.len(), 9);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn text_merge_preserves_nonoverlapping_commits_and_uncommitted_edits() -> anyhow::Result<()> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path();
    let (_server, url) = remote(root).await?;
    run(root, &["init", "alice"]);
    let alice = root.join("alice");
    write(&alice, "file", b"one\ntwo\nthree\nfour\nfive\n");
    run(&alice, &["commit", "-m", "Base"]);
    run(&alice, &["push", &url]);
    run(root, &["clone", &url, "bob"]);
    let bob = root.join("bob");
    write(&alice, "file", b"ONE\ntwo\nthree\nfour\nfive\n");
    write(&bob, "file", b"one\ntwo\nthree\nfour\nFIVE\n");
    run(&alice, &["commit", "-m", "First line"]);
    run(&bob, &["commit", "-m", "Last line"]);
    run(&alice, &["push"]);
    run(&bob, &["push"]);
    // A clean clone can materialize a clean merge without inventing a commit.
    run(root, &["clone", &url, "merged"]);
    assert_eq!(
        fs::read(root.join("merged/file"))?,
        b"ONE\ntwo\nthree\nfour\nFIVE\n"
    );
    write(&bob, "file", b"one\ntwo\nTHREE\nfour\nFIVE\n");
    run(&bob, &["pull"]);
    assert_eq!(
        fs::read(bob.join("file"))?,
        b"ONE\ntwo\nTHREE\nfour\nFIVE\n"
    );
    let resolution = run(&bob, &["commit", "-m", "Merged and tested"]);
    assert_eq!(
        Workspace::open(&bob)?
            .transaction(resolution["transaction"].as_str().unwrap())?
            .edits["file"]
            .parents
            .len(),
        2
    );
    run(&bob, &["push"]);
    run(&alice, &["pull"]);
    assert_eq!(
        fs::read(alice.join("file"))?,
        b"ONE\ntwo\nTHREE\nfour\nFIVE\n"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_saved_views_require_expansion_and_partial_copies_need_uncached_history()
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
    let view_hash = run(&full, &["log"])[0]["view"].as_str().unwrap().to_owned();
    {
        let partial = Workspace::open(&partial)?;
        let full = Workspace::open(&full)?;
        let view: SavedView = storage::get_json(&full.db, "demo", "saved-view", &view_hash)?;
        storage::put_json(&partial.db, "demo", "saved-view", &view)?;
    }
    let error = command(&partial, &["restore", &view_hash]);
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
async fn expanding_a_checkout_preserves_drafts_and_old_scoped_recovery_views() -> anyhow::Result<()>
{
    let temporary = tempfile::tempdir()?;
    let root = temporary.path();
    let (_server, url) = remote(root).await?;
    run(root, &["init", "full"]);
    let full = root.join("full");
    write(&full, "api/file", b"API");
    write(&full, "docs/file", b"docs");
    write(&full, "assets/file", b"asset");
    run(&full, &["commit", "-m", "Initial"]);
    run(&full, &["push", &url]);
    run(root, &["clone", &url, "partial", "--paths", "api"]);
    let partial = root.join("partial");
    let old_view = run(&partial, &["log"])[0]["view"]
        .as_str()
        .unwrap()
        .to_owned();
    write(&partial, "api/file", b"draft API");
    write(&partial, "docs/file", b"local docs");
    assert!(
        !command(&partial, &["pull", "--paths", "docs"])
            .status
            .success()
    );
    assert_eq!(
        run(&partial, &["status"])["paths"],
        serde_json::json!(["api"])
    );
    assert_eq!(fs::read(partial.join("docs/file"))?, b"local docs");
    run(&partial, &["pull", "--paths", "docs", "--keep-local"]);
    assert_eq!(
        run(&partial, &["status"])["paths"],
        serde_json::json!(["api", "docs"])
    );
    assert_eq!(fs::read(partial.join("api/file"))?, b"draft API");
    run(&partial, &["pull"]);
    assert!(!partial.join("assets/file").exists());
    run(&partial, &["pull", "--paths", "."]);
    assert_eq!(fs::read(partial.join("assets/file"))?, b"asset");
    run(&partial, &["restore", &old_view]);
    assert_eq!(fs::read(partial.join("api/file"))?, b"API");
    assert_eq!(fs::read(partial.join("docs/file"))?, b"local docs");
    assert_eq!(fs::read(partial.join("assets/file"))?, b"asset");
    run(&partial, &["commit", "-m", "Expanded edit"]);
    run(&partial, &["push"]);
    run(&full, &["pull"]);
    assert_eq!(fs::read(full.join("docs/file"))?, b"local docs");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn large_files_stream_through_partial_expansion_compaction_restore_and_push()
-> anyhow::Result<()> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path();
    let (_server, url) = remote(root).await?;
    run(root, &["init", "full"]);
    let full = root.join("full");
    let mut bytes: Vec<u8> = (0..21 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    write(&full, "assets/large.bin", &bytes);
    write(&full, "api/file", b"API");
    let first = run(&full, &["commit", "-m", "Large file"]);
    run(&full, &["push", &url]);
    run(root, &["clone", &url, "partial", "--paths", "api"]);
    let partial = root.join("partial");
    assert!(!partial.join("assets").exists());
    run(&partial, &["pull", "--paths", "assets"]);
    assert_eq!(fs::read(partial.join("assets/large.bin"))?, bytes);
    bytes[5 * 1024 * 1024] = 255;
    write(&partial, "assets/large.bin", &bytes);
    run(&partial, &["commit", "-m", "One chunk changed"]);
    run(&partial, &["push"]);
    run(&full, &["pull"]);
    assert_eq!(fs::read(full.join("assets/large.bin"))?, bytes);
    run(&full, &["gc"]);
    run(&full, &["restore", first["view"].as_str().unwrap()]);
    bytes[5 * 1024 * 1024] = ((5 * 1024 * 1024) % 251) as u8;
    assert_eq!(fs::read(full.join("assets/large.bin"))?, bytes);
    run(&full, &["commit", "-m", "Restore large file"]);
    run(&full, &["push"]);
    run(&partial, &["pull"]);
    assert_eq!(fs::read(partial.join("assets/large.bin"))?, bytes);
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn symlinks_round_trip_without_following_targets() -> anyhow::Result<()> {
    use std::os::unix::fs::symlink;
    let temporary = tempfile::tempdir()?;
    let root = temporary.path();
    let (_server, url) = remote(root).await?;
    run(root, &["init", "full"]);
    let full = root.join("full");
    write(root, "outside", b"outside stays untouched");
    symlink("../outside", full.join("link"))?;
    symlink("missing", full.join("dangling"))?;
    let original = run(&full, &["commit", "-m", "Links"]);
    run(&full, &["push", &url]);
    run(root, &["clone", &url, "copy"]);
    let copy = root.join("copy");
    assert_eq!(fs::read_link(copy.join("link"))?, Path::new("../outside"));
    assert_eq!(fs::read_link(copy.join("dangling"))?, Path::new("missing"));
    fs::remove_file(copy.join("link"))?;
    write(&copy, "link", b"regular file now");
    run(&copy, &["commit", "-m", "Replace link"]);
    run(&copy, &["push"]);
    run(&full, &["pull"]);
    assert_eq!(fs::read(full.join("link"))?, b"regular file now");
    run(&full, &["gc"]);
    run(&full, &["restore", original["view"].as_str().unwrap()]);
    assert_eq!(fs::read_link(full.join("link"))?, Path::new("../outside"));
    assert_eq!(fs::read(root.join("outside"))?, b"outside stays untouched");
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_names_round_trip_without_utf8_or_portability_restrictions() -> anyhow::Result<()> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path();
    let (_server, url) = remote(root).await?;
    run(root, &["init", "full"]);
    let full = root.join("full");
    fs::create_dir(full.join("data"))?;
    let names: Vec<std::ffi::OsString> = vec![
        "colon:star*".into(),
        "back\\slash".into(),
        "line\nbreak".into(),
        "trailing. ".into(),
    ];
    #[cfg(target_os = "linux")]
    let names = {
        use std::os::unix::ffi::OsStringExt;
        let mut names = names;
        names.push(std::ffi::OsString::from_vec(b"raw-\xff".to_vec()));
        names
    };
    for name in &names {
        fs::write(full.join("data").join(name), b"native file")?;
    }
    let saved = run(&full, &["commit", "-m", "Native file names"]);
    run(&full, &["push", &url]);
    run(root, &["clone", &url, "partial", "--paths", "data"]);
    for name in &names {
        assert_eq!(
            fs::read(root.join("partial/data").join(name))?,
            b"native file"
        );
    }
    for name in &names {
        fs::remove_file(full.join("data").join(name))?;
    }
    run(&full, &["commit", "-m", "Delete files"]);
    run(&full, &["restore", saved["view"].as_str().unwrap()]);
    for name in &names {
        assert_eq!(fs::read(full.join("data").join(name))?, b"native file");
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn git_import_preserves_merge_history_tags_authors_and_committed_bytes() -> anyhow::Result<()>
{
    let temporary = tempfile::tempdir()?;
    let root = temporary.path();
    let (_server, url) = remote(root).await?;
    let source = root.join("source");
    fs::create_dir(&source)?;
    let git = |args: &[&str]| {
        let result = Command::new("git")
            .arg("-C")
            .arg(&source)
            .args([
                "-c",
                "user.name=Import Test",
                "-c",
                "user.email=import@example.test",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    };
    git(&["init", "-b", "main"]);
    write(&source, "file", b"base");
    git(&["add", "."]);
    git(&["commit", "-m", "Base"]);
    git(&["tag", "v1.0"]);
    git(&["checkout", "-b", "feature"]);
    write(&source, "feature", b"side branch");
    git(&["add", "."]);
    git(&["commit", "-m", "Feature"]);
    git(&["checkout", "main"]);
    write(&source, "file", b"main change");
    git(&["add", "."]);
    git(&["commit", "-m", "Main"]);
    git(&["merge", "--no-ff", "feature", "-m", "Merge"]);
    write(&source, "uncommitted", b"not imported");
    let imported = run(root, &["import", "source", "imported", "--project", "demo"]);
    assert_eq!(imported["commits"], 4);
    let checkout = root.join("imported");
    assert_eq!(fs::read(checkout.join("file"))?, b"main change");
    assert_eq!(fs::read(checkout.join("feature"))?, b"side branch");
    assert!(!checkout.join("uncommitted").exists());
    assert_eq!(fs::read(source.join("uncommitted"))?, b"not imported");
    assert!(
        run(&checkout, &["log", "--commits"])[0]["message"]
            .as_str()
            .unwrap()
            .contains("Author: Import Test")
    );
    run(&checkout, &["push", &url]);
    run(root, &["clone", &url, "copy"]);
    let copy = root.join("copy");
    {
        let workspace = Workspace::open(&copy)?;
        for transaction in workspace.graph()?.transactions.values() {
            let original = Command::new("git")
                .arg("-C")
                .arg(&source)
                .args(["cat-file", "commit", &transaction.nonce])
                .output()?;
            assert!(original.status.success());
            let provenance = transaction
                .provenance
                .as_ref()
                .expect("imported commit provenance");
            assert_eq!(
                kelp_core::content::read(&workspace.db, "demo", provenance)?,
                original.stdout
            );
        }
    }
    run(&copy, &["restore", "v1.0"]);
    assert_eq!(fs::read(copy.join("file"))?, b"base");
    assert!(!copy.join("feature").exists());
    let imported = run(
        root,
        &[
            "import",
            "source",
            "feature-copy",
            "--ref",
            "feature",
            "--project",
            "feature",
        ],
    );
    assert_eq!(imported["commits"], 2);
    assert_eq!(fs::read(root.join("feature-copy/file"))?, b"base");
    Ok(())
}

#[test]
fn release_tags_reject_drafts_and_rebinding_an_existing_name() -> anyhow::Result<()> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path();
    run(root, &["init"]);
    write(root, "file", b"committed");
    run(root, &["commit", "-m", "Release"]);
    run(root, &["tag", "v1.0"]);
    write(root, "file", b"draft");
    {
        let workspace = Workspace::open(root)?;
        let draft = workspace.capture(Some("Explicit draft backup"))?;
        assert!(workspace.tag("draft".into(), Some(&draft.view)).is_err());
    }
    run(root, &["commit", "-m", "Next release"]);
    assert!(!command(root, &["tag", "v1.0"]).status.success());
    run(root, &["tag", "v2.0"]);
    assert_eq!(run(root, &["tag"]).as_array().unwrap().len(), 2);
    Ok(())
}

#[test]
fn saved_views_restore_file_directory_transitions_without_removing_ignored_files()
-> anyhow::Result<()> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path();
    run(root, &["init"]);
    write(root, "tree", b"file");
    let file = run(root, &["commit", "-m", "A file"]);
    fs::remove_file(root.join("tree"))?;
    write(root, "tree/child", b"child");
    let directory = run(root, &["commit", "-m", "A directory"]);
    run(root, &["restore", file["view"].as_str().unwrap()]);
    assert_eq!(fs::read(root.join("tree"))?, b"file");
    run(root, &["restore", directory["view"].as_str().unwrap()]);
    assert_eq!(fs::read(root.join("tree/child"))?, b"child");
    write(root, ".kelpignore", "tree/ignored\n".as_bytes());
    write(root, "tree/ignored", b"keep me");
    assert!(
        !command(root, &["restore", file["view"].as_str().unwrap()])
            .status
            .success()
    );
    assert_eq!(fs::read(root.join("tree/ignored"))?, b"keep me");
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

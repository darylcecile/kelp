use std::{
    fs,
    path::Path,
    process::{Command, Output},
    time::{Duration, Instant},
};

use kelp_cli::workspace::Workspace;
use kelp_core::{PublicationReceipt, Revision, storage};
use serde_json::Value;

fn command(directory: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kelp"))
        .arg("--directory")
        .arg(directory)
        .arg("--json")
        .args(args)
        .env("KELP_TOKEN", "test-token")
        .output()
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
fn local_only_checkpoint_recovery_and_later_remote_configuration() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    fs::write(root.join("file.txt"), "first")?;
    fs::write(root.join(".gitignore"), "ignored.txt\n")?;
    fs::write(root.join("ignored.txt"), "not versioned")?;
    run(root, &["init", "--project", "demo", "--no-watch"]);
    let status = run(root, &["status"]);
    assert!(status["remote"].is_null());
    let checkpoint = run(root, &["checkpoint", "-m", "Before editing"])["id"]
        .as_i64()
        .unwrap();
    fs::write(root.join("file.txt"), "second")?;
    fs::write(root.join("new.txt"), "new")?;
    assert_eq!(
        run(root, &["status"])["untracked"],
        serde_json::json!(["new.txt"])
    );
    run(root, &["track", "new.txt"]);
    run(
        root,
        &["restore", &checkpoint.to_string(), "--to", "recovered"],
    );
    assert_eq!(
        fs::read_to_string(root.join("recovered/file.txt"))?,
        "first"
    );
    assert!(!root.join("recovered/ignored.txt").exists());
    assert_eq!(fs::read_to_string(root.join("file.txt"))?, "second");
    assert!(
        !command(
            root,
            &["restore", &checkpoint.to_string(), "--to", "recovered"]
        )
        .status
        .success()
    );
    let output = Command::new(env!("CARGO_BIN_EXE_kelp"))
        .arg("-C")
        .arg(root)
        .args(["publish", "-m", "Local work"])
        .env_remove("KELP_TOKEN")
        .output()?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("local-only"));
    run(root, &["remote", "set", "http://127.0.0.1:9999"]);
    assert_eq!(run(root, &["status"])["remote"], "http://127.0.0.1:9999");
    run(root, &["remote", "remove"]);
    assert!(run(root, &["status"])["remote"].is_null());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn publishes_updates_and_opens_exact_files_from_the_remote() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    let data = root.join("remote-data");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let app = kelp_remote::app(&data, "test-token".into())?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    fs::create_dir(root.join("source"))?;
    fs::write(root.join("source/file.txt"), "first")?;
    fs::write(root.join("source/remove.txt"), "obsolete")?;
    run(
        root,
        &[
            "init",
            "source",
            "--project",
            "demo",
            "--remote",
            &url,
            "--no-watch",
        ],
    );
    let source = root.join("source");
    let first: PublicationReceipt =
        serde_json::from_value(run(&source, &["publish", "-m", "Example change"]))?;
    fs::write(source.join("file.txt"), "second")?;
    fs::remove_file(source.join("remove.txt"))?;
    let binary = [0, 255, 13, 10, 42];
    fs::write(source.join("asset.bin"), binary)?;
    run(&source, &["track", "asset.bin"]);
    let second: PublicationReceipt = serde_json::from_value(run(&source, &["publish"]))?;
    assert_eq!(first.change, second.change);
    assert_ne!(first.revision, second.revision);
    assert_eq!(run(&source, &["publish"])["state"], "already_published");
    assert_eq!(
        run(&source, &["changes"])[0]["heads"],
        serde_json::json!([second.revision])
    );
    run(
        root,
        &[
            "open",
            &url,
            "copy",
            "--project",
            "demo",
            "--change",
            &first.change,
            "--no-watch",
        ],
    );
    assert_eq!(fs::read_to_string(root.join("copy/file.txt"))?, "second");
    assert_eq!(fs::read(root.join("copy/asset.bin"))?, binary);
    assert!(!root.join("copy/remove.txt").exists());
    assert_eq!(
        run(&root.join("copy"), &["status"])["changed"],
        serde_json::json!([])
    );
    server.abort();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_first_publish_keeps_identity_and_retries_before_new_edits() -> anyhow::Result<()>
{
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    drop(listener);
    let url = format!("http://{address}");
    fs::write(root.join("file.txt"), "first attempt")?;
    run(
        root,
        &["init", "--project", "demo", "--remote", &url, "--no-watch"],
    );
    assert!(
        !command(root, &["publish", "-m", "Retry me"])
            .status
            .success()
    );
    let pending = Workspace::open(root)?
        .state
        .pending
        .clone()
        .expect("durable pending publication");
    run(root, &["remote", "remove"]);
    assert!(run(root, &["status"])["pending"].as_bool().unwrap());
    run(root, &["remote", "set", &url]);
    fs::write(root.join("file.txt"), "new edits while offline")?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    let remote_data = tempfile::tempdir()?;
    let app = kelp_remote::app(remote_data.path(), "test-token".into())?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let receipt: PublicationReceipt = serde_json::from_value(run(root, &["publish"]))?;
    assert_eq!(receipt.change, pending.revision.change);
    let workspace = Workspace::open(root)?;
    assert!(workspace.state.pending.is_none());
    let revision: Revision =
        storage::get_json(&workspace.db, "demo", "revision", &receipt.revision)?;
    assert_eq!(revision.predecessor, Some(pending.revision.id()?));
    server.abort();
    Ok(())
}

#[test]
fn background_watcher_captures_edits_without_a_checkpoint_command() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    fs::write(root.join("file.txt"), "before")?;
    run(root, &["init", "--project", "demo"]);
    struct StopWatcher<'a>(&'a Path);
    impl Drop for StopWatcher<'_> {
        fn drop(&mut self) {
            let _ = command(self.0, &["watch", "--stop"]);
        }
    }
    let _stop = StopWatcher(root);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !run(root, &["status"])["watcher_running"].as_bool().unwrap() {
        assert!(Instant::now() < deadline, "watcher did not start");
        std::thread::sleep(Duration::from_millis(100));
    }
    fs::write(root.join("file.txt"), "after")?;
    loop {
        let workspace = Workspace::open(root)?;
        let latest = workspace.checkpoints(1)?.pop().unwrap();
        let snapshot = workspace.checkpoint_snapshot(latest.id)?;
        let bytes = storage::get(
            &workspace.db,
            "demo",
            "blob",
            &snapshot.files["file.txt"].blob,
        )?;
        if bytes == b"after" {
            break;
        }
        drop(workspace);
        assert!(
            Instant::now() < deadline,
            "watcher did not checkpoint the edit"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

#[cfg(not(unix))]
#[test]
fn checkpoints_preserve_imported_executable_metadata() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("script.sh"), "echo hello")?;
    let mut workspace = Workspace::init(directory.path(), "demo", None)?;
    let latest = workspace.checkpoints(1)?.pop().unwrap();
    let mut imported = workspace.checkpoint_snapshot(latest.id)?;
    imported.files.get_mut("script.sh").unwrap().executable = true;
    workspace.state.base_snapshot =
        storage::put_json(&workspace.db, "demo", "snapshot", &imported)?;
    workspace.save_state()?;
    let checkpoint = workspace.capture(None)?;
    assert_eq!(
        workspace.checkpoint_snapshot(checkpoint.id)?.id()?,
        imported.id()?
    );
    assert!(workspace.status()?.changed.is_empty());
    Ok(())
}

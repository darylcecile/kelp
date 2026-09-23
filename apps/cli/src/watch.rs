use std::{
    fs::OpenOptions,
    path::Path,
    process::{Command, Stdio},
    sync::mpsc,
    time::Duration,
};

use anyhow::Result;
use fs2::FileExt;
use notify::{RecursiveMode, Watcher};

use crate::workspace::{Workspace, now};

pub fn start(workspace: &Workspace) -> Result<()> {
    workspace
        .db
        .execute("UPDATE watcher SET enabled = 1 WHERE id = 1", [])?;
    if workspace.status()?.watcher_running {
        return Ok(());
    }
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(workspace.root.join(".kelp/watch.log"))?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("--directory")
        .arg(&workspace.root)
        .arg("_watch")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command.spawn()?;
    Ok(())
}

pub fn stop(workspace: &Workspace) -> Result<()> {
    workspace
        .db
        .execute("UPDATE watcher SET enabled = 0 WHERE id = 1", [])?;
    Ok(())
}

pub fn run(root: &Path) -> Result<()> {
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join(".kelp/watch.lock"))?;
    if lock.try_lock_exclusive().is_err() {
        return Ok(());
    }
    let (sender, receiver) = mpsc::sync_channel(1);
    let watched_root = root.to_owned();
    let mut watcher =
        notify::recommended_watcher(move |event: notify::Result<notify::Event>| match event {
            Ok(event)
                if !event.kind.is_access()
                    && event.paths.iter().any(|path| {
                        !path.starts_with(watched_root.join(".kelp"))
                            && !path.starts_with(watched_root.join(".git"))
                    }) =>
            {
                let _ = sender.try_send(());
            }
            Err(error) => eprintln!("watch error: {error}"),
            _ => {}
        })?;
    watcher.watch(root, RecursiveMode::Recursive)?;
    let db = kelp_core::storage::open(&root.join(".kelp/workspace.sqlite3"))?;
    let mut dirty = true;
    loop {
        let enabled: bool =
            db.query_row("SELECT enabled FROM watcher WHERE id = 1", [], |r| r.get(0))?;
        if !enabled {
            break;
        }
        if dirty {
            match Workspace::open(root).and_then(|workspace| workspace.capture(None)) {
                Ok(_) => {}
                Err(error) => eprintln!("checkpoint failed: {error:#}"),
            }
        }
        db.execute("UPDATE watcher SET heartbeat = ?1 WHERE id = 1", [now()])?;
        dirty = receiver.recv_timeout(Duration::from_secs(1)).is_ok();
        if dirty {
            std::thread::sleep(Duration::from_millis(200));
            while receiver.try_recv().is_ok() {}
        }
    }
    db.execute("UPDATE watcher SET heartbeat = 0 WHERE id = 1", [])?;
    Ok(())
}

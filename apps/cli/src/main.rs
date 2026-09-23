use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use kelp_cli::{
    remote::{Remote, normalize_url},
    watch,
    workspace::{Workspace, materialize},
};
use serde::Serialize;

#[derive(Parser)]
#[command(name = "kelp", version, about)]
struct Cli {
    /// Run in this directory.
    #[arg(short = 'C', long, global = true, default_value = ".")]
    directory: PathBuf,
    /// Emit structured output.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Create a local workspace; no server or account is required.
    Init {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        remote: Option<String>,
        /// Do not start the automatic checkpoint process (useful in CI).
        #[arg(long)]
        no_watch: bool,
    },
    /// Show local edits, untracked files, and the active change.
    Status,
    /// Include new files or directories in future checkpoints and publications.
    Track {
        #[arg(required = true)]
        paths: Vec<PathBuf>,
    },
    /// Mark an optional named local checkpoint.
    Checkpoint {
        #[arg(short, long)]
        message: Option<String>,
    },
    /// List local checkpoints, newest first.
    Log {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Recover a checkpoint's files into a new directory.
    Restore {
        checkpoint: i64,
        #[arg(long)]
        to: PathBuf,
    },
    /// Capture and share current work, or update the same change after feedback.
    Publish {
        #[arg(short, long)]
        message: Option<String>,
    },
    /// List changes on the configured remote.
    Changes,
    /// Open a remote change as a new local workspace.
    Open {
        url: String,
        path: PathBuf,
        #[arg(long)]
        project: String,
        #[arg(long)]
        change: String,
        #[arg(long)]
        revision: Option<String>,
        #[arg(long)]
        no_watch: bool,
    },
    /// Configure a remote, or return to local-only operation.
    Remote {
        #[command(subcommand)]
        command: RemoteCommand,
    },
    /// Start a separate piece of work.
    Change {
        #[command(subcommand)]
        command: ChangeCommand,
    },
    /// Start automatic local checkpoints, or stop them with --stop.
    Watch {
        #[arg(long)]
        stop: bool,
    },
    #[command(name = "_watch", hide = true)]
    WatchWorker,
}

#[derive(Subcommand)]
enum RemoteCommand {
    /// Attach a server without uploading anything.
    Set { url: String },
    /// Detach the server while keeping all local history.
    Remove,
}

#[derive(Subcommand)]
enum ChangeCommand {
    /// Start new work based on the current files; previous checkpoints remain.
    New { message: String },
}

fn emit(json: bool, value: &impl Serialize, human: impl std::fmt::Display) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(value)?);
    } else {
        println!("{human}");
    }
    Ok(())
}

fn client(workspace: &Workspace) -> Result<Remote> {
    let url = workspace.state.remote.as_deref().context("this is a local-only workspace; attach a remote with `kelp remote set URL` when you want to publish")?;
    Remote::new(url, &workspace.state.project, token()?)
}

fn token() -> Result<String> {
    std::env::var("KELP_TOKEN").context("set KELP_TOKEN to the remote's access token")
}

fn run(cli: Cli) -> Result<()> {
    let directory = cli
        .directory
        .canonicalize()
        .context("working directory does not exist")?;
    match cli.command {
        Commands::Init {
            path,
            project,
            remote,
            no_watch,
        } => {
            let path = directory.join(path);
            std::fs::create_dir_all(&path)?;
            let path = path.canonicalize()?;
            let project = project
                .or_else(|| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .map(str::to_owned)
                })
                .context("supply --project NAME")?;
            let remote = remote.as_deref().map(normalize_url).transpose()?;
            let workspace = Workspace::init(&path, &project, remote)?;
            if !no_watch {
                watch::start(&workspace)?;
            }
            emit(
                cli.json,
                &workspace.state,
                format!(
                    "Initialized {} in {}.\nExisting non-ignored files are tracked. {}",
                    project,
                    path.display(),
                    if no_watch {
                        "Automatic checkpoints are stopped."
                    } else {
                        "Automatic local checkpoints started."
                    }
                ),
            )
        }
        Commands::Open {
            url,
            path,
            project,
            change,
            revision,
            no_watch,
        } => {
            let remote = Remote::new(&url, &project, token()?)?;
            let workspace = remote.open(
                &url,
                &project,
                &change,
                revision.as_deref(),
                &directory.join(path),
            )?;
            if !no_watch {
                watch::start(&workspace)?;
            }
            emit(
                cli.json,
                &workspace.state,
                format!("Opened change {change} in {}", workspace.root.display()),
            )
        }
        Commands::WatchWorker => watch::run(&directory),
        command => {
            let mut workspace = Workspace::open(&directory)?;
            match command {
                Commands::Status => {
                    let status = workspace.status()?;
                    let title = status
                        .message
                        .as_deref()
                        .unwrap_or("New work (not yet named)");
                    let mut text = format!(
                        "{title}\nProject: {}\nRemote: {}\nAutomatic checkpoints: {}\n",
                        status.project,
                        status.remote.as_deref().unwrap_or("local only"),
                        if status.watcher_running {
                            "running"
                        } else {
                            "stopped or starting"
                        }
                    );
                    if let Some(change) = &status.change {
                        text.push_str(&format!("Change: {change}\n"));
                    }
                    if let Some(id) = status.checkpoint {
                        text.push_str(&format!("Latest checkpoint: {id}\n"));
                    }
                    if status.pending {
                        text.push_str("Publication pending; run kelp publish to retry.\n");
                    }
                    for path in &status.changed {
                        text.push_str(&format!("  edited  {path}\n"));
                    }
                    for path in &status.untracked {
                        text.push_str(&format!("  untracked  {path}\n"));
                    }
                    if status.changed.is_empty() {
                        text.push_str("No edits since the last publication or base.\n");
                    }
                    emit(cli.json, &status, text.trim_end())
                }
                Commands::Track { paths } => {
                    let count = workspace.track(&paths, &directory)?;
                    emit(
                        cli.json,
                        &serde_json::json!({"tracked": count}),
                        format!("Tracking {count} new files."),
                    )
                }
                Commands::Checkpoint { message } => {
                    let checkpoint = workspace.capture(message.as_deref())?;
                    emit(
                        cli.json,
                        &checkpoint,
                        format!(
                            "Local checkpoint {}: {}",
                            checkpoint.id, checkpoint.snapshot
                        ),
                    )
                }
                Commands::Log { limit } => {
                    let checkpoints = workspace.checkpoints(limit.min(1000))?;
                    let text = checkpoints
                        .iter()
                        .map(|c| {
                            format!(
                                "{}  {}  {}",
                                c.id,
                                &c.snapshot[..12],
                                c.message.as_deref().unwrap_or("Automatic checkpoint")
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    emit(cli.json, &checkpoints, text)
                }
                Commands::Restore { checkpoint, to } => {
                    let snapshot = workspace.checkpoint_snapshot(checkpoint)?;
                    let to = directory.join(to);
                    materialize(&workspace.db, &workspace.state.project, &snapshot, &to)?;
                    emit(
                        cli.json,
                        &serde_json::json!({"checkpoint": checkpoint, "destination": to}),
                        format!("Recovered checkpoint {checkpoint} to {}", to.display()),
                    )
                }
                Commands::Publish { message } => {
                    let remote = client(&workspace)?;
                    if let Some(pending) = workspace.state.pending.clone() {
                        let receipt = remote.publish(&workspace, &pending)?;
                        workspace.acknowledge(&pending)?;
                        if !cli.json {
                            println!("Confirmed pending revision {}", receipt.revision);
                        }
                    }
                    match workspace.prepare_publication(message.as_deref())? {
                        Some(publication) => {
                            let receipt = remote.publish(&workspace, &publication)?;
                            workspace.acknowledge(&publication)?;
                            emit(
                                cli.json,
                                &receipt,
                                format!(
                                    "Published change {}\nRevision: {}\n{}",
                                    receipt.change,
                                    receipt.revision,
                                    if receipt.heads.len() > 1 {
                                        "This change has divergent revisions; all contributions are retained."
                                    } else {
                                        "Edit and run kelp publish again to update this change."
                                    }
                                ),
                            )
                        }
                        None => emit(
                            cli.json,
                            &serde_json::json!({"state": "already_published", "change": workspace.state.change, "revision": workspace.state.head}),
                            "Already published; no new edits.",
                        ),
                    }
                }
                Commands::Changes => {
                    let changes = client(&workspace)?.changes()?;
                    let text = changes
                        .iter()
                        .map(|change| {
                            format!("{}  {} revision head(s)", change.change, change.heads.len())
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    emit(cli.json, &changes, text)
                }
                Commands::Remote { command } => {
                    workspace.state.remote = match command {
                        RemoteCommand::Set { url } => Some(normalize_url(&url)?),
                        RemoteCommand::Remove => None,
                    };
                    workspace.save_state()?;
                    emit(
                        cli.json,
                        &serde_json::json!({"remote": workspace.state.remote}),
                        format!(
                            "Remote: {}",
                            workspace.state.remote.as_deref().unwrap_or("local only")
                        ),
                    )
                }
                Commands::Change {
                    command: ChangeCommand::New { message },
                } => {
                    workspace.new_change(message.clone())?;
                    emit(
                        cli.json,
                        &workspace.state,
                        format!(
                            "Started separate work: {message}\nCurrent files are the base; make edits and publish when ready."
                        ),
                    )
                }
                Commands::Watch { stop } => {
                    if stop {
                        watch::stop(&workspace)?;
                    } else {
                        watch::start(&workspace)?;
                    }
                    emit(
                        cli.json,
                        &serde_json::json!({"enabled": !stop}),
                        if stop {
                            "Automatic checkpoints stopping."
                        } else {
                            "Automatic checkpoints started."
                        },
                    )
                }
                Commands::Init { .. } | Commands::Open { .. } | Commands::WatchWorker => {
                    unreachable!()
                }
            }
        }
    }
}

fn main() {
    let cli = Cli::parse();
    let json = cli.json;
    if let Err(error) = run(cli) {
        if json {
            eprintln!("{}", serde_json::json!({"error": format!("{error:#}")}));
        } else {
            eprintln!("error: {error:#}");
        }
        std::process::exit(1);
    }
}

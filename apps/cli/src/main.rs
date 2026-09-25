use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use kelp_cli::{
    remote::{Remote, project_location, project_url},
    workspace::{FileDiff, Resolution, Workspace},
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
    /// Start a project locally. No account or remote needed.
    Init {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Project name (defaults to the directory name).
        #[arg(long)]
        project: Option<String>,
        /// Optional project URL, such as https://code.example.com/my-project.
        #[arg(long)]
        remote: Option<String>,
    },
    /// Download a shared project and its saved history.
    Clone {
        url: String,
        /// Destination directory (defaults to the project name).
        path: Option<PathBuf>,
        /// Download only these project-relative files/directories (comma-separated or repeated).
        #[arg(long, value_delimiter = ',')]
        paths: Vec<PathBuf>,
    },
    /// Show uncommitted files and commits ready to push.
    Status,
    /// Import a local Git repository and the selected revision's history.
    Import {
        source: PathBuf,
        path: Option<PathBuf>,
        #[arg(long = "ref", default_value = "HEAD")]
        revision: String,
        #[arg(long)]
        project: Option<String>,
    },
    /// Pin a committed release view, or list pinned views.
    Tag {
        name: Option<String>,
        view: Option<String>,
    },
    /// Preview edits not yet committed.
    Diff,
    /// Save a described version locally, without sharing it.
    Commit {
        #[arg(short, long)]
        message: String,
    },
    /// Send committed work to the remote. Leaves uncommitted edits local.
    Push { url: Option<String> },
    /// Get remote updates, preserving independent local edits.
    Pull {
        /// Also download these paths; use --paths . for the whole project.
        #[arg(long, value_delimiter = ',')]
        paths: Vec<PathBuf>,
        /// Keep your files where both sides edited the same path.
        #[arg(long, conflicts_with = "keep_remote")]
        keep_local: bool,
        /// Use remote files where both sides edited the same path.
        #[arg(long)]
        keep_remote: bool,
    },
    /// List saved versions, their descriptions, and changed files.
    Log {
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// List edit transactions, in dependency order.
        #[arg(long)]
        commits: bool,
    },
    /// Show a saved view or commit by hash (unique prefixes are accepted).
    Show { version: String },
    /// Restore a complete saved view by hash, keeping a backup of current work.
    Restore { view: String },
    /// Compact local storage without deleting commits or recovery views.
    Gc,
    /// Show or configure the remote project URL.
    Remote {
        #[command(subcommand)]
        command: Option<RemoteCommand>,
    },
}

#[derive(Subcommand)]
enum RemoteCommand {
    /// Remember a project URL without uploading anything.
    Set { url: String },
    /// Work locally, keeping all saved versions.
    Remove,
}

fn emit(json: bool, value: &impl Serialize, human: impl std::fmt::Display) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(value)?);
    } else {
        println!("{human}");
    }
    Ok(())
}

fn token() -> Result<String> {
    std::env::var("KELP_TOKEN")
        .context("set KELP_TOKEN to the access token supplied by your remote's operator")
}

fn client(workspace: &Workspace) -> Result<Remote> {
    let url = workspace.state.remote.as_deref().context("no remote configured; use `kelp push URL` to share, or `kelp commit -m \"Description\"` to save locally")?;
    Remote::new(url, &workspace.state.project, token()?)
}

fn location(workspace: &Workspace) -> Option<String> {
    workspace
        .state
        .remote
        .as_ref()
        .map(|base| project_url(base, &workspace.state.project))
}

fn display_diff(json: bool, files: Vec<FileDiff>) -> Result<()> {
    let text = files
        .iter()
        .map(|file| {
            format!(
                "{} {}\n{}",
                file.file.kind,
                kelp_core::paths::display(&file.file.path),
                file.diff
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    emit(
        json,
        &files,
        if text.is_empty() {
            "No file changes."
        } else {
            text.trim_end()
        },
    )
}

fn path_arguments(paths: Vec<PathBuf>) -> Result<Vec<String>> {
    paths
        .into_iter()
        .map(|path| {
            anyhow::ensure!(
                !path.components().any(|part| matches!(
                    part,
                    std::path::Component::Prefix(_)
                        | std::path::Component::RootDir
                        | std::path::Component::ParentDir
                )),
                "--paths needs project-relative paths without .."
            );
            let clean: PathBuf = path
                .components()
                .filter(|part| !matches!(part, std::path::Component::CurDir))
                .collect();
            if clean.as_os_str().is_empty() && !path.as_os_str().is_empty() {
                return Ok(".".into());
            }
            kelp_core::paths::from_native(&clean)
        })
        .collect()
}

fn run(cli: Cli) -> Result<()> {
    let directory = cli
        .directory
        .canonicalize()
        .context("working directory does not exist")?;
    match cli.command {
        Commands::Import {
            source,
            path,
            revision,
            project,
        } => {
            let source = directory.join(source).canonicalize()?;
            let name = project.unwrap_or_else(|| {
                source
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .trim_end_matches(".git")
                    .to_owned()
            });
            let destination =
                directory.join(path.unwrap_or_else(|| PathBuf::from(format!("{name}-kelp"))));
            let workspace = kelp_cli::import::repository(&source, &destination, &name, &revision)?;
            emit(
                cli.json,
                &serde_json::json!({"project": name, "directory": workspace.root, "commits": workspace.state.transactions.len()}),
                format!(
                    "Imported {} commits into {}. Use kelp push to share them.",
                    workspace.state.transactions.len(),
                    workspace.root.display()
                ),
            )
        }
        Commands::Init {
            path,
            project,
            remote,
        } => {
            let path = directory.join(path);
            std::fs::create_dir_all(&path)?;
            let path = path.canonicalize()?;
            let (base, project) = if let Some(url) = remote {
                let (base, name) = project_location(&url)?;
                ensure!(
                    project.as_ref().is_none_or(|project| *project == name),
                    "--project must match the project in the remote URL"
                );
                (Some(base), name)
            } else {
                let name = project.unwrap_or_else(|| {
                    path.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .chars()
                        .map(|c| {
                            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                                c
                            } else {
                                '-'
                            }
                        })
                        .collect()
                });
                (None, name)
            };
            let workspace = Workspace::init(&path, &project, base)?;
            emit(
                cli.json,
                &serde_json::json!({"project": project, "directory": path, "remote": location(&workspace)}),
                format!(
                    "Initialized Kelp in {}.\nEdit files, commit locally, and push when ready to share.",
                    path.display()
                ),
            )
        }
        Commands::Clone { url, path, paths } => {
            let (base, project) = project_location(&url)?;
            let remote = Remote::new(&base, &project, token()?)?;
            let destination = directory.join(path.unwrap_or_else(|| PathBuf::from(&project)));
            let workspace =
                remote.clone_paths(&base, &project, &destination, path_arguments(paths)?)?;
            emit(
                cli.json,
                &serde_json::json!({"project": project, "directory": workspace.root, "remote": location(&workspace), "paths": workspace.state.paths}),
                format!(
                    "Cloned {project} into {}.{}",
                    workspace.root.display(),
                    if workspace.state.paths.is_empty() {
                        String::new()
                    } else {
                        format!(
                            "\nSelected paths: {}. Other file contents were not downloaded.",
                            workspace.state.paths.join(", ")
                        )
                    }
                ),
            )
        }
        command => {
            let mut workspace = Workspace::open(&directory)?;
            match command {
                Commands::Tag { name, view } => {
                    if let Some(name) = name {
                        let pin = workspace.tag(name, view.as_deref())?;
                        emit(
                            cli.json,
                            &serde_json::json!({"name": pin.name, "view": pin.view()?.id()?}),
                            format!(
                                "Pinned {} to view {}. Use kelp push to share it.",
                                pin.name,
                                pin.view()?.id()?
                            ),
                        )
                    } else {
                        let pins = workspace.pins()?;
                        let entries = pins
                            .iter()
                            .map(|pin| {
                                Ok(serde_json::json!({"name": pin.name, "view": pin.view()?.id()?}))
                            })
                            .collect::<Result<Vec<_>>>()?;
                        let text = entries
                            .iter()
                            .map(|entry| {
                                format!(
                                    "{}  {}",
                                    entry["name"].as_str().unwrap(),
                                    entry["view"].as_str().unwrap()
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        emit(cli.json, &entries, text)
                    }
                }
                Commands::Status => {
                    let mut status = workspace.status()?;
                    status.remote = location(&workspace);
                    let mut text = format!(
                        "Project: {}\nRemote: {}\n",
                        status.project,
                        status.remote.as_deref().unwrap_or("local only")
                    );
                    if !status.paths.is_empty() {
                        text.push_str(&format!("Selected paths: {}\n", status.paths.join(", ")));
                    }
                    if let Some(version) = &status.version {
                        text.push_str(&format!(
                            "Last saved view: {}\n",
                            workspace.short_hash(version)?
                        ));
                    } else {
                        text.push_str("No saved versions yet.\n");
                    }
                    if status.pending_commits != 0 {
                        text.push_str(&format!(
                            "{} committed version(s) ready to push.\n",
                            status.pending_commits
                        ));
                    }
                    for path in &status.changed {
                        text.push_str(&format!(
                            "  uncommitted  {}\n",
                            kelp_core::paths::display(path)
                        ));
                    }
                    if status.changed.is_empty() {
                        text.push_str("No uncommitted edits.\n");
                    }
                    if !status.conflicts.is_empty() {
                        text.push_str(&format!(
                            "Commit a resolution for: {}\n",
                            status
                                .conflicts
                                .iter()
                                .cloned()
                                .collect::<Vec<_>>()
                                .join(", ")
                        ));
                    }
                    emit(cli.json, &status, text.trim_end())
                }
                Commands::Diff => display_diff(cli.json, workspace.diff()?),
                Commands::Show { version } => {
                    display_diff(cli.json, workspace.show_reference(&version)?)
                }
                Commands::Commit { message } => {
                    let version = workspace.commit(&message)?;
                    emit(
                        cli.json,
                        &version,
                        format!(
                            "Commit {}: {message}\nSaved view {} — use this hash to restore this checkout.",
                            workspace.short_hash(
                                version
                                    .transaction
                                    .as_ref()
                                    .context("commit ID is missing")?
                            )?,
                            workspace.short_hash(&version.view)?
                        ),
                    )
                }
                Commands::Log {
                    limit,
                    commits: true,
                } => {
                    let graph = workspace.graph()?;
                    let entries: Vec<_> = graph.ordered()?.into_iter().rev().take(limit.min(1000)).map(|id| {
                        let transaction = &graph.transactions[&id];
                        serde_json::json!({"commit": id, "message": transaction.message, "files": transaction.edits.keys().collect::<Vec<_>>()})
                    }).collect();
                    let text = entries
                        .iter()
                        .map(|entry| -> Result<String> {
                            Ok(format!(
                                "{}  {}\n   {}",
                                workspace.short_hash(entry["commit"].as_str().unwrap())?,
                                entry["message"].as_str().unwrap(),
                                entry["files"]
                                    .as_array()
                                    .unwrap()
                                    .iter()
                                    .map(|path| path.as_str().unwrap())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ))
                        })
                        .collect::<Result<Vec<_>>>()?
                        .join("\n");
                    emit(
                        cli.json,
                        &entries,
                        if text.is_empty() {
                            "No commits yet."
                        } else {
                            &text
                        },
                    )
                }
                Commands::Log {
                    limit,
                    commits: false,
                } => {
                    let history = workspace.history(limit.min(1000))?;
                    let text = history
                        .iter()
                        .map(|entry| -> Result<String> {
                            let hash = workspace.short_hash(&entry.version.view)?;
                            let mut text = format!(
                                "{}  {}\n",
                                hash,
                                entry.version.message.as_deref().unwrap_or("Saved version")
                            );
                            for file in entry.files.iter().take(5) {
                                text.push_str(&format!(
                                    "   {} {}\n",
                                    file.kind,
                                    kelp_core::paths::display(&file.path)
                                ));
                            }
                            if entry.files.len() > 5 {
                                text.push_str(&format!(
                                    "   + {} more; kelp show {}\n",
                                    entry.files.len() - 5,
                                    hash
                                ));
                            }
                            Ok(text)
                        })
                        .collect::<Result<Vec<_>>>()?
                        .join("\n");
                    emit(
                        cli.json,
                        &history,
                        if text.is_empty() {
                            "No saved versions yet."
                        } else {
                            text.trim_end()
                        },
                    )
                }
                Commands::Restore { view } => {
                    let (restored, backup) = workspace.restore_reference(&view)?;
                    emit(
                        cli.json,
                        &serde_json::json!({"restored": restored, "backup": backup}),
                        format!(
                            "Restored view {} in this folder.\nPrevious work is saved as view {}.",
                            workspace.short_hash(&restored)?,
                            workspace.short_hash(&backup)?
                        ),
                    )
                }
                Commands::Gc => {
                    let report = workspace.compact()?;
                    emit(
                        cli.json,
                        &report,
                        format!(
                            "Compacted storage for {} objects ({} packed groups).\nStored object payload: {} → {} bytes. All history retained.",
                            report.objects,
                            report.packs,
                            report.payload_before,
                            report.payload_after
                        ),
                    )
                }
                Commands::Push { url } => {
                    if let Some(url) = url {
                        let (base, project) = project_location(&url)?;
                        workspace.set_remote(base, project)?;
                    }
                    let remote = client(&workspace)?;
                    let count = remote.push(&mut workspace)?;
                    emit(
                        cli.json,
                        &serde_json::json!({"pushed": count, "view": workspace.projection()?.id()?, "remote": location(&workspace)}),
                        if count == 0 {
                            "No committed work to push. Use `kelp commit -m \"Description\"` to save edits first.".to_owned()
                        } else {
                            format!(
                                "Pushed {count} committed version(s) to {}.",
                                location(&workspace).unwrap_or_default()
                            )
                        },
                    )
                }
                Commands::Pull {
                    keep_local,
                    keep_remote,
                    paths,
                } => {
                    let resolution = if keep_local {
                        Resolution::Local
                    } else if keep_remote {
                        Resolution::Remote
                    } else {
                        Resolution::Stop
                    };
                    let updated = client(&workspace)?.pull_paths(
                        &mut workspace,
                        resolution,
                        path_arguments(paths)?,
                    )?;
                    emit(
                        cli.json,
                        &serde_json::json!({"updated": updated, "view": workspace.projection()?.id()?}),
                        if updated {
                            "Pulled updates. Your independent edits are preserved."
                        } else {
                            "Up to date."
                        },
                    )
                }
                Commands::Remote { command } => {
                    match command {
                        Some(RemoteCommand::Set { url }) => {
                            let (base, project) = project_location(&url)?;
                            workspace.set_remote(base, project)?;
                        }
                        Some(RemoteCommand::Remove) => {
                            workspace.state.remote = None;
                            workspace.save_state()?;
                        }
                        None => {}
                    }
                    let remote = location(&workspace);
                    emit(
                        cli.json,
                        &serde_json::json!({"remote": remote}),
                        format!("Remote: {}", remote.as_deref().unwrap_or("local only")),
                    )
                }
                Commands::Init { .. } | Commands::Clone { .. } | Commands::Import { .. } => {
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

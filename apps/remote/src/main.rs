use std::{net::SocketAddr, path::PathBuf};

use clap::Parser;

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Address on which to serve HTTP.
    #[arg(long, env = "KELP_LISTEN", default_value = "127.0.0.1:8080")]
    listen: SocketAddr,
    /// Persistent object and metadata storage.
    #[arg(long, env = "KELP_DATA_DIR", default_value = "data")]
    data_dir: PathBuf,
    /// Shared bearer token used by clients. Prefer the environment variable.
    #[arg(long, env = "KELP_TOKEN", hide_env_values = true)]
    token: String,
    /// Run a private storage node for a gateway.
    #[arg(long, conflicts_with = "shards")]
    storage_only: bool,
    /// Fixed storage-node URLs; omit for a single-node remote.
    #[arg(long, env = "KELP_SHARDS", value_delimiter = ',')]
    shards: Vec<String>,
    /// Gateway credential for the private storage nodes.
    #[arg(long, env = "KELP_STORAGE_TOKEN", hide_env_values = true)]
    storage_token: Option<String>,
    /// Compact a stopped storage node or standalone database, then exit.
    #[arg(long, conflicts_with = "shards")]
    compact: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "kelp_remote=info".into()),
        )
        .init();
    let args = Args::parse();
    if args.compact {
        let path = args.data_dir.join("kelp.sqlite3");
        anyhow::ensure!(path.is_file(), "no storage database at {}", path.display());
        let db = kelp_core::storage::open(&path)?;
        let report = kelp_core::storage::compact(&db, &Default::default())?;
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    let app = if args.storage_only {
        kelp_remote::storage_app(&args.data_dir, args.token)?
    } else if args.shards.is_empty() {
        kelp_remote::app(&args.data_dir, args.token)?
    } else {
        kelp_remote::cluster_app(
            args.shards,
            args.token,
            args.storage_token
                .ok_or_else(|| anyhow::anyhow!("set KELP_STORAGE_TOKEN for the storage nodes"))?,
        )?
    };
    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    tracing::info!(address = %listener.local_addr()?, "Kelp remote ready");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

async fn shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    {
        let terminate = async {
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(mut signal) => {
                    signal.recv().await;
                }
                Err(_) => std::future::pending::<()>().await,
            }
        };
        tokio::select! { _ = ctrl_c => {}, _ = terminate => {} }
    }
    #[cfg(not(unix))]
    ctrl_c.await;
}

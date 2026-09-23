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
    let app = kelp_remote::app(&args.data_dir, args.token)?;
    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    tracing::info!(address = %listener.local_addr()?, data = %args.data_dir.display(), "Kelp remote ready");
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

#![forbid(unsafe_code)]

use std::error::Error;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use joedlna_core::catalog::Catalog;
use joedlna_core::{Config, ConfigError, ServerError};
use thiserror::Error;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(version, about = "A filesystem-first Rust DLNA media server")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Validate the configuration and media share paths without opening sockets.
    CheckConfig {
        #[arg(short, long, default_value = "config.toml")]
        config: PathBuf,
    },
    /// Scan shares, advertise the server, and serve media until interrupted.
    Serve {
        #[arg(short, long, default_value = "config.toml")]
        config: PathBuf,
    },
}

#[derive(Debug, Error)]
enum MainError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Server(#[from] ServerError),
    #[error("failed to initialize logging: {0}")]
    Logging(#[source] Box<dyn Error + Send + Sync>),
}

#[tokio::main]
async fn main() -> Result<(), MainError> {
    init_tracing().map_err(MainError::Logging)?;
    let cli = Cli::parse();
    match cli.command {
        Command::CheckConfig { config } => {
            let parsed = Config::load(&config)?;
            let catalog = Catalog::scan(&parsed).map_err(ServerError::from)?;
            println!(
                "configuration is valid: {} share(s), {} media file(s), {} byte(s)",
                parsed.shares.len(),
                catalog.file_count(),
                catalog.total_bytes()
            );
        }
        Command::Serve { config } => {
            let parsed = Config::load(&config)?;
            joedlna_core::serve(config, parsed).await?;
        }
    }
    Ok(())
}

fn init_tracing() -> Result<(), Box<dyn Error + Send + Sync>> {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("joedlna_core=info,joedlna=info"));
    #[cfg(distribute)]
    return tracing_subscriber::fmt()
        .with_env_filter(filter)
        .json()
        .try_init();
    #[cfg(not(distribute))]
    tracing_subscriber::fmt().with_env_filter(filter).try_init()
}

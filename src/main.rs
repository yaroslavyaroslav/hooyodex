mod app;
mod approval;
mod channels;
mod codex;
mod config;
mod markdown;
mod service;
mod state;
mod telegram;
mod transcription;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::config::AppConfig;

#[derive(Parser, Debug)]
#[command(name = "hooyodex")]
#[command(about = "Telegram gate for persistent Codex app-server threads")]
struct Cli {
    #[arg(long)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    Run,
    PrintPaths,
    SetWebhook,
    DeleteWebhook,
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },
}

#[derive(Subcommand, Debug)]
enum ServiceCommand {
    Install,
    Uninstall,
    Start,
    Stop,
    Restart,
    Reload,
    Status,
    Print,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "hooyodex=info".into()),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::PrintPaths => {
            let paths = config::AppPaths::from_explicit(cli.config)?;
            println!("config={}", paths.config_path.display());
            println!("state_dir={}", paths.state_dir.display());
            println!("cache_dir={}", paths.cache_dir.display());
        }
        Command::Run => {
            let config = AppConfig::load(cli.config)?;
            app::run(config).await?;
        }
        Command::SetWebhook => {
            let config = AppConfig::load(cli.config)?;
            telegram::set_webhook(&config).await?;
        }
        Command::DeleteWebhook => {
            let config = AppConfig::load(cli.config)?;
            telegram::delete_webhook(&config).await?;
        }
        Command::Service { command } => {
            let paths = config::AppPaths::from_explicit(cli.config)?;
            service::handle(command, &paths)?;
        }
    }

    Ok(())
}

mod account;
mod api;
mod auth;
mod config;
mod msgtype;
mod player;
mod plugin;
mod proto;
mod cmaf;
mod secret;
mod session;
mod ws;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

/// qconnect — Qobuz source plugin.
#[derive(Parser, Debug)]
#[command(name = "qconnect", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run as a source plugin: JSON-RPC on stdin/stdout, logs on stderr.
    /// Started by the host player, not by hand.
    Plugin {
        /// Qobuz API root (tests point it at a mock server).
        #[arg(long, hide = true)]
        api_base: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let Command::Plugin { api_base } = Cli::parse().command;
    // stdout carries the protocol; the host copies stderr to its log.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
    plugin::run(api_base.as_deref()).await
}

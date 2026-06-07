//! princeps-faucet — v0 public-testnet faucet for the Princeps
//! testnet (Stage T4 of `docs/plans/v0-testnet-deploy.md`).
//!
//! What this binary becomes when T4 is complete: a small HTTP
//! service that accepts a `(captcha, recipient_address)` POST,
//! checks rate limits in SQLite, signs and submits a transfer
//! of testnet ETH + testnet USDC to the recipient, returns the
//! tx hash. Runs as an independent process behind its own
//! domain (TD-007 — faucet outage doesn't degrade the chain).
//!
//! T4a-1 scope (this commit): crate skeleton, axum HTTP server,
//! `GET /health` and `GET /status` endpoints, config-file
//! loading, observability boot. NO rate-limiting (T4a-2), NO
//! captcha verification (T4a-4), NO on-chain transfer (T4a-5).
//!
//! Future-slice hooks intentionally left visible:
//! - [`config::FaucetConfig`] grows fields per slice; T4a-1 ships
//!   only what `/status` and the server bring-up need.
//! - [`server::serve`] is the single entry point for future
//!   middleware (rate limiting, request logging, etc.).

mod captcha;
mod config;
mod observability;
mod rate_limit;
mod server;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "princeps-faucet", about = "v0 testnet faucet for the Princeps chain")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Serve the HTTP API against the supplied config file. This is
    /// the only subcommand at T4a-1 — future slices may add
    /// `check-config`, `drain` (for ordered shutdown), and so on.
    Serve {
        /// Path to the faucet config JSON. See
        /// `princeps/ops/faucet/config.example.json` for the
        /// committed reference shape.
        #[arg(long)]
        config: PathBuf,
    },
}

fn main() -> eyre::Result<()> {
    // Match the princeps-bin tracing default so operators don't
    // need to set RUST_LOG just to see startup messages.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .try_init()
        .ok();

    let cli = Cli::parse();
    match cli.command {
        Command::Serve { config } => {
            let cfg = config::FaucetConfig::load(&config)?;
            tokio_rt()?.block_on(server::serve(cfg))
        }
    }
}

fn tokio_rt() -> eyre::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(Into::into)
}

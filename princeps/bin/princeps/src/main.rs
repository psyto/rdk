//! princeps — Hyperliquid-shape L1 reference implementation.
//!
//! Three subcommands:
//!
//!   - `info` (default) — print the node's static config + initial state.
//!   - `devnet [N]` — `N` single-validator consensus rounds through an
//!     in-memory EVM bridge, calling `PrincepsNode::tick` between blocks.
//!     Stage 13b. The smallest runnable demo of the full per-block flow
//!     at the binary level.
//!   - `reth-devnet [N]` — Boots the production-shape stack: Reth via
//!     `NodeBuilder` + `PrincepsExecutorBuilder`, then `LiveRethEvmBridge`
//!     against its provider, then the Malachite actor engine via
//!     consensus `PrincepsNode::start`, then `run_engine_app` to drive
//!     consensus decisions. Stage 13c.
//!
//!     Stage 13d + 8e make `reth-devnet N` produce N real blocks
//!     end-to-end. 13d plumbed Reth's `ChainSpec::genesis_hash()` as
//!     the consensus engine's initial parent. 8e made the bridge's
//!     `build_payload` consult its own internal `chain` map for parent
//!     lookup before falling back to Reth's provider — the bridge's
//!     `commit` doesn't upload an `ExecutionPayload` to Reth (the
//!     synthetic headers have placeholder `state_root`s that Reth would
//!     reject), but consensus only needs the bridge to be
//!     self-consistent, which it now is.
//!
//! Examples:
//!   $ princeps                                      # equivalent to `princeps info`
//!   $ princeps info
//!   $ princeps devnet                               # one in-memory round
//!   $ princeps devnet --rounds 5                    # five in-memory rounds
//!   $ princeps reth-devnet                          # one Reth-backed decision
//!   $ princeps reth-devnet --rounds 3
//!   $ princeps reth-devnet --moniker alice --data-dir ~/.princeps/data
//!
//! Stage 13e (this commit) introduces clap-based subcommands and the
//! `--moniker` / `--data-dir` flags. Full production `NodeBuilder` path
//! (persistent across restarts, real network config, multi-validator)
//! lands in Stage 13f.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use alloy_genesis::Genesis;
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use informalsystems_malachitebft_app::node::{Node, NodeHandle};
use informalsystems_malachitebft_signing_ed25519::PrivateKey;
use princeps_consensus::run_engine_app;
use princeps_consensus::run_single_validator;
use princeps_consensus::PrincepsPrivateKeyFile;
use rdk_clob::{AccountId as ClobAccountId, Order, OrderId, OrderType, Price, Qty, Side};
use princeps_evm::{BridgeSnapshot, InMemoryEvmBridge, LiveRethEvmBridge, PrincepsExecutorBuilder};
use k256::ecdsa::{signature::Signer, SigningKey};
use rdk_funding::{IndexPrice, MarkPrice, Notional, PositionSize};
use rdk_liquidation::{AccountSnapshot, CloseOutcomeKind};
use princeps_node::{CoordinatorSnapshot, PrincepsNode, PrincepsNodeConfig, TickInput, TickReport};
use rdk_oracle::{FeedId, PriceObservation, PublisherKey, Signature as OracleSignature};
use rdk_types::BlockHash;
use rand::rngs::OsRng;
use reth_chainspec::ChainSpec;
use reth_db::{init_db, mdbx::DatabaseArguments};
use reth_node_builder::{NodeBuilder, NodeHandle as RethNodeHandle};
use reth_node_core::{
    args::DatadirArgs,
    dirs::{DataDirPath, MaybePlatformPath},
    node_config::NodeConfig,
};
use reth_node_ethereum::{node::EthereumAddOns, EthereumNode};
use reth_tasks::Runtime;
use sha2::{Digest, Sha256};

#[derive(Debug, Parser)]
#[command(
    name = "princeps",
    version,
    about = "Hyperliquid-shape L1 reference implementation",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print the node's static config and initial state (default).
    Info,

    /// Drive single-validator consensus rounds through an in-memory bridge,
    /// calling `PrincepsNode::tick` between blocks. Stage 13b demo path.
    Devnet {
        /// Number of consensus rounds to drive.
        #[arg(long, default_value_t = 1)]
        rounds: u64,
    },

    /// Stage 24b: Cross-margin demo. Walk Alice's canonical scenario
    /// end-to-end against an in-memory bridge — deposit USDC collateral,
    /// borrow ETH, open a perp position, simulate an ETH price crash,
    /// show siloed-LIQUIDATABLE becoming unified-HEALTHY via cross-margin.
    /// No Reth / Malachite — fully standalone, runs in ~1 second.
    LendingDemo {
        /// ETH price after the simulated crash (USDC per ETH unit).
        /// Default 90 produces the canonical "siloed liquidatable but
        /// unified healthy" result. Try other values to explore the
        /// cross-margin envelope.
        #[arg(long, default_value_t = 90)]
        eth_crash_price: u128,
    },

    /// Stage 24c: hands-on lending sandbox CLI. Operates on a local JSON
    /// state file at `$HOME/.princeps/lending-state.json` (overridable).
    /// Each subcommand loads → mutates via bridge methods → saves, so
    /// state persists across invocations and lets users explore lending
    /// flows without writing Rust code.
    ///
    /// Examples:
    ///   princeps lending init                          # create empty state
    ///   princeps lending deposit 1 1000                # account 1 deposits 1000 USDC
    ///   princeps lending borrow 1 200 --eth-price 1    # account 1 borrows 200 ETH
    ///   princeps lending health 1 --eth-price 2        # check health at ETH=2
    ///   princeps lending scan --eth-price 2            # find underwater accounts
    ///   princeps lending list                          # show all positions
    Lending {
        #[command(subcommand)]
        action: LendingCommand,
    },

    /// Drive consensus decisions through Reth-backed `LiveRethEvmBridge` +
    /// the Malachite actor engine. Stage 13c-e production-shape boot.
    RethDevnet {
        /// Number of consensus decisions to drive.
        #[arg(long, default_value_t = 1)]
        rounds: u64,

        /// Moniker for the consensus node identity (used in logs / network
        /// p2p discovery when wired). Default: princeps-reth-devnet.
        #[arg(long, default_value = "princeps-reth-devnet")]
        moniker: String,

        /// Data directory for Reth's MDBX database and the consensus
        /// home dir. Defaults to `$HOME/.princeps/data`.
        ///
        /// Stage 13f swapped this to the production `NodeBuilder` path
        /// (`reth_db::init_db` + `with_database` + `with_launch_context`),
        /// so the directory is now a real persistent MDBX database — it
        /// is **not** deleted at process exit. Re-running with the same
        /// `--data-dir` opens the existing database. Stages 13g–13i
        /// added bridge snapshot, validator-key, and consensus-height
        /// resume on top.
        #[arg(long)]
        data_dir: Option<PathBuf>,

        /// Path to a JSON chain spec file (Stage 13j). If omitted, the
        /// embedded dev chain spec (chain id 2600) is used — the same
        /// one Reth uses in its `examples/custom-dev-node`. Real
        /// deployments load a per-network spec.
        ///
        /// Format: the standard `alloy_genesis::Genesis` JSON
        /// (`nonce` / `timestamp` / `extraData` / `gasLimit` /
        /// `difficulty` / `mixHash` / `coinbase` / `alloc` / `number` /
        /// `gasUsed` / `parentHash` / `config`).
        #[arg(long)]
        chain_spec: Option<PathBuf>,

        /// Path to a JSON validator-set file (Stage 13j). If omitted,
        /// a single-validator set is constructed from the loaded
        /// (or freshly generated) validator key — the existing
        /// behavior through Stage 13h.
        ///
        /// Format:
        /// ```json
        /// {
        ///   "validators": [
        ///     {
        ///       "pubkey_hex": "<64 hex chars>",
        ///       "voting_power": 1,
        ///       "peer_multiaddr": "/ip4/10.0.0.5/tcp/9000"
        ///     }
        ///   ]
        /// }
        /// ```
        ///
        /// `peer_multiaddr` is optional (Stage 13k) and currently
        /// only logged — full vote relay wiring is a follow-up.
        ///
        /// When supplied, the locally-loaded validator key's public
        /// key must appear in the set, otherwise the node refuses to
        /// start — refusing to sign on behalf of an identity the
        /// network doesn't recognize.
        #[arg(long)]
        validators: Option<PathBuf>,

        /// libp2p listen multiaddr for this node's consensus engine
        /// (Stage 13k). Default: `/ip4/127.0.0.1/tcp/0` (ephemeral
        /// local port — single-validator devnet default). For
        /// multi-validator deployments use `/ip4/0.0.0.0/tcp/<port>`
        /// so peers can dial in.
        #[arg(long)]
        listen_addr: Option<String>,

        /// Reth HTTP RPC server bind in `<addr>:<port>` form (Stage
        /// 13k). Default: Reth's `RpcServerArgs::default()` which is
        /// `127.0.0.1:8545`. Examples:
        ///   `0.0.0.0:8545` (listen on all interfaces)
        ///   `127.0.0.1:0` (let the OS pick a free port)
        ///
        /// Use IPv6 by wrapping the address in brackets:
        /// `[::1]:8545`.
        #[arg(long)]
        rpc_bind: Option<String>,
    },
}

/// Stage 24c — per-step lending subcommands. Operate on a file-based
/// state sandbox so each command's mutation persists.
#[derive(Debug, Subcommand)]
enum LendingCommand {
    /// Initialize the lending state file with an empty USDC/ETH market
    /// and no positions. Overwrites any existing state at the path.
    Init {
        /// Override path. Default: `$HOME/.princeps/lending-state.json`.
        #[arg(long)]
        state_file: Option<PathBuf>,
    },
    /// Deposit collateral on behalf of an account.
    Deposit {
        account: u64,
        amount: u128,
        #[arg(long)]
        state_file: Option<PathBuf>,
    },
    /// Borrow underlying against an account's collateral (portfolio-gated).
    Borrow {
        account: u64,
        amount: u128,
        /// ETH price (USDC per ETH) used for the post-borrow health check.
        #[arg(long, default_value_t = 1)]
        eth_price: u64,
        #[arg(long)]
        state_file: Option<PathBuf>,
    },
    /// Repay debt for an account. Caps at outstanding debt.
    Repay {
        account: u64,
        amount: u128,
        #[arg(long)]
        state_file: Option<PathBuf>,
    },
    /// Withdraw collateral (portfolio-gated post-withdraw health check).
    Withdraw {
        account: u64,
        amount: u128,
        #[arg(long, default_value_t = 1)]
        eth_price: u64,
        #[arg(long)]
        state_file: Option<PathBuf>,
    },
    /// Show health metrics for an account at the given ETH price.
    Health {
        account: u64,
        #[arg(long, default_value_t = 1)]
        eth_price: u64,
        #[arg(long)]
        state_file: Option<PathBuf>,
    },
    /// Scan all accounts at the given ETH price, list any underwater.
    Scan {
        #[arg(long, default_value_t = 1)]
        eth_price: u64,
        #[arg(long)]
        state_file: Option<PathBuf>,
    },
    /// List all open positions.
    List {
        #[arg(long)]
        state_file: Option<PathBuf>,
    },
}

/// On-disk shape of `--validators <path>`. Stage 13j.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ValidatorSetFile {
    validators: Vec<ValidatorEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ValidatorEntry {
    /// Hex-encoded 32-byte Ed25519 public key (no `0x` prefix, no
    /// length-byte; just 64 hex chars).
    pubkey_hex: String,
    /// Voting power for this validator. Must be > 0.
    voting_power: u64,
    /// libp2p multiaddr where peers can reach this validator
    /// (Stage 13k). Optional; when present it's logged so operators
    /// can sanity-check the network layout. Full vote relay wiring
    /// remains a follow-up — the consensus engine doesn't yet
    /// consume per-peer multiaddrs from the validator set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    peer_multiaddr: Option<String>,
}

fn main() -> eyre::Result<()> {
    // Initialise tracing so Malachite/libp2p events surface. Default
    // filter `info,libp2p=warn` keeps multi-validator bring-up logs
    // readable; override via `RUST_LOG` for deeper investigation.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,libp2p=warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .try_init()
        .ok();

    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Info) {
        Command::Info => {
            print_info();
            Ok(())
        }
        Command::Devnet { rounds } => tokio_rt()?.block_on(run_devnet(rounds)),
        Command::LendingDemo { eth_crash_price } => run_lending_demo(eth_crash_price),
        Command::Lending { action } => run_lending_subcommand(action),
        Command::RethDevnet {
            rounds,
            moniker,
            data_dir,
            chain_spec,
            validators,
            listen_addr,
            rpc_bind,
        } => tokio_rt()?.block_on(run_reth_devnet(
            rounds,
            moniker,
            data_dir,
            chain_spec,
            validators,
            listen_addr,
            rpc_bind,
        )),
    }
}

fn tokio_rt() -> eyre::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(Into::into)
}

/// Resolve the effective `--data-dir` path. If the user passed one
/// explicitly we use it as-is; otherwise we default to
/// `$HOME/.princeps/data`. Errors if neither is available (no HOME).
/// Stage 24b: standalone cross-margin demo. Walks through Alice's
/// canonical scenario against an in-memory `LiveRethEvmBridge` and
/// proves the prime broker thesis: a position that would be liquidated
/// under siloed margin checks stays open under unified portfolio margin.
///
/// Mirrors the `cross_margin_demo_scenario_e2e` test in
/// `crates/evm/src/live_node.rs` but in binary form for any reader who
/// clones the repo and runs `cargo run --bin princeps -- lending-demo`.
fn run_lending_demo(eth_crash_price: u128) -> eyre::Result<()> {
    use rdk_clearing::Account;
    use rdk_clob::AccountId;
    use princeps_lending::{AssetId, Bps, Index as LendingIndex, IrmParams, Market, MarketId};
    use std::collections::BTreeMap;

    println!();
    println!("=== Princeps — cross-margin demo (Stage 24b) ===");
    println!();
    println!("    Walking Alice through the canonical prime-broker scenario:");
    println!("    deposit USDC → borrow ETH → open perp → market crash → check health.");
    println!();
    println!("    Run with --nocapture in tests, or `cargo run --bin princeps -- lending-demo`.");
    println!();

    let chain_spec = dev_chain_spec();
    let bridge = LiveRethEvmBridge::new((), chain_spec);

    // Register a single USDC/ETH lending market with the standard v0 params.
    bridge.with_markets_mut(|m| {
        let mut market = Market::new(
            MarketId(0),
            AssetId(1), // ETH = underlying
            AssetId(0), // USDC = collateral
            IrmParams {
                base_rate_per_block: 0,
                slope_below_kink_per_block: LendingIndex::RAY / 10_000,
                slope_above_kink_per_block: LendingIndex::RAY / 1_000,
                kink_bps: Bps(8_000),
            },
            Bps(9_500), // LT 95%
            Bps(500),   // liquidation bonus 5%
            Bps(1_000), // reserve factor 10%
            0,
        );
        market.total_supplied = 1_000_000;
        m.insert(MarketId(0), market);
    });

    let alice = AccountId(42);

    // Step 1: lending collateral
    bridge
        .lending_deposit_collateral(alice, MarketId(0), 1_000)
        .map_err(|e| eyre::eyre!("step 1 (deposit) failed: {e}"))?;
    println!("[Step 1] Alice deposits 1000 USDC as lending collateral.");

    // Step 2: borrow 5 ETH at entry price 100
    bridge
        .lending_borrow(alice, MarketId(0), 5, 1, 100)
        .map_err(|e| eyre::eyre!("step 2 (borrow) failed: {e}"))?;
    println!("[Step 2] Alice borrows 5 ETH at ETH=100 USDC (debt value = 500 USDC).");

    // Step 3: open perp position
    bridge.with_accounts_mut(|map| {
        let mut a = Account::flat(alice);
        a.position_size = PositionSize(10);
        a.avg_entry = MarkPrice(100);
        a.collateral = Notional(50);
        map.insert(alice, a);
    });
    println!("[Step 3] Alice opens long perp: 10 contracts ETH @ entry 100, posts 50 USDC.");

    // Step 4: market crash
    println!();
    println!(
        "[Step 4] Market shock: ETH price drops from 100 → {eth_crash_price}."
    );
    println!();

    // Build prices map for the crash state
    let mut crash_prices: BTreeMap<MarketId, (u128, u128)> = BTreeMap::new();
    crash_prices.insert(MarketId(0), (1, eth_crash_price));
    let empty_prices: BTreeMap<MarketId, (u128, u128)> = BTreeMap::new();
    let crash_mark = MarkPrice(u64::try_from(eth_crash_price).unwrap_or(u64::MAX));

    let perp_only_free =
        bridge.account_free_equity(alice, crash_mark, 1_000, &empty_prices);
    let unified_free = bridge.account_free_equity(alice, crash_mark, 1_000, &crash_prices);

    println!("            View                       Free equity       Verdict");
    println!("            ─────────────────────────  ───────────       ──────────────");
    println!(
        "            Siloed (perp only)         {:>11}       {}",
        perp_only_free,
        if perp_only_free < 0 {
            "LIQUIDATABLE"
        } else {
            "healthy"
        }
    );
    println!(
        "            Unified (perp + lending)   {:>11}       {}",
        unified_free,
        if unified_free < 0 {
            "liquidatable"
        } else {
            "HEALTHY"
        }
    );
    println!();
    println!("=> Same account that gets liquidated under Aave + dYdX silos");
    println!("   stays open under Princeps's unified portfolio margin.");
    println!("=> This is the prime broker thesis in action.");
    println!();

    if perp_only_free >= 0 {
        eprintln!(
            "warning: at --eth-crash-price={eth_crash_price}, perp is NOT siloed-liquidatable. \
             Default 90 produces the canonical demo result."
        );
    }
    if unified_free < 0 {
        eprintln!(
            "warning: at --eth-crash-price={eth_crash_price}, unified portfolio is ALSO liquidatable. \
             Try a smaller crash."
        );
    }

    Ok(())
}

// ============================================================
// Stage 24c — file-based lending sandbox CLI
// ============================================================

/// On-disk shape of the lending state file. v0 single-market layout.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct LendingState {
    market: princeps_lending::Market,
    positions: Vec<(
        rdk_clob::AccountId,
        princeps_lending::MarketId,
        princeps_lending::Position,
    )>,
}

fn resolve_lending_state_path(user: Option<&PathBuf>) -> eyre::Result<PathBuf> {
    if let Some(p) = user {
        return Ok(p.clone());
    }
    let home = std::env::var("HOME").map_err(|_| {
        eyre::eyre!("--state-file not supplied and $HOME is not set")
    })?;
    Ok(PathBuf::from(home)
        .join(".princeps")
        .join("lending-state.json"))
}

fn make_default_market() -> princeps_lending::Market {
    use princeps_lending::{AssetId, Bps, Index as LendingIndex, IrmParams, Market, MarketId};
    let mut market = Market::new(
        MarketId(0),
        AssetId(1), // ETH underlying
        AssetId(0), // USDC collateral
        IrmParams {
            base_rate_per_block: 0,
            slope_below_kink_per_block: LendingIndex::RAY / 10_000,
            slope_above_kink_per_block: LendingIndex::RAY / 1_000,
            kink_bps: Bps(8_000),
        },
        Bps(9_500), // LT 95%
        Bps(500),
        Bps(1_000),
        0,
    );
    market.total_supplied = 1_000_000;
    market
}

fn load_lending_state(path: &Path) -> eyre::Result<LendingState> {
    let bytes = std::fs::read(path)
        .map_err(|e| eyre::eyre!("read {}: {}", path.display(), e))?;
    let state: LendingState = serde_json::from_slice(&bytes)
        .map_err(|e| eyre::eyre!("parse {}: {}", path.display(), e))?;
    Ok(state)
}

fn save_lending_state(path: &Path, state: &LendingState) -> eyre::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(state)?;
    std::fs::write(path, json)?;
    Ok(())
}

fn bridge_from_state(state: &LendingState) -> LiveRethEvmBridge<()> {
    let bridge = LiveRethEvmBridge::new((), dev_chain_spec());
    bridge.with_markets_mut(|m| {
        m.insert(state.market.id, state.market.clone());
    });
    bridge.with_positions_mut(|p| {
        for (acc, mid, pos) in &state.positions {
            p.insert((*acc, *mid), pos.clone());
        }
    });
    bridge
}

fn state_from_bridge(bridge: &LiveRethEvmBridge<()>) -> LendingState {
    let markets = bridge.markets_snapshot();
    let market = markets
        .into_iter()
        .next()
        .map(|(_, m)| m)
        .unwrap_or_else(make_default_market);
    let positions = bridge
        .positions_snapshot()
        .into_iter()
        .map(|((acc, mid), pos)| (acc, mid, pos))
        .collect();
    LendingState { market, positions }
}

fn run_lending_subcommand(cmd: LendingCommand) -> eyre::Result<()> {
    use rdk_clob::AccountId;
    use princeps_lending::MarketId;
    use std::collections::BTreeMap;

    // Resolve state-file path from the command's --state-file flag (each
    // variant has its own, all named identically).
    let state_path = match &cmd {
        LendingCommand::Init { state_file }
        | LendingCommand::Deposit { state_file, .. }
        | LendingCommand::Borrow { state_file, .. }
        | LendingCommand::Repay { state_file, .. }
        | LendingCommand::Withdraw { state_file, .. }
        | LendingCommand::Health { state_file, .. }
        | LendingCommand::Scan { state_file, .. }
        | LendingCommand::List { state_file } => resolve_lending_state_path(state_file.as_ref())?,
    };

    // `init` writes fresh state and returns.
    if let LendingCommand::Init { .. } = &cmd {
        let state = LendingState {
            market: make_default_market(),
            positions: Vec::new(),
        };
        save_lending_state(&state_path, &state)?;
        println!(
            "Initialized lending sandbox at {} (empty USDC/ETH market, no positions).",
            state_path.display()
        );
        return Ok(());
    }

    // All other commands need an existing state file.
    let state = load_lending_state(&state_path).map_err(|e| {
        eyre::eyre!(
            "{e}\n  hint: run `princeps lending init` first to create the sandbox."
        )
    })?;
    let bridge = bridge_from_state(&state);

    // Run the operation; for write ops we save back, for read ops we don't.
    let mut should_save = true;
    match cmd {
        LendingCommand::Init { .. } => unreachable!(),

        LendingCommand::Deposit { account, amount, .. } => {
            bridge
                .lending_deposit_collateral(AccountId(account), MarketId(0), amount)
                .map_err(|e| eyre::eyre!("deposit failed: {e:?}"))?;
            print_account_state(&bridge, account, "deposit");
        }
        LendingCommand::Borrow {
            account, amount, eth_price, ..
        } => {
            let mut prices = BTreeMap::new();
            prices.insert(MarketId(0), (1u128, u128::from(eth_price)));
            bridge
                .lending_borrow_unified(
                    AccountId(account),
                    MarketId(0),
                    amount,
                    MarkPrice(0),
                    0,
                    &prices,
                )
                .map_err(|e| eyre::eyre!("borrow failed: {e:?}"))?;
            print_account_state(&bridge, account, "borrow");
        }
        LendingCommand::Repay { account, amount, .. } => {
            let repaid = bridge
                .lending_repay(AccountId(account), MarketId(0), amount)
                .map_err(|e| eyre::eyre!("repay failed: {e:?}"))?;
            println!(
                "Repaid {} units (requested {}). Account {} updated.",
                repaid, amount, account
            );
            print_account_state(&bridge, account, "repay");
        }
        LendingCommand::Withdraw {
            account, amount, eth_price, ..
        } => {
            let mut prices = BTreeMap::new();
            prices.insert(MarketId(0), (1u128, u128::from(eth_price)));
            bridge
                .lending_withdraw_collateral_unified(
                    AccountId(account),
                    MarketId(0),
                    amount,
                    MarkPrice(0),
                    0,
                    &prices,
                )
                .map_err(|e| eyre::eyre!("withdraw failed: {e:?}"))?;
            print_account_state(&bridge, account, "withdraw");
        }
        LendingCommand::Health { account, eth_price, .. } => {
            should_save = false;
            print_health(&bridge, account, eth_price);
        }
        LendingCommand::Scan { eth_price, .. } => {
            should_save = false;
            print_scan(&bridge, eth_price);
        }
        LendingCommand::List { .. } => {
            should_save = false;
            print_position_list(&bridge);
        }
    }

    if should_save {
        let updated = state_from_bridge(&bridge);
        save_lending_state(&state_path, &updated)?;
    }
    Ok(())
}

fn print_account_state(bridge: &LiveRethEvmBridge<()>, account: u64, action: &str) {
    use rdk_clob::AccountId;
    let pos = bridge
        .positions_snapshot()
        .into_iter()
        .find(|((a, _), _)| *a == AccountId(account))
        .map(|(_, p)| p);
    match pos {
        Some(p) => println!(
            "Account {} after {}: collateral={} scaled_debt={}",
            account, action, p.collateral_amount, p.scaled_debt
        ),
        None => println!("Account {} after {}: (no position)", account, action),
    }
}

fn print_health(bridge: &LiveRethEvmBridge<()>, account: u64, eth_price: u64) {
    use rdk_clob::AccountId;
    use princeps_lending::MarketId;
    use std::collections::BTreeMap;
    let mut prices = BTreeMap::new();
    prices.insert(MarketId(0), (1u128, u128::from(eth_price)));
    let mark = MarkPrice(0);
    let inputs =
        bridge.compute_account_portfolio_inputs(AccountId(account), mark, 0, &prices);
    let free = bridge.account_free_equity(AccountId(account), mark, 0, &prices);
    let healthy = bridge.account_is_healthy_portfolio(AccountId(account), mark, 0, &prices);

    println!("Account {} (at ETH price {}):", account, eth_price);
    println!(
        "  Lending collateral (LT-adjusted): {}",
        inputs.lending_adjusted_collateral_value
    );
    println!("  Lending debt:                     {}", inputs.lending_debt_value);
    println!("  Free equity:                      {}", free);
    println!(
        "  Verdict:                          {}",
        if healthy { "HEALTHY" } else { "LIQUIDATABLE" }
    );
}

fn print_scan(bridge: &LiveRethEvmBridge<()>, eth_price: u64) {
    use princeps_lending::MarketId;
    use std::collections::BTreeMap;
    let mut prices = BTreeMap::new();
    prices.insert(MarketId(0), (1u128, u128::from(eth_price)));
    let report = bridge.scan_unified(MarkPrice(0), 0, &prices);
    println!(
        "Scanned {} account(s) at ETH price {}; {} flagged.",
        report.scanned,
        eth_price,
        report.flagged.len()
    );
    if report.flagged.is_empty() {
        return;
    }
    println!();
    println!("Flagged (free_equity < 0):");
    for (acc, free) in report.flagged {
        println!("  Account {} → free_equity = {}", acc.0, free);
    }
}

fn print_position_list(bridge: &LiveRethEvmBridge<()>) {
    let positions = bridge.positions_snapshot();
    if positions.is_empty() {
        println!("(no positions)");
        return;
    }
    println!(
        "{:>10}  {:>10}  {:>12}  {:>12}",
        "Account", "Market", "Collateral", "ScaledDebt"
    );
    println!(
        "{:>10}  {:>10}  {:>12}  {:>12}",
        "-------", "------", "----------", "----------"
    );
    for ((acc, mid), pos) in positions {
        println!(
            "{:>10}  {:>10}  {:>12}  {:>12}",
            acc.0, mid.0, pos.collateral_amount, pos.scaled_debt
        );
    }
}

fn resolve_data_dir(user_supplied: Option<&PathBuf>) -> eyre::Result<PathBuf> {
    if let Some(p) = user_supplied {
        return Ok(p.clone());
    }
    let home = std::env::var("HOME")
        .map_err(|_| eyre::eyre!("--data-dir not supplied and $HOME is not set"))?;
    Ok(PathBuf::from(home).join(".princeps").join("data"))
}

fn print_info() {
    let config = PrincepsNodeConfig::hyperliquid_default();
    let node = PrincepsNode::new(config);

    println!(
        "princeps v{} (Hyperliquid-shape L1 reference)",
        env!("CARGO_PKG_VERSION")
    );
    println!("config:");
    println!(
        "  oracle refresh interval : {}s",
        config.oracle_refresh_interval_secs
    );
    println!(
        "  oracle staleness window : {}s",
        config.oracle_params.staleness_window_secs
    );
    println!(
        "  oracle min feeds        : {}",
        config.oracle_params.min_feeds_required
    );
    println!(
        "  initial margin          : {} bps",
        config.liquidation_params.initial_margin_bps
    );
    println!(
        "  maintenance margin      : {} bps",
        config.liquidation_params.maintenance_margin_bps
    );
    println!(
        "  liquidation fee         : {} bps",
        config.liquidation_params.liquidation_fee_bps
    );
    println!(
        "  vault min deposit       : {}",
        config.vault_params.min_deposit
    );
    println!(
        "  auto-ADL on deficit     : {}",
        config.run_adl_on_unfilled_deficit
    );
    println!("state:");
    println!("  oracle feeds            : {}", node.oracle().feed_count());
    println!(
        "  insurance fund balance  : {}",
        node.scanner().fund_balance()
    );
    println!(
        "  vault shares            : {}",
        node.vault().total_shares().0
    );
    println!(
        "  vault assets            : {}",
        node.vault().total_assets().0
    );
}

/// Drive `rounds` single-validator consensus rounds through an
/// **in-memory** EVM bridge, calling `PrincepsNode::tick` between each.
/// Stage 13b path — no Reth boot.
async fn run_devnet(rounds: u64) -> eyre::Result<()> {
    let mut coordinator = PrincepsNode::new(PrincepsNodeConfig::hyperliquid_default());
    let bridge = Arc::new(InMemoryEvmBridge::new());

    let mut parent = BlockHash([0u8; 32]);

    println!(
        "princeps v{} — driving {} single-validator devnet round{}",
        env!("CARGO_PKG_VERSION"),
        rounds,
        if rounds == 1 { "" } else { "s" }
    );

    for round in 0..rounds {
        let block_height = round + 1;
        let block_time = wallclock_secs().saturating_add(round);

        let decided = run_single_validator(bridge.as_ref(), parent).await?;
        println!(
            "round {}: decided {} via in-memory bridge",
            block_height,
            short_hash(&decided)
        );

        let report = coordinator.tick(TickInput {
            block_height,
            block_time,
            mark: MarkPrice(100),
            account_snapshots: &[],
            vault_total_assets: coordinator.vault().total_assets().0,
        });
        print_tick_report(&report);

        parent = decided;
    }

    Ok(())
}

/// Drive `rounds` consensus decisions through the **production-shape**
/// actor-engine loop with a Reth-backed [`LiveRethEvmBridge`].
/// Stage 13c path — the real boot ceremony.
///
/// Flow:
///   1. Spin up a Reth `EthereumNode` with `PrincepsExecutorBuilder`
///      (so the EVM has our custom CLOB precompiles registered).
///   2. Construct a [`LiveRethEvmBridge`] against the node's provider.
///   3. Bootstrap a consensus [`princeps_consensus::PrincepsNode`] with a
///      fresh Ed25519 keypair and a single-validator set.
///   4. `node.start().await` — spawns the Malachite actor system.
///   5. `take_channels().await` — get the engine's `AppMsg` channels.
///   6. Spawn `run_engine_app(bridge, channels, validator_set, rounds)`
///      to drive `rounds` decisions then exit.
///   7. Clean shutdown of the consensus node.
#[allow(clippy::too_many_lines)] // 6-step boot ceremony — flat for readability
#[allow(clippy::too_many_arguments)] // CLI surface — clap collects + forwards
async fn run_reth_devnet(
    rounds: u64,
    moniker: String,
    data_dir: Option<PathBuf>,
    chain_spec_path: Option<PathBuf>,
    validators_path: Option<PathBuf>,
    listen_addr: Option<String>,
    rpc_bind: Option<String>,
) -> eyre::Result<()> {
    println!(
        "princeps v{} — driving {} reth-backed decision{}",
        env!("CARGO_PKG_VERSION"),
        rounds,
        if rounds == 1 { "" } else { "s" }
    );

    // 1. Reth boot — production path (`init_db` + `with_database` +
    //    `with_launch_context`, no `test-utils` feature).
    let data_dir_path = resolve_data_dir(data_dir.as_ref())?;
    std::fs::create_dir_all(&data_dir_path)?;
    let reth_db_path = data_dir_path.join("reth");
    std::fs::create_dir_all(&reth_db_path)?;

    println!("[1/6] booting Reth EthereumNode with PrincepsExecutorBuilder…");
    println!("      data dir         = {}", data_dir_path.display());
    println!("      Reth MDBX dir    = {}", reth_db_path.display());

    let chain_spec = if let Some(path) = chain_spec_path.as_deref() {
        println!("      chain spec       = {} (loaded)", path.display());
        load_chain_spec(path)?
    } else {
        println!("      chain spec       = (embedded dev chain id 2600)");
        dev_chain_spec()
    };

    // Stage 13k: optional `--rpc-bind <addr:port>` overrides Reth's
    // default RPC bind (127.0.0.1:8545). Parse <ip>:<port>; supports
    // IPv4 (e.g. `0.0.0.0:8545`) and bracketed IPv6 (e.g. `[::1]:8545`).
    // `RpcServerArgs` exposes `http_addr`/`http_port` as public fields,
    // so we mutate the default rather than using a builder method.
    let mut rpc_args = reth_node_core::args::RpcServerArgs::default();
    if let Some(spec) = rpc_bind.as_deref() {
        let (ip, port) = parse_socket_spec(spec)?;
        println!("      rpc bind         = {ip}:{port}");
        rpc_args.http_addr = ip;
        rpc_args.http_port = port;
        // Stage 13l/13n: overriding `--rpc-bind` is the signal that this
        // process shares a host with other princeps nodes. Reth's WS
        // (8546) and auth-RPC (8551) defaults would collide between
        // peers, so bind both to ephemeral ports (port 0 — OS picks).
        // The IPC endpoint at `/tmp/reth.ipc` is a single global path
        // shared across processes — disable it entirely to avoid the
        // collision (we don't use IPC yet anyway).
        // Operators who need stable WS/auth ports or IPC can switch
        // to explicit flags later.
        rpc_args.ws_addr = ip;
        rpc_args.ws_port = 0;
        rpc_args.auth_addr = ip;
        rpc_args.auth_port = 0;
        rpc_args.ipcdisable = true;
        println!("      ws / auth bind   = {ip}:ephemeral (multi-node-safe)");
        println!("      ipc              = disabled (multi-node-safe)");
    } else {
        println!("      rpc bind         = (Reth default 127.0.0.1:8545)");
    }
    let node_config = NodeConfig::test()
        .dev()
        .with_chain(chain_spec.clone())
        .with_datadir_args(DatadirArgs {
            datadir: MaybePlatformPath::<DataDirPath>::from(reth_db_path.clone()),
            ..Default::default()
        })
        .with_rpc(rpc_args);
    let runtime = Runtime::test();

    // `init_db` opens an existing MDBX database at the path or creates
    // a fresh one if none exists — idempotent across restarts.
    let db = Arc::new(init_db(&reth_db_path, DatabaseArguments::default())?);

    let RethNodeHandle {
        node,
        node_exit_future: _,
    } = NodeBuilder::new(node_config)
        .with_database(db)
        .with_launch_context(runtime)
        .with_types::<EthereumNode>()
        .with_components(EthereumNode::components().executor(PrincepsExecutorBuilder::default()))
        .with_add_ons(EthereumAddOns::default())
        .launch()
        .await?;
    println!(
        "      Reth up; chain id = {}",
        node.chain_spec().chain.id()
    );

    // 2. LiveRethEvmBridge against the live node's provider.
    println!("[2/6] constructing LiveRethEvmBridge against node provider…");
    // Capture the genesis hash *before* moving chain_spec into the bridge —
    // run_engine_app needs it as the initial parent of its first decision
    // (Stage 13d gap closure).
    let genesis_hash_bytes: [u8; 32] = chain_spec.genesis_hash().into();
    let genesis_parent = BlockHash(genesis_hash_bytes);
    let bridge = Arc::new(LiveRethEvmBridge::new(node.provider.clone(), chain_spec));
    println!(
        "      genesis hash     = 0x{}…{}",
        hex_prefix(&genesis_hash_bytes, 4),
        hex_suffix(&genesis_hash_bytes, 4),
    );

    // Stage 13g+13i: load any prior bridge state and derive both the
    // initial parent hash AND the initial consensus height.
    let bridge_state_path = data_dir_path.join("bridge").join("state.json");
    let (resume_parent, prior_decisions) = if bridge_state_path.exists() {
        let bytes = std::fs::read(&bridge_state_path)?;
        let snapshot: BridgeSnapshot = serde_json::from_slice(&bytes)
            .map_err(|e| eyre::eyre!("malformed bridge snapshot at {bridge_state_path:?}: {e}"))?;
        let head_for_print = snapshot.head;
        let chain_len = snapshot.chain.len();
        bridge.load_snapshot(snapshot);
        println!(
            "      loaded snapshot  = {} block(s); head = {}",
            chain_len,
            head_for_print.map_or_else(|| "(none)".to_string(), |h| short_b256(&h)),
        );
        (
            head_for_print.map(|b| BlockHash(b.into())),
            u64::try_from(chain_len).unwrap_or(u64::MAX),
        )
    } else {
        println!("      no prior snapshot (fresh chain)");
        (None, 0)
    };

    // Stage 24a — on a fresh chain, register the v0 USDC/ETH lending
    // market and seed 5 demo accounts borrowing ETH at varying
    // leverage. On restart, markets + positions are restored from the
    // bridge snapshot so we deliberately skip this to avoid
    // double-seeding (which would panic on the second `lending_borrow`
    // against the same account).
    if resume_parent.is_none() {
        seed_v0_lending_markets(bridge.as_ref());
        seed_v0_demo_accounts(bridge.as_ref());
        println!("      lending genesis  = USDC/ETH market + 5 demo accounts seeded");
    }

    let initial_parent_for_consensus = resume_parent.unwrap_or(genesis_parent);
    // Stage 13i: consensus height = prior decisions + 1, so log lines
    // and (future) multi-validator peers see a continuous height
    // sequence rather than restarting at 1 every run.
    let initial_height_for_consensus =
        princeps_consensus::types::PrincepsHeight(prior_decisions.saturating_add(1));

    // 3. Consensus node with single-validator set.
    //    Stage 13h: load the validator key from disk if present,
    //    otherwise generate fresh and write it. With this in place
    //    consecutive runs use the same validator identity, which is
    //    a prerequisite for Malachite WAL reuse (Stage 13h+).
    let key_path = data_dir_path.join("validator-key.json");
    let (private, key_status) = if key_path.exists() {
        let bytes = std::fs::read(&key_path)?;
        let file: PrincepsPrivateKeyFile = serde_json::from_slice(&bytes)
            .map_err(|e| eyre::eyre!("malformed validator key at {key_path:?}: {e}"))?;
        (file.into_private_key(), "loaded")
    } else {
        let fresh = PrivateKey::generate(OsRng);
        let file = PrincepsPrivateKeyFile::from_private_key(&fresh);
        if let Some(parent) = key_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&key_path, serde_json::to_vec_pretty(&file)?)?;
        // Make the key file owner-readable only — minor hardening so a
        // shared-filesystem mishap doesn't surface the validator's
        // secret to other users on the host.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))?;
        }
        (fresh, "generated")
    };
    let public = private.public_key();
    println!("[3/6] {key_status} validator key from {}", key_path.display());

    // Write a `validator-pubkey.hex` sidecar so scripts / operators can
    // read the pubkey without parsing logs or reimplementing Ed25519
    // scalar multiplication. Idempotent — overwrites every boot so a
    // stale sidecar from a different key file can't drift.
    let pubkey_path = data_dir_path.join("validator-pubkey.hex");
    let pubkey_hex = hex::encode(public.as_bytes());
    std::fs::write(&pubkey_path, format!("{pubkey_hex}\n"))?;

    // Stage 13j: validator set — load from file if given, else
    // construct single-validator set from the loaded key (preserves
    // pre-13j behavior).
    // Stage 13l: also derive the libp2p dial list — every peer entry
    // with a `peer_multiaddr` that isn't *us*.
    let (validator_set, persistent_peers) = if let Some(path) = validators_path.as_deref() {
        let LoadedValidatorSet {
            set,
            peer_multiaddrs,
            self_index,
        } = load_validator_set(path, &public)?;
        println!(
            "      validator set    = {} ({} validator{})",
            path.display(),
            set.validators().len(),
            if set.validators().len() == 1 { "" } else { "s" }
        );
        // Log advertised peer multiaddrs (Stage 13k) so operators can
        // sanity-check the network layout.
        for (idx, addr) in peer_multiaddrs.iter().enumerate() {
            let marker = if idx == self_index { " (self)" } else { "" };
            match addr {
                Some(a) => println!("        peer[{idx}].multiaddr = {a}{marker}"),
                None => println!("        peer[{idx}].multiaddr = (unset){marker}"),
            }
        }
        // Stage 13l: build the dial list — every non-self entry that
        // has a multiaddr set. Self is excluded to avoid a libp2p
        // self-dial; entries without a multiaddr are skipped (they're
        // valid validators we just can't reach until they advertise).
        let dial_list: Vec<String> = peer_multiaddrs
            .iter()
            .enumerate()
            .filter_map(|(idx, addr)| {
                if idx == self_index {
                    None
                } else {
                    addr.clone()
                }
            })
            .collect();
        (set, dial_list)
    } else {
        let digest = Sha256::digest(public.as_bytes());
        let mut addr_bytes = [0u8; 20];
        addr_bytes.copy_from_slice(&digest[12..32]);
        let address = princeps_consensus::types::PrincepsAddress(addr_bytes);
        println!("      validator set    = (single-validator default)");
        let set = princeps_consensus::types::PrincepsValidatorSet::new(vec![
            princeps_consensus::types::PrincepsValidator::new(address, public, 1),
        ]);
        (set, Vec::new())
    };
    // Consensus home dir: a subdir of the resolved data dir. Persists
    // across restarts so the Malachite WAL has a stable location (real
    // WAL load/save remains Stage 13g work).
    let consensus_home = data_dir_path.join("consensus");
    std::fs::create_dir_all(&consensus_home)?;
    println!("      consensus home   = {}", consensus_home.display());
    let mut consensus_node = princeps_consensus::PrincepsNode::new(
        private,
        validator_set.clone(),
        consensus_home,
        moniker.clone(),
    );
    if let Some(ref multiaddr) = listen_addr {
        consensus_node = consensus_node.with_listen_addr(multiaddr.clone());
        println!("      listen addr      = {multiaddr}");
    } else {
        println!("      listen addr      = (ephemeral /ip4/127.0.0.1/tcp/0)");
    }
    // Stage 13l: forward the derived dial list. Empty (single-validator
    // path, or no peer_multiaddrs in the validator file) preserves
    // pre-13l behavior.
    if persistent_peers.is_empty() {
        println!("      persistent peers = (none)");
    } else {
        println!("      persistent peers = {} peer(s)", persistent_peers.len());
        for (idx, peer) in persistent_peers.iter().enumerate() {
            println!("        dial[{idx}]            = {peer}");
        }
        consensus_node = consensus_node.with_persistent_peers(persistent_peers.clone());
    }
    println!("      moniker          = {moniker}");

    // 4. Start the Malachite actor system.
    println!("[4/6] starting Malachite actor system…");
    let handle = consensus_node.start().await?;

    // 5. Take the engine's AppMsg channels.
    println!("[5/6] taking engine AppMsg channels…");
    let channels = handle
        .take_channels()
        .await
        .ok_or_else(|| eyre::eyre!("channels already taken"))?;

    // 6. Drive run_engine_app for N decisions, seeded with Reth's
    //    actual genesis hash so the first `build_payload` finds its
    //    parent block in the database.
    println!(
        "[6/6] driving run_engine_app for {rounds} decision(s) starting at height {}…",
        initial_height_for_consensus.0
    );
    let bridge_for_engine = bridge.clone();
    let validator_set_for_engine = validator_set.clone();
    let rounds_usize = usize::try_from(rounds)
        .map_err(|_| eyre::eyre!("rounds value too large for usize on this target"))?;

    // Stage 14a: integration coordinator. One `PrincepsNode` per
    // running validator. Every committed block triggers a `tick`.
    // Stage 14e: if a prior run persisted a coordinator snapshot,
    // load it so the insurance fund balance, vault, and oracle
    // refresh marker carry across restart. Mirrors the bridge
    // snapshot pattern (Stage 13g).
    // Stage 15a: dev override — shrink the funding interval from
    // Hyperliquid's 1 hour to 1 second so the clock fires per block
    // in a 3-round test. Production deployments leave it at 3600.
    let mut node_config = PrincepsNodeConfig::hyperliquid_default();
    node_config.funding_params.interval_secs = 1;
    let mut coordinator_inner = PrincepsNode::new(node_config);
    let coordinator_state_path = data_dir_path.join("coordinator").join("state.json");
    if coordinator_state_path.exists() {
        let bytes = std::fs::read(&coordinator_state_path)?;
        let snap: CoordinatorSnapshot = serde_json::from_slice(&bytes).map_err(|e| {
            eyre::eyre!("malformed coordinator snapshot at {coordinator_state_path:?}: {e}")
        })?;
        coordinator_inner.load_snapshot(snap);
        println!(
            "      loaded coordinator snapshot: fund={}, vault_shares={}, vault_assets={}, last_oracle_refresh_at={:?}",
            snap.insurance_fund_balance,
            snap.vault_total_shares,
            snap.vault_total_assets,
            snap.last_oracle_refresh_at,
        );
    } else {
        println!("      no prior coordinator snapshot (fresh state)");
    }
    let coordinator = Arc::new(Mutex::new(coordinator_inner));

    // Stage 14b: register synthetic publishers and seed the oracle
    // so the per-block refresh has feeds to aggregate. In production
    // these come from external CEX publishers; here we generate them
    // in-process with deterministic seeds (same code on every
    // validator → identical signed bytes → matching aggregation).
    let publishers: Vec<SyntheticPublisher> = SYNTHETIC_FEEDS
        .iter()
        .map(|(feed_id, seed, _)| SyntheticPublisher::from_seed(*feed_id, *seed))
        .collect();
    {
        let mut node = coordinator.lock().expect("coordinator mutex poisoned");
        for pub_ in &publishers {
            node.register_publisher(pub_.feed, pub_.public_key);
        }
    }
    println!(
        "      oracle publishers = {} synthetic feed(s) registered",
        publishers.len()
    );
    let publishers = Arc::new(publishers);

    // Stage 17h: seed five accounts via real CLOB fills, all
    // trading at the same fair price (100). Replaces the Stage 17a
    // single-MM seed (account 999 taking absurd off-market orders).
    // The cascade-inducing PnL now comes from the mark drift in
    // `seed_mark_orders` — exactly how real markets generate
    // winners and losers.
    //
    // Cast: Alice (10), Bob (20), Carol (30) are demo traders going
    // long; Dave (40), Eve (50) are makers taking the other side.
    // After the seed sequence, the mark book opens at midpoint 96
    // (Buy@95 / Sell@97), and the deposit phase funds collateral
    // (200, 50, 100, 300, 200) such that on the first tick:
    //   - Bob is Liquidatable (scan target),
    //   - Carol is Underwater (ADL target),
    //   - Dave + Eve are ADL-eligible counterparties (Safe + positive uPnL),
    //   - Alice rides through Safe.
    //
    // Scan and ADL never share an account in a single tick — the
    // disjoint-target invariant the `apply records` logic below
    // relies on is preserved across the retire-the-MM rewrite.
    let accounts_already_loaded = !bridge.accounts_snapshot().is_empty();
    if accounts_already_loaded {
        println!(
            "      accounts             = {} loaded from bridge snapshot",
            bridge.accounts_snapshot().len(),
        );
        // The mark-providing token orders are not part of the
        // bridge's persisted state (the CLOB book itself doesn't
        // snapshot today), so re-seed them on every boot.
        seed_mark_orders(&bridge);
        println!("      mark book            = re-seeded (Buy@95 / Sell@97)");
    } else {
        let fills_count = seed_accounts_via_fills(&bridge);
        println!(
            "      seed fills           = {} (Alice/Bob/Carol take longs from Dave + Eve @ price 100)",
            fills_count,
        );
        seed_mark_orders(&bridge);
        println!("      mark book            = Buy@95 / Sell@97 (mid 96 — 4-point drift)");
        // Stage 17b: deposit collateral via the bridge's deposit
        // primitive instead of mutating the account map directly.
        // This is the bridge-layer hook an EVM-side
        // `deposit(account, amount)` instruction would call once
        // we have a real on-chain collateral flow.
        for (id, coll) in [(10, 200), (20, 50), (30, 100), (40, 300), (50, 200)] {
            let new_balance = bridge.deposit(ClobAccountId(id), coll);
            println!(
                "      deposit              = account {id} → collateral {}",
                new_balance.0,
            );
        }
        println!(
            "      accounts             = {} (3 traders + 2 makers, no MM)",
            bridge.accounts_snapshot().len(),
        );
    }

    let coordinator_for_hook = coordinator.clone();
    let publishers_for_hook = publishers.clone();
    let bridge_for_hook = bridge.clone();
    let app_task = tokio::spawn(async move {
        run_engine_app(
            bridge_for_engine,
            channels,
            validator_set_for_engine,
            initial_parent_for_consensus,
            initial_height_for_consensus,
            rounds_usize,
            move |hash, height, block_time| {
                let mut node = coordinator_for_hook
                    .lock()
                    .map_err(|_| eyre::eyre!("coordinator mutex poisoned"))?;

                // Stage 14b: ingest one fresh signed observation per
                // synthetic publisher before the tick. Prices are the
                // hardcoded per-feed values from SYNTHETIC_FEEDS; the
                // timestamp is the same `block_time` the tick will
                // see, so the staleness window is irrelevant. Errors
                // are non-fatal (we log them and let `tick` decide
                // whether the resulting feed count is enough to
                // aggregate) — this matches the production pattern
                // where a bridge would never halt the chain on a
                // single feed's ingestion failure.
                for (publisher, &(_, _, price)) in
                    publishers_for_hook.iter().zip(SYNTHETIC_FEEDS.iter())
                {
                    let obs = publisher.sign(IndexPrice(price), block_time);
                    if let Err(e) = node.ingest_signed_observation(obs, block_time) {
                        tracing::warn!(
                            "stage 14b: ingest_signed_observation failed for feed {}: {e:?}",
                            publisher.feed.0,
                        );
                    }
                }

                let vault_total_assets = node.vault().total_assets().0;
                // Stage 14c: live CLOB mark from the bridge. Falls back
                // to MarkPrice(100) only when the book is one-sided or
                // empty (e.g., if every order has been crossed out).
                // The fallback keeps the tick running with a stable
                // value rather than failing on a transient empty book.
                let (mark, mark_source) = match bridge_for_hook.current_mark() {
                    Some(m) => (m, "clob"),
                    None => (MarkPrice(100), "stub-empty-book"),
                };

                // Stage 16c: read the bridge-owned accounts into a
                // tick-input slice. `Account` and `AccountSnapshot`
                // are structurally identical (same fields, same
                // types); the conversion is a field-by-field copy.
                let snapshots: Vec<AccountSnapshot> = bridge_for_hook
                    .accounts_snapshot()
                    .into_iter()
                    .map(|a| AccountSnapshot {
                        account: a.account,
                        position_size: a.position_size,
                        avg_entry: a.avg_entry,
                        collateral: a.collateral,
                    })
                    .collect();

                let report = node.tick(TickInput {
                    block_height: height.0,
                    block_time,
                    mark,
                    account_snapshots: &snapshots,
                    vault_total_assets,
                });
                println!("  mark = {} ({mark_source})", mark.0);
                print_tick_report(&report);

                // Per-block lending integration: accrue interest on every
                // registered market, scan portfolio-wide for underwater
                // accounts (perp + lending unified), and route any
                // shortfall into the coordinator's insurance fund.
                //
                // v0 reth-devnet has no lending markets configured at
                // genesis — Stage 24a's chain-spec will add USDC/ETH —
                // so the scan-and-absorb branches no-op for now. Wiring
                // them here closes the correctness gap that lending
                // interest never accrues in the running node (previously
                // `lending_tick` was only invoked from tests), and the
                // unified-scan loop will start firing the moment markets
                // exist without further integration changes.
                let lending_report = bridge_for_hook.lending_tick(height.0);
                if !lending_report.interest_reports.is_empty() {
                    println!(
                        "  lending tick: {} market(s) accrued interest at block {}",
                        lending_report.interest_reports.len(),
                        lending_report.block,
                    );
                }

                // Oracle circuit breaker (threat-model row O-3): if the
                // per-block deviation guard tripped on the perp oracle,
                // skip the unified scan + bad-debt absorption loop until
                // the halt window expires. Acting on a suspect price
                // would let an attacker trigger the very liquidations
                // they are trying to extract value from. Interest
                // accrual above remains safe (no price input).
                if let Some(until) = report.circuit_breaker_tripped_until {
                    println!(
                        "  oracle circuit breaker TRIPPED at block {}; halted through block {}",
                        height.0, until,
                    );
                }
                // Lending depletion guard (threat-model row L-5,
                // ADR-010 Layer 1): if the insurance fund's running
                // coverage of recent shortfalls dropped below
                // threshold, also skip the unified scan + bad-debt
                // absorption loop. Continuing would write more
                // bad-debt into a fund that can't cover it; the halt
                // window is the protocol's "stop digging" signal that
                // Layer 2 (operator-cap, ADR-008 agreement) and
                // Layer 3 (manually-declared socialization) are
                // expected to act in.
                if let Some(until) = report.lending_halt_tripped_until {
                    println!(
                        "  lending depletion guard ARMED at block {}; halted through block {}",
                        height.0, until,
                    );
                }
                if node.is_oracle_halted(height.0) {
                    if let Some(until) = node.oracle_halt_until() {
                        println!(
                            "  lending scan: skipped (oracle halt active, expires at block {})",
                            until,
                        );
                    }
                } else if node.is_lending_halted(height.0) {
                    if let Some(until) = node.lending_halt_until() {
                        println!(
                            "  lending scan: skipped (lending halt active, expires at block {})",
                            until,
                        );
                    }
                } else {
                    // Stage 24a — hardcoded prices matching the seed
                    // genesis (USDC=1, ETH=1). v1 will read these from the
                    // oracle's published lending feeds once the oracle
                    // type → lending price-pair bridge lands. With the seed
                    // accounts all healthy at (1, 1), the unified scan no-ops
                    // here; raising ETH via an external mutation (e.g., the
                    // lending CLI's --eth-price) makes accounts 2–5 flag.
                    let mut lending_prices: std::collections::BTreeMap<
                        princeps_lending::MarketId,
                        (u128, u128),
                    > = std::collections::BTreeMap::new();
                    lending_prices.insert(princeps_lending::MarketId(0), (1, 1));
                    let perp_im_bps = node.config().liquidation_params.initial_margin_bps;
                    let unified = bridge_for_hook.scan_unified(mark, perp_im_bps, &lending_prices);
                    for (account, free) in &unified.flagged {
                        let shortfall = bridge_for_hook
                            .absorb_account_bad_debt(*account, mark, &lending_prices);
                        if shortfall > 0 {
                            let outcome = node.absorb_lending_bad_debt(
                                i64::try_from(shortfall).unwrap_or(i64::MAX),
                            );
                            println!(
                                "  lending bad-debt: account {} free_equity={} shortfall={} fund_outcome={:?}",
                                account.0, free, shortfall, outcome,
                            );
                        }
                    }
                }

                // Stage 15b → 16c: apply funding settlements back to
                // the bridge-owned account map. The bridge is the
                // sole source of truth for per-account state — every
                // delta lands there.
                if let Some(ref ft) = report.funding {
                    bridge_for_hook.with_accounts_mut(|accts| {
                        for settlement in &ft.settlements {
                            if let Some(acct) = accts.get_mut(&settlement.account) {
                                let prev = acct.collateral.0;
                                let next = prev.saturating_add(settlement.delta.0);
                                acct.collateral = Notional(next);
                                println!(
                                    "  funding apply: account {} collateral {} → {} (Δ={})",
                                    acct.account.0, prev, next, settlement.delta.0,
                                );
                            }
                        }
                    });
                }

                // Stage 15d → 16c: liquidation + ADL records also
                // go through the bridge. Same disjoint-target
                // invariant: the synthetic seed is designed so
                // scan and ADL never target the same account from
                // one tick's snapshot.
                let has_liq = !report.liquidation.records.is_empty();
                let has_adl = report
                    .adl
                    .as_ref()
                    .is_some_and(|a| !a.records.is_empty());
                if has_liq || has_adl {
                    bridge_for_hook.with_accounts_mut(|accts| {
                        for rec in &report.liquidation.records {
                            if let Some(acct) = accts.get_mut(&rec.close_order.account) {
                                let prev_coll = acct.collateral.0;
                                match rec.outcome {
                                    CloseOutcomeKind::Solvent(sc) => {
                                        acct.position_size = PositionSize(0);
                                        acct.collateral = Notional(sc.residual_to_account);
                                        println!(
                                            "  liquidation apply: account {} closed (solvent) coll {} → {} (fee {} to fund)",
                                            acct.account.0,
                                            prev_coll,
                                            sc.residual_to_account,
                                            sc.fee_to_fund,
                                        );
                                    }
                                    CloseOutcomeKind::Underwater(uc) => {
                                        acct.position_size = PositionSize(0);
                                        acct.collateral = Notional(0);
                                        println!(
                                            "  liquidation apply: account {} closed (underwater) coll {} → 0 (fund covered shortfall {}, fee {})",
                                            acct.account.0,
                                            prev_coll,
                                            uc.shortfall_to_fund,
                                            uc.fee_to_fund,
                                        );
                                    }
                                }
                            }
                        }
                        if let Some(ref ar) = report.adl {
                            for rec in &ar.records {
                                if let Some(acct) = accts.get_mut(&rec.close_order.account) {
                                    let prev_size = acct.position_size.0;
                                    let prev_coll = acct.collateral.0;
                                    let qty = i64::try_from(rec.close_order.qty.0)
                                        .unwrap_or(i64::MAX);
                                    let new_size = match rec.close_order.side {
                                        Side::Sell => prev_size.saturating_sub(qty),
                                        Side::Buy => prev_size.saturating_add(qty),
                                    };
                                    acct.position_size = PositionSize(new_size);
                                    acct.collateral =
                                        Notional(prev_coll.saturating_add(rec.pnl_paid));
                                    println!(
                                        "  adl apply: account {} size {} → {} coll {} → {} (pnl_paid={}, haircut={})",
                                        acct.account.0,
                                        prev_size,
                                        new_size,
                                        prev_coll,
                                        prev_coll.saturating_add(rec.pnl_paid),
                                        rec.pnl_paid,
                                        rec.haircut,
                                    );
                                }
                            }
                        }
                    });
                }

                let _ = hash; // hash currently unused; future stages may want it
                Ok(())
            },
        )
        .await
    });

    #[allow(clippy::duration_suboptimal_units)]
    let timeout = std::time::Duration::from_secs(60);
    let app_result = tokio::time::timeout(timeout, app_task)
        .await
        .map_err(|_| eyre::eyre!("run_engine_app timed out after 60s"))?
        .map_err(|e| eyre::eyre!("run_engine_app task panicked: {e}"))?;

    match app_result {
        Ok(decisions) => {
            for (idx, hash) in decisions.iter().enumerate() {
                println!(
                    "decision {}: {} via reth-backed bridge",
                    idx + 1,
                    short_hash(hash)
                );
            }
            println!(
                "reth-devnet complete: {} decision(s) committed",
                decisions.len()
            );
        }
        Err(e) => {
            println!("run_engine_app halted with error: {e}");
        }
    }

    // Stage 13g: persist the bridge's final committed-chain state so
    // the next run can resume from it. Saved as JSON for easy
    // inspection (e.g., `jq < state.json '.head'`).
    let final_snapshot = bridge.snapshot();
    let chain_len = final_snapshot.chain.len();
    if let Some(parent) = bridge_state_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(
        &bridge_state_path,
        serde_json::to_vec_pretty(&final_snapshot)?,
    )?;
    println!(
        "persisted bridge snapshot ({} block(s)) → {}",
        chain_len,
        bridge_state_path.display()
    );

    // Stage 14e: persist the coordinator's load-bearing state alongside
    // the bridge snapshot so the next boot resumes the insurance fund,
    // vault, and oracle refresh marker.
    let coordinator_snap = coordinator
        .lock()
        .map_err(|_| eyre::eyre!("coordinator mutex poisoned"))?
        .snapshot();
    if let Some(parent) = coordinator_state_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(
        &coordinator_state_path,
        serde_json::to_vec_pretty(&coordinator_snap)?,
    )?;
    println!(
        "persisted coordinator snapshot (fund={}, vault_shares={}, vault_assets={}) → {}",
        coordinator_snap.insurance_fund_balance,
        coordinator_snap.vault_total_shares,
        coordinator_snap.vault_total_assets,
        coordinator_state_path.display()
    );

    // Stage 16c: per-account state is now persisted inside the
    // bridge snapshot above. The standalone `accounts/state.json`
    // file from 15c is no longer written — the bridge owns the
    // map.
    println!(
        "      (accounts now persisted inside bridge snapshot — see `.accounts`)"
    );

    // Clean shutdown regardless of the run_engine_app result above —
    // proves the teardown path works even when block production stops
    // short.
    println!("shutting down consensus actor system…");
    handle.kill(None).await?;
    println!("reth-devnet teardown complete");

    Ok(())
}

/// Stage 24a — register the v0 USDC/ETH lending market on a freshly
/// constructed bridge at boot. Parameters match the liquidator-bot
/// and lending-rpc-server demos: LT 95%, bonus 5%, reserve factor
/// 10%, kinked IRM at 80% utilization, seed supply of 1M USDC so a
/// borrow has somewhere to come from.
fn seed_v0_lending_markets<P>(bridge: &LiveRethEvmBridge<P>) {
    use princeps_lending::{AssetId, Bps, Index as LendingIndex, IrmParams, Market, MarketId};
    bridge.with_markets_mut(|m| {
        let mut market = Market::new(
            MarketId(0),
            AssetId(1), // ETH underlying (borrowed)
            AssetId(0), // USDC collateral
            IrmParams {
                base_rate_per_block: 0,
                slope_below_kink_per_block: LendingIndex::RAY / 10_000,
                slope_above_kink_per_block: LendingIndex::RAY / 1_000,
                kink_bps: Bps(8_000),
            },
            Bps(9_500), // LT 95%
            Bps(500),   // liquidation bonus 5%
            Bps(1_000), // reserve factor 10%
            0,
        );
        market.total_supplied = 1_000_000;
        m.insert(MarketId(0), market);
    });
}

/// Stage 24a — seed 5 demo accounts borrowing ETH at varying leverage
/// against the v0 USDC/ETH market. Mirrors the liquidator-bot fixture
/// so the same prime-broker scenario plays out on the running
/// reth-devnet without further setup. All accounts are healthy at the
/// seed price (USDC=1, ETH=1); pumping ETH price to 2 flags accounts
/// 2–5 — exactly what the per-block `scan_unified` will surface.
fn seed_v0_demo_accounts<P>(bridge: &LiveRethEvmBridge<P>) {
    use rdk_clearing::Account;
    use rdk_clob::AccountId as LendingAccountId;
    use princeps_lending::MarketId;
    let setups: [(u64, u128, u128); 5] = [
        (1, 1_000, 200), // very healthy
        (2, 500, 300),
        (3, 200, 150),
        (4, 100, 90),
        (5, 80, 70), // thinnest
    ];
    for &(acct_id, coll, debt) in &setups {
        let acct = LendingAccountId(acct_id);
        bridge
            .lending_deposit_collateral(acct, MarketId(0), coll)
            .expect("seed deposit");
        bridge
            .lending_borrow(acct, MarketId(0), debt, 1, 1)
            .expect("seed borrow");
        // Empty perp account so the unified scan walks this account.
        bridge.with_accounts_mut(|map| {
            let a = Account::flat(acct);
            map.insert(acct, a);
        });
    }
}

/// Load a `ChainSpec` from a JSON file containing an
/// `alloy_genesis::Genesis`. The file format is the same one the
/// embedded `dev_chain_spec` uses inline.
fn load_chain_spec(path: &Path) -> eyre::Result<Arc<ChainSpec>> {
    let bytes = std::fs::read(path)
        .map_err(|e| eyre::eyre!("failed to read chain spec at {}: {e}", path.display()))?;
    let genesis: Genesis = serde_json::from_slice(&bytes)
        .map_err(|e| eyre::eyre!("malformed chain spec at {}: {e}", path.display()))?;
    Ok(Arc::new(genesis.into()))
}

/// Load a validator set from a JSON file. The locally-loaded validator
/// key's public key MUST appear in the set — otherwise the node
/// refuses to sign on behalf of an identity the network doesn't
/// recognize.
/// Loaded validator-set result. `peer_multiaddrs[i]` is the
/// `peer_multiaddr` entry for validator `i` (parallel to
/// `set.validators()`); `self_index` is the position of *our*
/// validator in the set — used to filter our own entry out of the
/// libp2p dial list (Stage 13l).
struct LoadedValidatorSet {
    set: princeps_consensus::types::PrincepsValidatorSet,
    peer_multiaddrs: Vec<Option<String>>,
    self_index: usize,
}

fn load_validator_set(
    path: &Path,
    our_pubkey: &informalsystems_malachitebft_signing_ed25519::PublicKey,
) -> eyre::Result<LoadedValidatorSet> {
    let bytes = std::fs::read(path)
        .map_err(|e| eyre::eyre!("failed to read validator set at {}: {e}", path.display()))?;
    let file: ValidatorSetFile = serde_json::from_slice(&bytes)
        .map_err(|e| eyre::eyre!("malformed validator set at {}: {e}", path.display()))?;
    if file.validators.is_empty() {
        return Err(eyre::eyre!("validator set at {} is empty", path.display()));
    }
    let our_pubkey_bytes = our_pubkey.as_bytes();

    let mut self_index: Option<usize> = None;
    let mut built = Vec::with_capacity(file.validators.len());
    let mut peer_multiaddrs = Vec::with_capacity(file.validators.len());
    for (idx, entry) in file.validators.iter().enumerate() {
        if entry.voting_power == 0 {
            return Err(eyre::eyre!(
                "validator with pubkey_hex={} has voting_power=0; must be > 0",
                entry.pubkey_hex
            ));
        }
        let raw = hex::decode(&entry.pubkey_hex)
            .map_err(|e| eyre::eyre!("invalid hex in pubkey_hex={}: {e}", entry.pubkey_hex))?;
        let bytes: [u8; 32] = raw
            .try_into()
            .map_err(|v: Vec<u8>| eyre::eyre!("pubkey_hex must decode to 32 bytes, got {}", v.len()))?;
        // PublicKey::from_bytes panics on invalid Ed25519 points; go
        // through `VerificationKey::try_from` so malformed entries
        // surface as a graceful eyre error instead.
        let vk = ed25519_consensus::VerificationKey::try_from(bytes).map_err(|e| {
            eyre::eyre!(
                "pubkey_hex={} is not a valid Ed25519 public key: {e}",
                entry.pubkey_hex
            )
        })?;
        let pubkey = informalsystems_malachitebft_signing_ed25519::PublicKey::new(vk);
        if pubkey.as_bytes() == our_pubkey_bytes {
            if let Some(prior) = self_index {
                return Err(eyre::eyre!(
                    "validator set at {} lists our public key twice (positions {prior} and {idx})",
                    path.display()
                ));
            }
            self_index = Some(idx);
        }
        let digest = Sha256::digest(pubkey.as_bytes());
        let mut addr = [0u8; 20];
        addr.copy_from_slice(&digest[12..32]);
        built.push(princeps_consensus::types::PrincepsValidator::new(
            princeps_consensus::types::PrincepsAddress(addr),
            pubkey,
            entry.voting_power,
        ));
        peer_multiaddrs.push(entry.peer_multiaddr.clone());
    }
    let self_index = self_index.ok_or_else(|| {
        eyre::eyre!(
            "loaded validator key's public key does not appear in {}; \
             refusing to start (won't sign as an unrecognized identity)",
            path.display()
        )
    })?;
    Ok(LoadedValidatorSet {
        set: princeps_consensus::types::PrincepsValidatorSet::new(built),
        peer_multiaddrs,
        self_index,
    })
}

/// Parse an `<addr>:<port>` socket spec for `--rpc-bind`. Accepts:
///   `127.0.0.1:8545` (IPv4)
///   `0.0.0.0:8545`   (IPv4 all-interfaces)
///   `[::1]:8545`     (IPv6, brackets required to disambiguate `:` in addr)
fn parse_socket_spec(spec: &str) -> eyre::Result<(IpAddr, u16)> {
    let (addr_str, port_str) = if let Some(rest) = spec.strip_prefix('[') {
        // Bracketed IPv6: `[<v6>]:<port>`
        let (v6, after) = rest
            .split_once(']')
            .ok_or_else(|| eyre::eyre!("malformed IPv6 spec `{spec}`: missing closing `]`"))?;
        let port = after
            .strip_prefix(':')
            .ok_or_else(|| eyre::eyre!("malformed IPv6 spec `{spec}`: expected `:` after `]`"))?;
        (v6, port)
    } else {
        spec.rsplit_once(':')
            .ok_or_else(|| eyre::eyre!("malformed socket spec `{spec}`: expected `<addr>:<port>`"))?
    };
    let addr: IpAddr = addr_str
        .parse()
        .map_err(|e| eyre::eyre!("invalid IP `{addr_str}` in `{spec}`: {e}"))?;
    let port: u16 = port_str
        .parse()
        .map_err(|e| eyre::eyre!("invalid port `{port_str}` in `{spec}`: {e}"))?;
    Ok((addr, port))
}

/// Minimal post-merge dev genesis. Chain ID 2600 mirrors the upstream
/// reth custom-dev-node example so behaviour can be compared 1:1 if
/// needed. Same shape `crates/evm` uses in its integration tests.
fn dev_chain_spec() -> Arc<ChainSpec> {
    let genesis_json = r#"{
        "nonce": "0x42",
        "timestamp": "0x0",
        "extraData": "0x5343",
        "gasLimit": "0x5208",
        "difficulty": "0x400000000",
        "mixHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
        "coinbase": "0x0000000000000000000000000000000000000000",
        "alloc": {},
        "number": "0x0",
        "gasUsed": "0x0",
        "parentHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
        "config": {
            "ethash": {},
            "chainId": 2600,
            "homesteadBlock": 0,
            "eip150Block": 0,
            "eip155Block": 0,
            "eip158Block": 0,
            "byzantiumBlock": 0,
            "constantinopleBlock": 0,
            "petersburgBlock": 0,
            "istanbulBlock": 0,
            "berlinBlock": 0,
            "londonBlock": 0,
            "terminalTotalDifficulty": 0,
            "terminalTotalDifficultyPassed": true,
            "shanghaiTime": 0
        }
    }"#;
    let genesis: Genesis = serde_json::from_str(genesis_json).expect("dev genesis parses");
    Arc::new(genesis.into())
}

fn wallclock_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Stage 14b synthetic publisher. In real production, external
/// publishers (Binance/Coinbase/OKX) run their own signing services
/// and the bridge only ingests + verifies. For the v0 reference
/// devnet we generate observations in-process from deterministic
/// seeds so every validator computes identical signed bytes for the
/// same `(feed, price, timestamp)` tuple — that's the determinism the
/// oracle relies on.
///
/// The seed byte is repeated 32 times to form the secp256k1 secret
/// scalar; this is the same trick the oracle's `test_signing_key`
/// helper uses internally, lifted into the binary so the bridge-
/// simulator code stays out of the oracle crate's production
/// surface.
struct SyntheticPublisher {
    feed: FeedId,
    signing_key: SigningKey,
    public_key: PublisherKey,
}

impl SyntheticPublisher {
    fn from_seed(feed_id: u32, seed: u8) -> Self {
        assert!(seed != 0, "seed must be non-zero (scalar must be in 1..n)");
        let signing_key = SigningKey::from_slice(&[seed; 32])
            .expect("repeating seed forms a valid secp256k1 scalar");
        let compressed = signing_key.verifying_key().to_encoded_point(true);
        let mut bytes = [0u8; 33];
        bytes.copy_from_slice(compressed.as_bytes());
        Self {
            feed: FeedId(feed_id),
            signing_key,
            public_key: PublisherKey(bytes),
        }
    }

    fn sign(&self, price: IndexPrice, timestamp: u64) -> PriceObservation {
        let unsigned = PriceObservation::unsigned(self.feed, price, timestamp);
        let signed_bytes = unsigned.signed_bytes();
        let sig: k256::ecdsa::Signature = self.signing_key.sign(&signed_bytes);
        let mut sig_array = [0u8; 64];
        sig_array.copy_from_slice(&sig.to_bytes());
        PriceObservation {
            signature: OracleSignature(sig_array),
            ..unsigned
        }
    }
}

/// Spread of three publishers around a 102-cent anchor. Median is 102.
/// Trivial enough to verify visually in tick logs; rich enough that the
/// oracle's deviation filter is exercised (101/102/103 are all within
/// the 100-bps default deviation cap).
const SYNTHETIC_FEEDS: &[(u32, u8, u64)] = &[
    (1, 1, 101),
    (2, 2, 102),
    (3, 3, 103),
];

/// Stage 17h: five-account market scenario. Replaces the Stage 17a
/// single-MM seed (account 999 taking absurd off-market orders) with
/// a realistic shape: every trade in the seed sequence happens at
/// the same fair price (100), and the cascade-inducing PnL comes
/// from the mark moving in [`seed_mark_orders`] — exactly how real
/// markets generate winners and losers.
///
/// Sequence (deterministic across validators — both nodes execute
/// this identically on boot, every order at price 100):
///
///   1. Dave (40) Sell-limit 10 → Alice (10) Buy-market 10
///   2. Dave (40) Sell-limit 10 → Bob   (20) Buy-market 10
///   3. Dave (40) Sell-limit 30 → Carol (30) Buy-market 30
///   4. Eve  (50) Sell-limit 20 → Carol (30) Buy-market 20
///
/// Resulting positions (avg_entry = 100 for everyone):
///   - Alice (10): long 10  — safe trader with margin to spare
///   - Bob   (20): long 10  — thinly-collateralised, drops below
///                            maintenance once mark moves
///   - Carol (30): long 50  — large position, equity goes negative
///                            once mark moves (the underwater case)
///   - Dave  (40): short 50 — counterparty to rounds 1–3; ADL-
///                            eligible after mark drift gives him
///                            positive uPnL
///   - Eve   (50): short 20 — counterparty to round 4's tail;
///                            also ADL-eligible
///
/// At the post-boot mark (96, see [`seed_mark_orders`]) and the
/// post-boot collateral deposits (200, 50, 100, 300, 200) the
/// `MarginHealth` shakes out to:
///   - Alice: Safe          (MR ≈ 16.7%)
///   - Bob:   Liquidatable  (MR ≈ 1.0%, < 2% maintenance) — scan
///   - Carol: Underwater    (equity = −100)                — ADL
///   - Dave:  Safe          (MR ≈ 10.4%, +200 uPnL)        — ADL ctp
///   - Eve:   Safe          (MR ≈ 14.6%, +80  uPnL)        — ADL ctp
///
/// **Disjoint-target invariant.** Scan targets {Bob}, ADL targets
/// {Carol}, ADL counterparties are drawn from {Dave, Eve}. All
/// three sets are disjoint, so the per-tick `apply records` logic
/// in `main.rs` (which assumes scan and ADL don't double-touch one
/// account) keeps its precondition.
///
/// Returns the total number of fills produced.
#[allow(clippy::too_many_lines)]
fn seed_accounts_via_fills<P>(bridge: &LiveRethEvmBridge<P>) -> usize {
    let mut fills = 0;

    // Round 1 — Dave makes Sell 10 @ 100; Alice takes long 10.
    let r = bridge.submit_order(Order {
        id: OrderId(1001),
        account: ClobAccountId(40),
        side: Side::Sell,
        qty: Qty(10),
        order_type: OrderType::Limit { price: Price(100) },
    });
    fills += r.fills.len();
    let r = bridge.submit_order(Order {
        id: OrderId(1002),
        account: ClobAccountId(10),
        side: Side::Buy,
        qty: Qty(10),
        order_type: OrderType::Market,
    });
    fills += r.fills.len();

    // Round 2 — Dave makes another Sell 10 @ 100; Bob takes long 10.
    let r = bridge.submit_order(Order {
        id: OrderId(1003),
        account: ClobAccountId(40),
        side: Side::Sell,
        qty: Qty(10),
        order_type: OrderType::Limit { price: Price(100) },
    });
    fills += r.fills.len();
    let r = bridge.submit_order(Order {
        id: OrderId(1004),
        account: ClobAccountId(20),
        side: Side::Buy,
        qty: Qty(10),
        order_type: OrderType::Market,
    });
    fills += r.fills.len();

    // Round 3 — Dave makes Sell 30 @ 100; Carol takes long 30.
    // Dave is now short 50 total, all at avg 100.
    let r = bridge.submit_order(Order {
        id: OrderId(1005),
        account: ClobAccountId(40),
        side: Side::Sell,
        qty: Qty(30),
        order_type: OrderType::Limit { price: Price(100) },
    });
    fills += r.fills.len();
    let r = bridge.submit_order(Order {
        id: OrderId(1006),
        account: ClobAccountId(30),
        side: Side::Buy,
        qty: Qty(30),
        order_type: OrderType::Market,
    });
    fills += r.fills.len();

    // Round 4 — Eve makes Sell 20 @ 100; Carol tops up to long 50.
    let r = bridge.submit_order(Order {
        id: OrderId(1007),
        account: ClobAccountId(50),
        side: Side::Sell,
        qty: Qty(20),
        order_type: OrderType::Limit { price: Price(100) },
    });
    fills += r.fills.len();
    let r = bridge.submit_order(Order {
        id: OrderId(1008),
        account: ClobAccountId(30),
        side: Side::Buy,
        qty: Qty(20),
        order_type: OrderType::Market,
    });
    fills += r.fills.len();

    fills
}

/// Stage 17h: place two token resting orders so `current_mark()`
/// has a bid + ask to compute a midpoint. With trade-time price
/// 100 and these mark orders at 95/97 (mid 96), every position
/// opened by [`seed_accounts_via_fills`] picks up a 4-point uPnL
/// drift — what generates the cascade. The
/// `seed_accounts_via_fills` sequence exhausts all matched
/// liquidity, leaving the book empty, so neither of these orders
/// crosses anything.
fn seed_mark_orders<P>(bridge: &LiveRethEvmBridge<P>) {
    let _ = bridge.submit_order(Order {
        id: OrderId(2001),
        account: ClobAccountId(1),
        side: Side::Buy,
        qty: Qty(1),
        order_type: OrderType::Limit { price: Price(95) },
    });
    let _ = bridge.submit_order(Order {
        id: OrderId(2002),
        account: ClobAccountId(2),
        side: Side::Sell,
        qty: Qty(1),
        order_type: OrderType::Limit { price: Price(97) },
    });
}

fn short_hash(h: &BlockHash) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(10);
    for b in &h.0[..4] {
        let _ = write!(s, "{b:02x}");
    }
    s.push('…');
    s
}

fn hex_prefix(bytes: &[u8; 32], n: usize) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(n * 2);
    for b in &bytes[..n] {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn hex_suffix(bytes: &[u8; 32], n: usize) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(n * 2);
    for b in &bytes[32 - n..] {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn short_b256(h: &alloy_primitives::B256) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(10);
    for b in &h.0[..4] {
        let _ = write!(s, "{b:02x}");
    }
    s.push('…');
    s
}

fn print_tick_report(report: &TickReport) {
    print!(
        "  tick(height={}, time={}): ",
        report.block_height, report.block_time
    );
    match &report.oracle {
        Some(Ok(p)) => print!("oracle=Ok(idx={}, feeds={}) ", p.index.0, p.feeds_used),
        Some(Err(e)) => print!("oracle=Err({e:?}) "),
        None => print!("oracle=skip "),
    }
    print!(
        "scan(records={}, dep={}, wd={}, deficit={}) ",
        report.liquidation.records.len(),
        report.liquidation.fund_deposits,
        report.liquidation.fund_withdrawals,
        report.liquidation.unfilled_deficit
    );
    match &report.adl {
        Some(a) => print!(
            "adl(records={}, absorbed={}, remaining={}) ",
            a.records.len(),
            a.deficit_absorbed,
            a.deficit_remaining,
        ),
        None => print!("adl=skip "),
    }
    match &report.funding {
        Some(f) => print!(
            "funding(premium={}, rate={}, settlements={}) ",
            f.premium.0,
            f.rate.0,
            f.settlements.len(),
        ),
        None => print!("funding=skip "),
    }
    println!(
        "vault(shares={}, assets={}, price_bps={:?})",
        report.vault_total_shares, report.vault_total_assets, report.vault_share_price_bps
    );
}

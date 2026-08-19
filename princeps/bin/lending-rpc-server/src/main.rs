//! HTTP RPC + demo UI server for Princeps cross-protocol margin (Stage 24d).
//!
//! JSON endpoints over an in-process `LiveRethEvmBridge`, plus a single-page
//! UI that drives the real `princeps-portfolio` kernel from a browser. Mirrors
//! what a future production deployment would expose alongside Reth's own
//! JSON-RPC. v0 scope: in-process bridge with seeded demo data so any reader
//! can `curl localhost:8080/lending/markets` — or open `http://localhost:8080`
//! — after `cargo run --bin princeps-lending-rpc-server`, without booting a
//! validator.
//!
//! ### Endpoints
//!
//! - `GET /`
//!   → the cross-protocol margin demo UI (`ui/index.html`, read from disk at
//!     request time so the frontend iterates without a rebuild).
//!
//! - `GET /lending/markets`
//!   → `[{ market_id, market }]` for every registered market.
//!
//! - `GET /lending/positions`
//!   → `[{ account_id, market_id, position }]` for every open lending
//!     position, sorted lexicographically by `(account_id, market_id)`.
//!
//! - `GET /lending/health?account=N&perp_mark=M&perp_im_bps=B&coll_price=C&debt_price=D`
//!   → `{ account, free_equity, is_healthy, portfolio_inputs }` for a seeded
//!     account. Builds a single-market price map `{ MarketId(0) => (C, D) }`.
//!
//! - `GET /portfolio/health?perp_collateral=..&perp_unrealized_pnl=..&perp_im_req=..&lending_adjusted_collateral_value=..&lending_debt_value=..`
//!   → `{ free_equity, is_healthy, portfolio_inputs }` for an arbitrary,
//!     caller-supplied cross-protocol book. Stateless: nothing is seeded, the
//!     caller's numbers run straight through `princeps_portfolio::compute_free_equity`.
//!     This is what the UI calls, so a trader's own book — not demo data —
//!     drives the engine.
//!
//! - `GET /lending/scan?perp_mark=M&perp_im_bps=B&coll_price=C&debt_price=D`
//!   → `UnifiedScanReport` from `LiveRethEvmBridge::scan_unified`.
//!
//! ### Usage
//!
//! ```bash
//! cargo run --bin princeps-lending-rpc-server
//! open http://localhost:8080                       # the demo UI
//! curl http://localhost:8080/lending/markets
//! curl 'http://localhost:8080/portfolio/health?perp_collateral=1000&perp_unrealized_pnl=900&perp_im_req=100&lending_adjusted_collateral_value=950&lending_debt_value=1800'
//! ```
//!
//! ### Scope
//!
//! v0 in-process bridge. Real-world deployment serves data from a
//! running `reth-devnet`; v1 will replace this binary's local bridge
//! setup with a connection to a long-lived node via an internal IPC
//! handle. The endpoint shapes don't change. See `README.md` in this
//! crate for the UI's honesty model (real kernel, published venue
//! rulebooks, real historical price path, Princeps-can-lose).

use std::collections::BTreeMap;
use std::sync::Arc;

use alloy_genesis::Genesis;
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{Html, Json},
    routing::get,
    Router,
};
use clap::Parser;
use rdk_clearing::Account;
use rdk_clob::AccountId;
use princeps_evm::LiveRethEvmBridge;
use rdk_funding::{MarkPrice, Notional, PositionSize};
use princeps_lending::{AssetId, Bps, Index as LendingIndex, IrmParams, Market, MarketId, Position};
use princeps_portfolio::PortfolioInputs;
use reth_chainspec::ChainSpec;
use serde::{Deserialize, Serialize};

type Bridge = LiveRethEvmBridge<()>;

#[derive(Debug, Parser)]
#[command(
    name = "princeps-lending-rpc-server",
    version,
    about = "Read-only HTTP RPC for Princeps lending state (Stage 24d)"
)]
struct Args {
    /// TCP port to listen on.
    #[arg(long, default_value_t = 8080)]
    port: u16,

    /// Listen address (default loopback).
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let args = Args::parse();

    let bridge = setup_bridge();
    seed_accounts(&bridge);
    let state = Arc::new(bridge);

    let app = Router::new()
        .route("/", get(serve_ui))
        .route("/lending/markets", get(list_markets))
        .route("/lending/positions", get(list_positions))
        .route("/lending/health", get(get_health))
        .route("/lending/scan", get(get_scan))
        .route("/portfolio/health", get(get_portfolio_health))
        .with_state(state);

    let addr = format!("{}:{}", args.host, args.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    println!("Princeps lending RPC listening on http://{addr}");
    println!("Try:");
    println!("  curl http://{addr}/lending/markets");
    println!("  curl http://{addr}/lending/positions");
    println!(
        "  curl '{addr_proto}/lending/health?account=1&perp_mark=0&perp_im_bps=0&coll_price=1&debt_price=2'",
        addr_proto = format_args!("http://{addr}")
    );
    println!(
        "  curl '{addr_proto}/lending/scan?perp_mark=0&perp_im_bps=0&coll_price=1&debt_price=2'",
        addr_proto = format_args!("http://{addr}")
    );

    axum::serve(listener, app).await?;
    Ok(())
}

// ============================================================
// Response types
// ============================================================

#[derive(Serialize)]
struct MarketResponse {
    market_id: MarketId,
    market: Market,
}

#[derive(Serialize)]
struct PositionResponse {
    account_id: AccountId,
    market_id: MarketId,
    position: Position,
}

#[derive(Serialize)]
struct HealthResponse {
    account: AccountId,
    free_equity: i128,
    is_healthy: bool,
    portfolio_inputs: PortfolioInputs,
}

/// Response for an arbitrary (stateless) cross-protocol book run directly
/// through the real `princeps-portfolio` kernel — no seeded account.
#[derive(Serialize)]
struct PortfolioHealthResponse {
    free_equity: i128,
    is_healthy: bool,
    portfolio_inputs: PortfolioInputs,
}

// ============================================================
// Query parameter shapes
// ============================================================

// Note: serde_urlencoded (axum's query deserializer) doesn't support u128;
// we use u64 in the wire format and cast to u128 in handlers. For v0 prices
// (single-digit ETH multiples in USDC units) u64 is plenty; v1 RPC can move
// to JSON-body POSTs if larger values are ever needed.
#[derive(Debug, Deserialize)]
struct PricesQuery {
    perp_mark: u64,
    perp_im_bps: u32,
    coll_price: u64,
    debt_price: u64,
}

#[derive(Debug, Deserialize)]
struct HealthQuery {
    account: u64,
    perp_mark: u64,
    perp_im_bps: u32,
    coll_price: u64,
    debt_price: u64,
}

/// Raw `PortfolioInputs` fields for a caller-supplied cross-protocol book.
/// Signed i64 on the wire (serde_urlencoded rejects i128); cast to i128 in
/// the handler. i64 range covers any realistic demo book.
#[derive(Debug, Deserialize)]
struct PortfolioQuery {
    perp_collateral: i64,
    perp_unrealized_pnl: i64,
    perp_im_req: i64,
    lending_adjusted_collateral_value: i64,
    lending_debt_value: i64,
}

// ============================================================
// Handlers
// ============================================================

async fn list_markets(State(bridge): State<Arc<Bridge>>) -> Json<Vec<MarketResponse>> {
    let markets = bridge.markets_snapshot();
    let response: Vec<MarketResponse> = markets
        .into_iter()
        .map(|(market_id, market)| MarketResponse { market_id, market })
        .collect();
    Json(response)
}

async fn list_positions(State(bridge): State<Arc<Bridge>>) -> Json<Vec<PositionResponse>> {
    let positions = bridge.positions_snapshot();
    let response: Vec<PositionResponse> = positions
        .into_iter()
        .map(|((account_id, market_id), position)| PositionResponse {
            account_id,
            market_id,
            position,
        })
        .collect();
    Json(response)
}

async fn get_health(
    State(bridge): State<Arc<Bridge>>,
    Query(q): Query<HealthQuery>,
) -> Json<HealthResponse> {
    let mut prices: BTreeMap<MarketId, (u128, u128)> = BTreeMap::new();
    prices.insert(MarketId(0), (u128::from(q.coll_price), u128::from(q.debt_price)));

    let account = AccountId(q.account);
    let mark = MarkPrice(q.perp_mark);
    let inputs = bridge.compute_account_portfolio_inputs(account, mark, q.perp_im_bps, &prices);
    let free_equity = princeps_portfolio::compute_free_equity(&inputs);
    let is_healthy = princeps_portfolio::is_healthy(&inputs);

    Json(HealthResponse {
        account,
        free_equity,
        is_healthy,
        portfolio_inputs: inputs,
    })
}

/// Run an arbitrary caller-supplied cross-protocol book (a lending leg + a
/// perp leg) straight through the REAL `princeps-portfolio` kernel. Stateless:
/// nothing is seeded, the caller's numbers are the numbers. This is what the
/// UI calls so a trader's own book — not demo data — drives the engine.
async fn get_portfolio_health(Query(q): Query<PortfolioQuery>) -> Json<PortfolioHealthResponse> {
    let inputs = PortfolioInputs {
        perp_collateral: i128::from(q.perp_collateral),
        perp_unrealized_pnl: i128::from(q.perp_unrealized_pnl),
        perp_im_req: i128::from(q.perp_im_req),
        lending_adjusted_collateral_value: i128::from(q.lending_adjusted_collateral_value),
        lending_debt_value: i128::from(q.lending_debt_value),
    };
    let free_equity = princeps_portfolio::compute_free_equity(&inputs);
    let is_healthy = princeps_portfolio::is_healthy(&inputs);
    Json(PortfolioHealthResponse {
        free_equity,
        is_healthy,
        portfolio_inputs: inputs,
    })
}

/// Serve the single-page UI. Read from disk at request time (relative to the
/// crate manifest) so the frontend can be iterated without rebuilding.
async fn serve_ui() -> Result<Html<String>, (StatusCode, String)> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/ui/index.html");
    std::fs::read_to_string(path)
        .map(Html)
        .map_err(|e| (StatusCode::NOT_FOUND, format!("UI not found at {path}: {e}")))
}

async fn get_scan(
    State(bridge): State<Arc<Bridge>>,
    Query(q): Query<PricesQuery>,
) -> Json<princeps_evm::UnifiedScanReport> {
    let mut prices: BTreeMap<MarketId, (u128, u128)> = BTreeMap::new();
    prices.insert(MarketId(0), (u128::from(q.coll_price), u128::from(q.debt_price)));

    let mark = MarkPrice(q.perp_mark);
    let report = bridge.scan_unified(mark, q.perp_im_bps, &prices);
    Json(report)
}

// ============================================================
// Bridge setup (identical to liquidator-bot's pattern)
// ============================================================

fn setup_bridge() -> Bridge {
    let bridge = LiveRethEvmBridge::new((), dev_chain_spec());
    bridge.with_markets_mut(|m| {
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
            Bps(9_500),
            Bps(500),
            Bps(1_000),
            0,
        );
        market.total_supplied = 1_000_000;
        m.insert(MarketId(0), market);
    });
    bridge
}

fn seed_accounts(bridge: &Bridge) {
    let setups: [(u64, u128, u128); 5] = [
        (1, 1_000, 200),
        (2, 500, 300),
        (3, 200, 150),
        (4, 100, 90),
        (5, 80, 70),
    ];
    for &(acct_id, coll, debt) in &setups {
        let acct = AccountId(acct_id);
        bridge
            .lending_deposit_collateral(acct, MarketId(0), coll)
            .expect("seed deposit");
        bridge
            .lending_borrow(acct, MarketId(0), debt, 1, 1)
            .expect("seed borrow");
        bridge.with_accounts_mut(|map| {
            let mut a = Account::flat(acct);
            a.collateral = Notional(0);
            a.position_size = PositionSize(0);
            map.insert(acct, a);
        });
    }
}

fn dev_chain_spec() -> Arc<ChainSpec> {
    let custom_genesis = r#"{
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
    let genesis: Genesis =
        serde_json::from_str(custom_genesis).expect("dev genesis parses");
    Arc::new(genesis.into())
}

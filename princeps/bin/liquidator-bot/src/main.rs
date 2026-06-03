//! Sample liquidator bot for Princeps v0 (Stage 24e).
//!
//! Watches an in-process bridge for portfolio-underwater accounts via
//! `scan_unified`, picks the most-underwater each iteration, and
//! liquidates its lending position via the Stage 22b
//! `lending_liquidate` bridge method. If a target has more debt than
//! recoverable collateral (HF < 0), routes the shortfall through Stage
//! 22c's `absorb_account_bad_debt`.
//!
//! ### Scope
//!
//! This is a v0 **logical** demo — the bot operates on an in-process
//! `LiveRethEvmBridge<()>` rather than connecting to a running
//! `reth-devnet` via RPC. v1 will replace the in-process loop with an
//! RPC-driven version once Stage 24d (RPC endpoints) lands. The
//! mechanics — scan, target selection, liquidate, fallback to bad-debt
//! absorption — are identical.
//!
//! Token transfers between liquidator and pool (the EVM caller pays
//! repay, receives seized collateral) are NOT modeled here. The bridge
//! methods only mutate lending state; a real-EVM bot would also issue
//! ERC-20 calls between its own balance and the protocol.
//!
//! ### Usage
//!
//! ```bash
//! cargo run --bin princeps-liquidator-bot
//! cargo run --bin princeps-liquidator-bot -- --eth-price 70 --max-iterations 5
//! ```

use std::collections::BTreeMap;
use std::sync::Arc;

use alloy_genesis::Genesis;
use clap::Parser;
use rdk_clearing::Account;
use rdk_clob::AccountId;
use princeps_evm::LiveRethEvmBridge;
use rdk_funding::{MarkPrice, Notional, PositionSize};
use princeps_lending::{AssetId, Bps, Index as LendingIndex, IrmParams, Market, MarketId};
use reth_chainspec::ChainSpec;

#[derive(Debug, Parser)]
#[command(
    name = "princeps-liquidator-bot",
    version,
    about = "Sample liquidator bot for Princeps v0 (Stage 24e)"
)]
struct Args {
    /// ETH price the bot assumes during this run (USDC per ETH). Borrowers
    /// owe ETH; raising the price makes their debt more expensive and pushes
    /// them underwater. Default 2 (debt 2× more expensive than at seed time)
    /// flags 4 of the 5 seeded accounts.
    #[arg(long, default_value_t = 2)]
    eth_price: u128,

    /// Maximum number of liquidation iterations before the bot exits.
    #[arg(long, default_value_t = 10)]
    max_iterations: u32,

    /// The bot's own account id (used as the `liquidator` parameter to
    /// `bridge.lending_liquidate`).
    #[arg(long, default_value_t = 999)]
    liquidator_account: u64,

    /// Perp initial-margin bps used in the unified scan. Default 1000 =
    /// 10%, matching the openhl/Princeps default.
    #[arg(long, default_value_t = 1_000)]
    perp_im_bps: u32,
}

fn main() -> eyre::Result<()> {
    let args = Args::parse();

    println!();
    println!("=== Princeps sample liquidator bot (Stage 24e) ===");
    println!();
    println!("    ETH price assumption:  {}", args.eth_price);
    println!("    Max iterations:        {}", args.max_iterations);
    println!("    Liquidator account:    {}", args.liquidator_account);
    println!();

    let bridge = setup_bridge();
    seed_accounts(&bridge);

    println!("Seeded 5 accounts. Initial state:");
    print_position_table(&bridge);
    println!();

    let mut prices: BTreeMap<MarketId, (u128, u128)> = BTreeMap::new();
    prices.insert(MarketId(0), (1, args.eth_price));
    let mark = MarkPrice(u64::try_from(args.eth_price).unwrap_or(u64::MAX));

    let mut iter: u32 = 0;
    let mut total_repaid: u128 = 0;
    let mut total_seized: u128 = 0;
    let mut total_bad_debt: i128 = 0;

    loop {
        if iter >= args.max_iterations {
            println!("Reached --max-iterations ({}). Stopping.", args.max_iterations);
            break;
        }

        let scan = bridge.scan_unified(mark, args.perp_im_bps, &prices);
        println!(
            "[iter {:>2}] scanned {} accounts, {} flagged",
            iter + 1,
            scan.scanned,
            scan.flagged.len()
        );

        if scan.flagged.is_empty() {
            println!("           no flagged accounts — bot idle");
            break;
        }

        // Most-underwater first (smallest, i.e. most-negative, free_equity)
        let (target, free) = scan
            .flagged
            .iter()
            .min_by_key(|(_, free)| *free)
            .copied()
            .expect("flagged is non-empty");
        println!(
            "           target = AccountId({}), free_equity = {}",
            target.0, free
        );

        let liquidator = AccountId(args.liquidator_account);
        let market_id = MarketId(0);
        // Pass a large repay amount; lending_liquidate caps internally at outstanding debt.
        let huge = u128::MAX / 2;

        match bridge.lending_liquidate(liquidator, target, market_id, huge, 1, args.eth_price)
        {
            Ok(result) => {
                println!(
                    "           liquidated: repaid={} seized={} target_hf_after={}",
                    result.actual_repay, result.actual_seized, result.target_hf_after
                );
                total_repaid = total_repaid.saturating_add(result.actual_repay);
                total_seized = total_seized.saturating_add(result.actual_seized);
            }
            Err(e) => {
                println!("           liquidate refused ({:?})", e);
                // Either healthy (won't recur this iter), or position has no debt
                // left — try bad-debt absorption.
                let bad_debt = bridge.absorb_account_bad_debt(target, mark, &prices);
                if bad_debt > 0 {
                    println!(
                        "           absorbed bad debt: {} (would route to InsuranceFund in prod)",
                        bad_debt
                    );
                    total_bad_debt = total_bad_debt.saturating_add(bad_debt);
                } else {
                    println!("           no bad debt to absorb either; advancing");
                }
            }
        }

        iter += 1;
        println!();
    }

    println!();
    println!("=== Final state ===");
    print_position_table(&bridge);
    println!();
    println!("=== Summary ===");
    println!("    Iterations executed:   {}", iter);
    println!("    Total debt repaid:     {} (in ETH units)", total_repaid);
    println!("    Total collateral seized: {} (in USDC units)", total_seized);
    println!("    Bad debt absorbed:     {} (would flow to InsuranceFund)", total_bad_debt);
    println!();

    Ok(())
}

fn setup_bridge() -> LiveRethEvmBridge<()> {
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
            Bps(9_500), // LT 95%
            Bps(500),   // bonus 5%
            Bps(1_000), // reserve factor 10%
            0,
        );
        market.total_supplied = 1_000_000;
        m.insert(MarketId(0), market);
    });
    bridge
}

/// Five accounts borrowing ETH at seed-time price ETH=1, with varying
/// degrees of leverage. All are healthy at ETH=1 (HF >= 1.0). Raising
/// the ETH price (the default --eth-price=2) flags accounts 2–5; only
/// account 1 stays healthy.
fn seed_accounts(bridge: &LiveRethEvmBridge<()>) {
    let setups: [(u64, u128, u128); 5] = [
        (1, 1_000, 200), // very healthy: HF at price 1 = 4.75, at price 2 = 2.375
        (2, 500, 300),   // healthy at 1 (HF 1.58); flagged at 2 (HF 0.79)
        (3, 200, 150),   // healthy at 1 (HF 1.27); flagged at 2 (HF 0.63)
        (4, 100, 90),    // borderline at 1 (HF 1.06); flagged at 2 (HF 0.53)
        (5, 80, 70),     // thin at 1 (HF 1.086); flagged at 2 (HF 0.54)
    ];
    for &(acct_id, coll, debt) in &setups {
        let acct = AccountId(acct_id);
        bridge
            .lending_deposit_collateral(acct, MarketId(0), coll)
            .expect("seed deposit");
        bridge
            .lending_borrow(acct, MarketId(0), debt, 1, 1)
            .expect("seed borrow");
        // Also seed a (small) perp account for the unified scan to see.
        bridge.with_accounts_mut(|map| {
            let mut a = Account::flat(acct);
            a.collateral = Notional(0);
            a.position_size = PositionSize(0);
            map.insert(acct, a);
        });
    }
}

fn print_position_table(bridge: &LiveRethEvmBridge<()>) {
    println!("    AccountId   collateral   scaled_debt");
    println!("    ─────────   ──────────   ───────────");
    let positions = bridge.positions_snapshot();
    if positions.is_empty() {
        println!("    (none)");
        return;
    }
    for ((acct, _market), pos) in positions {
        println!(
            "    {:>9}   {:>10}   {:>11}",
            acct.0, pos.collateral_amount, pos.scaled_debt
        );
    }
}

/// Local copy of the dev chain spec used by `bin/princeps`. Self-contained
/// so the liquidator-bot stays a clean separate binary.
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

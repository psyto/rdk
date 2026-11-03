//! `cargo run -p rdk-liquidation --example quickstart`
//!
//! A 10-unit long opened at $100 with $15 of collateral per unit (15% margin),
//! checked as the mark falls.

use rdk_liquidation::{margin_health, margin_ratio, AccountSnapshot, LiquidationParams};
use rdk_units::{AccountId, MarkPrice, Notional, PositionSize};

fn main() {
    let params = LiquidationParams::hyperliquid_default(); // initial 10%, maintenance 2%
    let account = AccountSnapshot {
        account: AccountId(1),
        position_size: PositionSize(10),
        avg_entry: MarkPrice(100_000_000),
        collateral: Notional(150_000_000),
    };
    for mark in [100_000_000, 90_000_000, 86_000_000, 84_000_000] {
        let mark = MarkPrice(mark);
        println!(
            "mark {:>11}: margin {:>5} bp, {:?}",
            mark.0,
            margin_ratio(&account, mark).0,
            margin_health(&account, mark, &params)
        );
    }
}

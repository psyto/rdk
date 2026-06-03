//! Per-block interest accrual. Stage 19e.
//!
//! Each block, the bridge calls `accrue_interest` on every active market
//! before processing any user-facing operations. This:
//!
//! 1. Computes the borrow rate for the current utilization (`compute_borrow_rate`).
//! 2. Grows `borrow_index` linearly: `borrow_index *= (1 + rate × blocks_elapsed)`.
//! 3. Grows `total_borrowed` (nominal) by the same ratio.
//! 4. Routes `reserve_factor` of the accrued interest into `reserves`.
//!
//! Index-based bookkeeping means individual positions don't need
//! per-position interest math — `position.nominal_debt(market.borrow_index)`
//! automatically reflects the new debt.
//!
//! ## v0 scope: supply side parked
//!
//! v0 ships **without** supplier-side `supply_index` accounting. The pre-funded
//! lending pool is bridge-owned and accrues interest into `reserves` (insurance
//! fund). Aave-style supplier aTokens with separate supply rate accrual
//! lands in a later stage (19f or v1 multi-asset).
//!
//! Rationale: demonstrate lending + liquidation mechanics cleanly for v0
//! without complicating Position struct with `scaled_supply` field. Real
//! suppliers come in v1+ when we extend to multi-asset markets and need
//! third-party liquidity provision.
//!
//! ## Linear interest approximation
//!
//! `(1 + r × t)` rather than `(1 + r)^t`. For per-block accrual with small
//! per-block rates (RAY/10_000 ≈ 0.01%), the linear approximation is
//! accurate to <1 bps over a year. Matches Aave's convention.

use crate::irm::compute_borrow_rate;
use crate::types::{Index, Market};

/// Result of an `accrue_interest` call. Returned for logging / observability.
/// Not load-bearing on state — all mutations happened in-place on `market`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InterestAccrualReport {
    pub blocks_elapsed: u64,
    pub borrow_rate_per_block: u128,
    pub new_borrow_index: Index,
    pub interest_accrued: u128,
    pub reserves_added: u128,
}

impl InterestAccrualReport {
    /// A no-op report (no blocks elapsed, or nothing borrowed).
    #[must_use]
    pub fn no_change(borrow_index: Index) -> Self {
        Self {
            blocks_elapsed: 0,
            borrow_rate_per_block: 0,
            new_borrow_index: borrow_index,
            interest_accrued: 0,
            reserves_added: 0,
        }
    }
}

/// Accrue interest on `market` up to `current_block`. Idempotent: calling
/// with the same `current_block` as `market.last_accrual_block` is a no-op.
pub fn accrue_interest(market: &mut Market, current_block: u64) -> InterestAccrualReport {
    if current_block <= market.last_accrual_block {
        return InterestAccrualReport::no_change(market.borrow_index);
    }
    let blocks_elapsed = current_block - market.last_accrual_block;

    // Nothing borrowed → no interest, but advance the clock so future
    // accruals don't claim phantom blocks.
    if market.total_borrowed == 0 {
        market.last_accrual_block = current_block;
        return InterestAccrualReport::no_change(market.borrow_index);
    }

    let utilization = market.utilization_bps();
    let borrow_rate_per_block = compute_borrow_rate(utilization, &market.irm_params);

    // Linear interest factor: rate × blocks. RAY-scaled.
    let interest_factor = borrow_rate_per_block.saturating_mul(u128::from(blocks_elapsed));

    // borrow_index_growth = current_index × interest_factor ÷ RAY
    let index_growth = market.borrow_index.0.saturating_mul(interest_factor) / Index::RAY;
    let new_borrow_index = market.borrow_index.0.saturating_add(index_growth);

    // Interest accrued (nominal) = total_borrowed × interest_factor ÷ RAY
    let interest_accrued = market.total_borrowed.saturating_mul(interest_factor) / Index::RAY;

    // Reserves cut = interest × reserve_factor_bps ÷ 10_000
    let reserve_cut =
        interest_accrued.saturating_mul(u128::from(market.reserve_factor.0)) / 10_000;

    // Apply
    market.borrow_index = Index(new_borrow_index);
    market.total_borrowed = market.total_borrowed.saturating_add(interest_accrued);
    market.reserves = market.reserves.saturating_add(reserve_cut);
    market.last_accrual_block = current_block;

    InterestAccrualReport {
        blocks_elapsed,
        borrow_rate_per_block,
        new_borrow_index: market.borrow_index,
        interest_accrued,
        reserves_added: reserve_cut,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AssetId, Bps, IrmParams, MarketId};

    fn standard_market() -> Market {
        Market::new(
            MarketId(0),
            AssetId(1),
            AssetId(0),
            IrmParams {
                base_rate_per_block: 0,
                slope_below_kink_per_block: Index::RAY / 10_000, // 0.01% added at kink
                slope_above_kink_per_block: Index::RAY / 1_000,
                kink_bps: Bps(8_000),
            },
            Bps(9_500),
            Bps(500),
            Bps(1_000), // 10% reserve factor
            0,
        )
    }

    #[test]
    fn no_blocks_elapsed_is_no_op() {
        let mut m = standard_market();
        m.total_supplied = 1_000;
        m.total_borrowed = 500;
        let before = m.clone();
        let report = accrue_interest(&mut m, 0);
        assert_eq!(report.blocks_elapsed, 0);
        assert_eq!(report.interest_accrued, 0);
        assert_eq!(m, before);
    }

    #[test]
    fn current_block_earlier_than_last_is_no_op() {
        let mut m = standard_market();
        m.last_accrual_block = 100;
        let before = m.clone();
        accrue_interest(&mut m, 50);
        assert_eq!(m, before);
    }

    #[test]
    fn no_borrowed_advances_clock_only() {
        let mut m = standard_market();
        m.total_supplied = 1_000;
        m.total_borrowed = 0;
        let before_index = m.borrow_index;
        let report = accrue_interest(&mut m, 100);
        assert_eq!(report.interest_accrued, 0);
        assert_eq!(report.reserves_added, 0);
        assert_eq!(m.borrow_index, before_index);
        assert_eq!(m.last_accrual_block, 100); // clock advanced
        assert_eq!(m.reserves, 0);
        assert_eq!(m.total_borrowed, 0);
    }

    #[test]
    fn accrual_at_50_percent_utilization_grows_index() {
        let mut m = standard_market();
        m.total_supplied = 1_000;
        m.total_borrowed = 500;
        let before_index = m.borrow_index;
        let report = accrue_interest(&mut m, 100);
        assert!(report.blocks_elapsed == 100);
        assert!(report.borrow_rate_per_block > 0);
        assert!(m.borrow_index.0 > before_index.0);
    }

    #[test]
    fn accrual_grows_total_borrowed_proportionally() {
        let mut m = standard_market();
        m.total_supplied = 1_000;
        m.total_borrowed = 500;
        let before_borrowed = m.total_borrowed;
        let report = accrue_interest(&mut m, 1_000);
        assert!(m.total_borrowed > before_borrowed);
        // interest_accrued == new_total - old_total
        assert_eq!(m.total_borrowed - before_borrowed, report.interest_accrued);
    }

    #[test]
    fn reserve_cut_is_reserve_factor_of_interest() {
        let mut m = standard_market();
        m.total_supplied = 1_000;
        m.total_borrowed = 500;
        let report = accrue_interest(&mut m, 10_000);
        // reserve_factor = 10% (1000 bps)
        // reserves_added should be 10% of interest_accrued
        let expected_reserves = report.interest_accrued / 10;
        assert_eq!(report.reserves_added, expected_reserves);
        assert_eq!(m.reserves, expected_reserves);
    }

    #[test]
    fn idempotent_when_called_twice_with_same_block() {
        let mut m = standard_market();
        m.total_supplied = 1_000;
        m.total_borrowed = 500;
        accrue_interest(&mut m, 100);
        let snapshot = m.clone();
        let report2 = accrue_interest(&mut m, 100);
        assert_eq!(report2.blocks_elapsed, 0);
        assert_eq!(m, snapshot);
    }

    #[test]
    fn accrual_advances_last_block() {
        let mut m = standard_market();
        m.total_supplied = 1_000;
        m.total_borrowed = 500;
        accrue_interest(&mut m, 1_234);
        assert_eq!(m.last_accrual_block, 1_234);
    }

    #[test]
    fn zero_rate_yields_no_interest_but_advances_clock() {
        // base = 0, kink = high, utilization low → rate = 0
        let mut m = standard_market();
        m.irm_params.slope_below_kink_per_block = 0;
        m.irm_params.slope_above_kink_per_block = 0;
        m.total_supplied = 1_000;
        m.total_borrowed = 500;

        let report = accrue_interest(&mut m, 1_000);
        assert_eq!(report.borrow_rate_per_block, 0);
        assert_eq!(report.interest_accrued, 0);
        assert_eq!(m.last_accrual_block, 1_000);
        assert_eq!(m.borrow_index, Index::ONE);
    }

    #[test]
    fn multiple_sequential_accruals_compound_via_index() {
        let mut m = standard_market();
        m.total_supplied = 10_000;
        m.total_borrowed = 5_000;

        let index_0 = m.borrow_index;
        accrue_interest(&mut m, 100);
        let index_1 = m.borrow_index;
        accrue_interest(&mut m, 200);
        let index_2 = m.borrow_index;

        // Each step should have grown the index strictly
        assert!(index_1.0 > index_0.0);
        assert!(index_2.0 > index_1.0);
    }

    #[test]
    fn report_no_change_helper_is_consistent() {
        let report = InterestAccrualReport::no_change(Index(42));
        assert_eq!(report.blocks_elapsed, 0);
        assert_eq!(report.interest_accrued, 0);
        assert_eq!(report.reserves_added, 0);
        assert_eq!(report.new_borrow_index, Index(42));
    }
}

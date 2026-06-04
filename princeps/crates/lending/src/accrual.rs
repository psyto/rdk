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
//! ## Supply-side accrual (v1 multi-asset / `scaled_supply` work)
//!
//! Each accrual call also grows `supply_index` per the Aave standard:
//!
//! ```text
//! supply_rate = borrow_rate × utilization × (1 - reserve_factor)
//! ```
//!
//! and grows `total_supplied` by the supplier-cut portion of accrued
//! interest (`interest_accrued − reserve_cut`). The supplier side is
//! "additive only" at v0: the bridge-owned pool continues to provide
//! initial liquidity via `total_supplied`, and new per-depositor positions
//! using `scaled_supply` are layered on top. The invariant
//! `sum(position.nominal_supply(supply_index)) ≤ total_supplied` holds
//! at v0, with the gap being the bridge's implicit pool. No bridge
//! migration in this commit — see ADR-010 Layer 3 deferral note for
//! the wider scope of supplier-side work still pending.
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
    /// Supply-side index after this accrual. Mirrors `new_borrow_index`
    /// for the supplier path (v1 multi-asset / `scaled_supply` work).
    pub new_supply_index: Index,
    /// Supplier-cut interest added to `total_supplied` this accrual
    /// (`interest_accrued − reserves_added`). This is the share of
    /// interest that flows to depositors via `supply_index` growth.
    pub supply_accrued: u128,
}

impl InterestAccrualReport {
    /// A no-op report (no blocks elapsed, or nothing borrowed).
    #[must_use]
    pub fn no_change(borrow_index: Index, supply_index: Index) -> Self {
        Self {
            blocks_elapsed: 0,
            borrow_rate_per_block: 0,
            new_borrow_index: borrow_index,
            interest_accrued: 0,
            reserves_added: 0,
            new_supply_index: supply_index,
            supply_accrued: 0,
        }
    }
}

/// Accrue interest on `market` up to `current_block`. Idempotent: calling
/// with the same `current_block` as `market.last_accrual_block` is a no-op.
pub fn accrue_interest(market: &mut Market, current_block: u64) -> InterestAccrualReport {
    if current_block <= market.last_accrual_block {
        return InterestAccrualReport::no_change(market.borrow_index, market.supply_index);
    }
    let blocks_elapsed = current_block - market.last_accrual_block;

    // Nothing borrowed → no interest, but advance the clock so future
    // accruals don't claim phantom blocks.
    if market.total_borrowed == 0 {
        market.last_accrual_block = current_block;
        return InterestAccrualReport::no_change(market.borrow_index, market.supply_index);
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

    // Supplier cut = interest − reserves cut. This is what suppliers earn.
    let supply_accrued = interest_accrued.saturating_sub(reserve_cut);

    // supply_index_growth = current_supply_index × (supply_accrued / total_supplied).
    // Equivalently in Aave form: supply_rate = borrow_rate × utilization
    //   × (1 − reserve_factor); we derive it from observed quantities to
    // stay arithmetically consistent with the borrow-side computation
    // even under boundary cases (zero supply, large reserve factor).
    let supply_index_growth = if market.total_supplied == 0 {
        // No depositors at all (not even the bridge's implicit pool) →
        // supply_index can't grow because there's nothing to scale
        // against. supply_accrued is still credited into total_supplied
        // below; future depositors then start fresh at the unit index.
        0
    } else {
        market
            .supply_index
            .0
            .saturating_mul(supply_accrued)
            / market.total_supplied
    };
    let new_supply_index = market.supply_index.0.saturating_add(supply_index_growth);

    // Apply
    market.borrow_index = Index(new_borrow_index);
    market.supply_index = Index(new_supply_index);
    market.total_borrowed = market.total_borrowed.saturating_add(interest_accrued);
    market.total_supplied = market.total_supplied.saturating_add(supply_accrued);
    market.reserves = market.reserves.saturating_add(reserve_cut);
    market.last_accrual_block = current_block;

    InterestAccrualReport {
        blocks_elapsed,
        borrow_rate_per_block,
        new_borrow_index: market.borrow_index,
        interest_accrued,
        reserves_added: reserve_cut,
        new_supply_index: market.supply_index,
        supply_accrued,
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
        let report = InterestAccrualReport::no_change(Index(42), Index(99));
        assert_eq!(report.blocks_elapsed, 0);
        assert_eq!(report.interest_accrued, 0);
        assert_eq!(report.reserves_added, 0);
        assert_eq!(report.new_borrow_index, Index(42));
        assert_eq!(report.new_supply_index, Index(99));
        assert_eq!(report.supply_accrued, 0);
    }

    // ─── supply-side accrual (v1 multi-asset / scaled_supply work) ─────────

    #[test]
    fn accrual_grows_supply_index_when_borrowed() {
        // Symmetric to accrual_at_50_percent_utilization_grows_index but
        // for the supply side. With borrowers, supply_index must grow.
        let mut m = standard_market();
        m.total_supplied = 1_000;
        m.total_borrowed = 500;
        let before = m.supply_index;
        let report = accrue_interest(&mut m, 100);
        assert!(report.new_supply_index.0 > before.0);
        assert!(report.supply_accrued > 0);
        assert_eq!(m.supply_index, report.new_supply_index);
    }

    #[test]
    fn supply_accrued_equals_interest_minus_reserve_cut() {
        // Conservation: every unit of interest is either credited to
        // suppliers or to reserves. Exact equality on the boundary.
        let mut m = standard_market();
        m.total_supplied = 1_000;
        m.total_borrowed = 500;
        let report = accrue_interest(&mut m, 10_000);
        assert_eq!(
            report.supply_accrued + report.reserves_added,
            report.interest_accrued,
        );
    }

    #[test]
    fn accrual_grows_total_supplied_by_supply_accrued() {
        // total_supplied tracks nominal supply. After accrual it should
        // have grown by exactly the supplier-cut interest.
        let mut m = standard_market();
        m.total_supplied = 1_000;
        m.total_borrowed = 500;
        let before = m.total_supplied;
        let report = accrue_interest(&mut m, 5_000);
        assert_eq!(m.total_supplied - before, report.supply_accrued);
    }

    #[test]
    fn supply_index_does_not_grow_when_nothing_borrowed() {
        // No borrowers → no interest → supply_index stays put.
        let mut m = standard_market();
        m.total_supplied = 1_000;
        m.total_borrowed = 0;
        let before = m.supply_index;
        let report = accrue_interest(&mut m, 1_000);
        assert_eq!(report.supply_accrued, 0);
        assert_eq!(m.supply_index, before);
    }

    #[test]
    fn zero_total_supplied_does_not_panic_or_grow_index() {
        // Boundary: total_borrowed > 0 with total_supplied == 0 is
        // arithmetically pathological (utilization would be ∞). Our
        // utilization is capped at 100%, so the rate computation
        // still produces something. supply_index must not be divided
        // by zero — accrual short-circuits that branch.
        let mut m = standard_market();
        m.total_supplied = 0;
        m.total_borrowed = 500;
        let before_supply_index = m.supply_index;
        let report = accrue_interest(&mut m, 100);
        // supply_index unchanged (no denominator to grow against), but
        // total_supplied is credited with the supplier-cut interest
        // — the bridge-implicit pool absorbs it.
        assert_eq!(report.new_supply_index, before_supply_index);
        assert_eq!(m.total_supplied, report.supply_accrued);
    }

    #[test]
    fn supplier_position_earns_yield_after_accrual() {
        // End-to-end: a depositor with a scaled_supply position should
        // see their nominal_supply grow after accrual runs.
        use crate::position::supply;
        use crate::types::{MarketId, Position};

        let mut m = standard_market();
        m.total_supplied = 1_000;
        m.total_borrowed = 500;

        // Open a supplier position of 100 at the current (unit) supply_index.
        let mut depositor = Position::empty(MarketId(0));
        supply(&mut depositor, 100, m.supply_index).unwrap();
        let nominal_before = depositor.nominal_supply(m.supply_index);
        assert_eq!(nominal_before, 100);

        // Run lots of blocks of accrual.
        accrue_interest(&mut m, 100_000);

        // Depositor's nominal_supply should be strictly greater now
        // — the index grew, the underlying scaled_supply didn't move.
        let nominal_after = depositor.nominal_supply(m.supply_index);
        assert!(
            nominal_after > nominal_before,
            "expected yield, got {} → {}",
            nominal_before,
            nominal_after,
        );
    }

    #[test]
    fn supply_index_is_monotonic_across_consecutive_accruals() {
        let mut m = standard_market();
        m.total_supplied = 10_000;
        m.total_borrowed = 5_000;

        let index_0 = m.supply_index;
        accrue_interest(&mut m, 100);
        let index_1 = m.supply_index;
        accrue_interest(&mut m, 200);
        let index_2 = m.supply_index;
        assert!(index_1.0 > index_0.0);
        assert!(index_2.0 > index_1.0);
    }
}

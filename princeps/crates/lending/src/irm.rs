//! Interest-rate-model pure compute. Stage 19c.
//!
//! Kinked two-slope rate curve, the same shape Aave / Compound use:
//!
//! ```text
//!     borrow_rate
//!          ▲
//!          │                                      ╱
//!          │                                   ╱
//!          │                                ╱  (slope_above)
//!          │                             ╱
//! base +   │                       ─────●  ← rate at kink
//! below ──→│                  ╱     (kink_bps)
//!          │             ╱
//!          │        ╱  (slope_below)
//!          │   ╱
//!     base ●
//!          └─────────────────────────────────────────→ utilization
//!          0                  kink                100%
//! ```
//!
//! - **Below kink**: `rate = base + slope_below × (u / kink)`
//! - **At or above kink**: `rate = base + slope_below + slope_above × ((u − kink) / (100% − kink))`
//!
//! Where `slope_below` is the *total* additional rate added between 0 and kink,
//! and `slope_above` is the *total* additional rate added between kink and 100%.
//!
//! All rates are RAY-scaled per-block (e.g., `RAY / 1_000` = 0.1% per block).
//! Supply-side rate compute lands in Stage 19e (alongside interest accrual).

use crate::types::{Bps, IrmParams};

/// Compute the borrow rate at `utilization` given the IRM `params`.
///
/// Returns a RAY-scaled per-block rate. Saturating arithmetic; never panics.
#[must_use]
pub fn compute_borrow_rate(utilization: Bps, params: &IrmParams) -> u128 {
    let u = u128::from(utilization.0);
    let kink = u128::from(params.kink_bps.0);

    if u <= kink {
        // Below or at kink boundary
        if kink == 0 {
            // Degenerate: kink at 0 means everything is "above kink"; handle in else branch
            return compute_above_kink(0, params);
        }
        let interp = params.slope_below_kink_per_block.saturating_mul(u) / kink;
        params.base_rate_per_block.saturating_add(interp)
    } else {
        compute_above_kink(u, params)
    }
}

/// Helper: rate when utilization is strictly above kink (or kink == 0).
fn compute_above_kink(u: u128, params: &IrmParams) -> u128 {
    const HUNDRED_PCT: u128 = 10_000;
    let kink = u128::from(params.kink_bps.0);
    let above = u.saturating_sub(kink);
    let remaining = HUNDRED_PCT.saturating_sub(kink);

    if remaining == 0 {
        // Degenerate: kink at 100% → no "above" range; cap at base + slope_below
        return params
            .base_rate_per_block
            .saturating_add(params.slope_below_kink_per_block);
    }

    let interp_above = params.slope_above_kink_per_block.saturating_mul(above) / remaining;
    params
        .base_rate_per_block
        .saturating_add(params.slope_below_kink_per_block)
        .saturating_add(interp_above)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Index;

    fn standard_params() -> IrmParams {
        IrmParams {
            base_rate_per_block: 0,
            slope_below_kink_per_block: Index::RAY / 10_000, // 0.01% added at kink
            slope_above_kink_per_block: Index::RAY / 1_000,  // 0.1% added at 100%
            kink_bps: Bps(8_000),                            // kink at 80%
        }
    }

    #[test]
    fn rate_at_zero_utilization_equals_base() {
        let p = standard_params();
        assert_eq!(compute_borrow_rate(Bps(0), &p), 0);

        // With nonzero base
        let mut p2 = p;
        p2.base_rate_per_block = Index::RAY / 100_000;
        assert_eq!(compute_borrow_rate(Bps(0), &p2), Index::RAY / 100_000);
    }

    #[test]
    fn rate_at_kink_equals_base_plus_slope_below() {
        let p = standard_params();
        let rate = compute_borrow_rate(p.kink_bps, &p);
        let expected = p.base_rate_per_block + p.slope_below_kink_per_block;
        assert_eq!(rate, expected);
    }

    #[test]
    fn rate_at_100_percent_equals_base_plus_both_slopes() {
        let p = standard_params();
        let rate = compute_borrow_rate(Bps(10_000), &p);
        let expected =
            p.base_rate_per_block + p.slope_below_kink_per_block + p.slope_above_kink_per_block;
        assert_eq!(rate, expected);
    }

    #[test]
    fn rate_at_half_kink_is_half_slope_below() {
        // kink = 8000, half = 4000
        let p = standard_params();
        let rate = compute_borrow_rate(Bps(4_000), &p);
        let expected = p.base_rate_per_block + (p.slope_below_kink_per_block / 2);
        assert_eq!(rate, expected);
    }

    #[test]
    fn rate_above_kink_interpolates_linearly() {
        // kink = 8000, at u = 9000 (halfway between kink and 100%)
        let p = standard_params();
        let rate = compute_borrow_rate(Bps(9_000), &p);
        let expected = p.base_rate_per_block
            + p.slope_below_kink_per_block
            + (p.slope_above_kink_per_block / 2);
        assert_eq!(rate, expected);
    }

    #[test]
    fn rate_is_monotonically_non_decreasing_in_utilization() {
        let p = standard_params();
        let mut prev = compute_borrow_rate(Bps(0), &p);
        for u in (0..=10_000).step_by(500) {
            let next = compute_borrow_rate(Bps(u), &p);
            assert!(next >= prev, "rate dropped at u={u}: {prev} -> {next}");
            prev = next;
        }
    }

    #[test]
    fn kink_at_100_percent_caps_rate_at_below_slope() {
        let mut p = standard_params();
        p.kink_bps = Bps(10_000);
        // At any utilization including 100%, no "above" range exists
        let rate = compute_borrow_rate(Bps(10_000), &p);
        assert_eq!(rate, p.base_rate_per_block + p.slope_below_kink_per_block);
    }

    #[test]
    fn kink_at_zero_uses_above_slope_only() {
        let mut p = standard_params();
        p.kink_bps = Bps(0);
        // Below "kink at 0" is empty; everything is above-kink
        let rate_at_zero = compute_borrow_rate(Bps(0), &p);
        // At u=0 with kink=0: in above-kink branch, above = 0, so rate = base + slope_below
        assert_eq!(
            rate_at_zero,
            p.base_rate_per_block + p.slope_below_kink_per_block
        );

        let rate_at_full = compute_borrow_rate(Bps(10_000), &p);
        // At u=100% with kink=0: above = 10000, remaining = 10000, full slope_above applies
        assert_eq!(
            rate_at_full,
            p.base_rate_per_block + p.slope_below_kink_per_block + p.slope_above_kink_per_block
        );
    }

    #[test]
    fn zero_params_give_zero_rate_everywhere() {
        let p = IrmParams {
            base_rate_per_block: 0,
            slope_below_kink_per_block: 0,
            slope_above_kink_per_block: 0,
            kink_bps: Bps(8_000),
        };
        assert_eq!(compute_borrow_rate(Bps(0), &p), 0);
        assert_eq!(compute_borrow_rate(Bps(5_000), &p), 0);
        assert_eq!(compute_borrow_rate(Bps(10_000), &p), 0);
    }

    #[test]
    fn saturating_arithmetic_doesnt_panic_at_max() {
        let p = IrmParams {
            base_rate_per_block: u128::MAX / 2,
            slope_below_kink_per_block: u128::MAX / 2,
            slope_above_kink_per_block: u128::MAX / 2,
            kink_bps: Bps(8_000),
        };
        // Should saturate, not panic
        let rate = compute_borrow_rate(Bps(10_000), &p);
        assert_eq!(rate, u128::MAX);
    }
}

//! Health factor compute. Stage 19d.
//!
//! The health factor is the single number that drives liquidation:
//!
//! ```text
//!                     collateral_value × LT
//! health_factor  =  ─────────────────────────
//!                          debt_value
//! ```
//!
//! Where:
//! - `collateral_value` = collateral_amount × collateral_price (same units as debt)
//! - `debt_value` = nominal_debt × debt_price (`scaled_debt × borrow_index ÷ RAY × debt_price`)
//! - `LT` = market.liquidation_threshold in basis points
//!
//! Returns are RAY-scaled. The thresholds:
//! - `HF > RAY` → position healthy
//! - `HF == RAY` → exactly at liquidation threshold (boundary)
//! - `HF < RAY` → position liquidatable
//! - `HF == u128::MAX` → position has no debt (infinite health)
//!
//! ## Pure math vs. wrapping convenience
//!
//! `compute_health_factor_from_values` is the pure kernel that takes
//! pre-normalized USD-equivalent values. `compute_health_factor` is a
//! thin convenience wrapper for callers that already have raw amounts +
//! prices in matching units.
//!
//! For complex unit normalization (8-decimal prices vs 6/18-decimal
//! assets, oracle scaling, etc.) the bridge layer does the conversion
//! before calling this crate.

use crate::types::{Bps, Index, Market, Position};

/// Compute health factor from pre-normalized values.
///
/// `collateral_value` and `debt_value` must be in the same units. Returns
/// RAY-scaled health factor. Saturating arithmetic.
#[must_use]
pub fn compute_health_factor_from_values(
    collateral_value: u128,
    debt_value: u128,
    liquidation_threshold: Bps,
) -> u128 {
    if debt_value == 0 {
        return u128::MAX;
    }
    let lt = u128::from(liquidation_threshold.0);
    let adjusted = collateral_value.saturating_mul(lt) / 10_000;
    let scaled = adjusted.saturating_mul(Index::RAY);
    scaled.checked_div(debt_value).unwrap_or(u128::MAX)
}

/// Compute health factor for a `Position` in a `Market` given current prices.
///
/// Convenience wrapper. Caller (the bridge) must pass prices in units
/// consistent with the asset amounts (e.g., both in 8-decimal USD scaling).
#[must_use]
pub fn compute_health_factor(
    position: &Position,
    market: &Market,
    collateral_price: u128,
    debt_price: u128,
) -> u128 {
    let nominal_debt = position.nominal_debt(market.borrow_index);
    let collateral_value = position.collateral_amount.saturating_mul(collateral_price);
    let debt_value = nominal_debt.saturating_mul(debt_price);
    compute_health_factor_from_values(collateral_value, debt_value, market.liquidation_threshold)
}

/// Is this position liquidatable at the given prices?
///
/// True iff `compute_health_factor < RAY` (i.e., HF < 1.0).
#[must_use]
pub fn is_liquidatable(
    position: &Position,
    market: &Market,
    collateral_price: u128,
    debt_price: u128,
) -> bool {
    compute_health_factor(position, market, collateral_price, debt_price) < Index::RAY
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AssetId, IrmParams, MarketId};

    fn standard_market() -> Market {
        Market::new(
            MarketId(0),
            AssetId(1),
            AssetId(0),
            IrmParams {
                base_rate_per_block: 0,
                slope_below_kink_per_block: Index::RAY / 10_000,
                slope_above_kink_per_block: Index::RAY / 1_000,
                kink_bps: Bps(8_000),
            },
            Bps(9_500), // LT 95%
            Bps(500),
            Bps(1_000),
            0,
        )
    }

    // --- from_values pure kernel ---

    #[test]
    fn hf_with_zero_debt_is_max() {
        let hf = compute_health_factor_from_values(0, 0, Bps(9_500));
        assert_eq!(hf, u128::MAX);

        let hf2 = compute_health_factor_from_values(1_000_000, 0, Bps(9_500));
        assert_eq!(hf2, u128::MAX);
    }

    #[test]
    fn hf_with_lt_100_and_equal_values_is_one() {
        // collateral_value = debt_value, LT = 100% → HF = 1.0 (RAY)
        let hf = compute_health_factor_from_values(1_000, 1_000, Bps(10_000));
        assert_eq!(hf, Index::RAY);
    }

    #[test]
    fn hf_above_one_when_healthy() {
        // Twice the collateral as debt, LT 100% → HF = 2.0
        let hf = compute_health_factor_from_values(2_000, 1_000, Bps(10_000));
        assert_eq!(hf, Index::RAY * 2);
    }

    #[test]
    fn hf_below_one_when_underwater() {
        // Half the collateral as debt, LT 100% → HF = 0.5
        let hf = compute_health_factor_from_values(500, 1_000, Bps(10_000));
        assert_eq!(hf, Index::RAY / 2);
    }

    #[test]
    fn hf_with_lt_95_applies_haircut() {
        // collateral = debt, LT = 95% → HF = 0.95
        let hf = compute_health_factor_from_values(1_000, 1_000, Bps(9_500));
        // Expected: 1000 × 9500 / 10000 × RAY / 1000 = 950 × RAY / 1000 = 0.95 RAY
        assert_eq!(hf, Index::RAY * 95 / 100);
    }

    #[test]
    fn hf_at_exactly_liquidation_threshold_is_one_ray() {
        // collateral × LT = debt × 10000  →  HF = RAY
        // collateral = 1000, debt = 950, LT = 9500
        // 1000 × 9500 / 10000 = 950 == debt → HF = 1.0
        let hf = compute_health_factor_from_values(1_000, 950, Bps(9_500));
        assert_eq!(hf, Index::RAY);
    }

    #[test]
    fn hf_just_below_threshold_is_just_below_one() {
        // collateral = 1000, debt = 951, LT = 9500
        // adjusted = 1000 × 9500/10000 = 950
        // HF = 950 × RAY / 951 → very slightly less than RAY
        let hf = compute_health_factor_from_values(1_000, 951, Bps(9_500));
        assert!(hf < Index::RAY);
        assert!(hf > Index::RAY * 99 / 100); // still close to 1
    }

    // --- wrapping function with Position + Market ---

    #[test]
    fn wrapper_with_no_debt_returns_max() {
        let pos = Position::empty(MarketId(0));
        let market = standard_market();
        let hf = compute_health_factor(&pos, &market, 1, 1);
        assert_eq!(hf, u128::MAX);
    }

    #[test]
    fn wrapper_healthy_position() {
        // 1000 collateral @ price 1, 100 debt @ price 1, LT 95%
        // HF = 1000 × 9500/10000 × RAY / 100 = 950 × RAY / 100 = 9.5 RAY
        let pos = Position {
            market_id: MarketId(0),
            collateral_amount: 1_000,
            scaled_debt: 100,
        };
        let market = standard_market(); // borrow_index = ONE → nominal == scaled
        let hf = compute_health_factor(&pos, &market, 1, 1);
        assert_eq!(hf, Index::RAY * 95 / 10);
    }

    #[test]
    fn wrapper_liquidatable_position() {
        // 100 collateral, 200 debt, equal prices, LT 95%
        // adjusted = 100 × 0.95 = 95, HF = 95 / 200 = 0.475
        let pos = Position {
            market_id: MarketId(0),
            collateral_amount: 100,
            scaled_debt: 200,
        };
        let market = standard_market();
        assert!(is_liquidatable(&pos, &market, 1, 1));
        let hf = compute_health_factor(&pos, &market, 1, 1);
        assert!(hf < Index::RAY);
    }

    #[test]
    fn is_liquidatable_threshold_boundary() {
        // Exactly HF = 1.0 is NOT liquidatable (must be strictly less)
        let pos = Position {
            market_id: MarketId(0),
            collateral_amount: 1_000,
            scaled_debt: 950,
        };
        let mut market = standard_market();
        market.liquidation_threshold = Bps(9_500);
        // HF = 1000 × 9500/10000 × RAY / 950 = 950 × RAY / 950 = RAY
        let hf = compute_health_factor(&pos, &market, 1, 1);
        assert_eq!(hf, Index::RAY);
        assert!(!is_liquidatable(&pos, &market, 1, 1));
    }

    #[test]
    fn wrapper_respects_borrow_index_for_nominal_debt() {
        // scaled_debt = 100, borrow_index = 2.0 → nominal_debt = 200
        // 1000 collateral × 1, 200 nominal_debt × 1, LT 95%
        // HF = 1000 × 0.95 × RAY / 200 = 4.75 RAY
        let pos = Position {
            market_id: MarketId(0),
            collateral_amount: 1_000,
            scaled_debt: 100,
        };
        let mut market = standard_market();
        market.borrow_index = Index(Index::RAY * 2);
        let hf = compute_health_factor(&pos, &market, 1, 1);
        assert_eq!(hf, Index::RAY * 475 / 100);
    }

    #[test]
    fn wrapper_respects_debt_price() {
        // Same scaled_debt but debt_price doubles → effective debt value doubles → HF halves
        let pos = Position {
            market_id: MarketId(0),
            collateral_amount: 1_000,
            scaled_debt: 100,
        };
        let market = standard_market();
        let hf_low_price = compute_health_factor(&pos, &market, 1, 1);
        let hf_high_price = compute_health_factor(&pos, &market, 1, 2);
        assert_eq!(hf_high_price, hf_low_price / 2);
    }
}

//! Lending market + position types. Stage 19a foundation.
//!
//! Types only — no behavior beyond constructors, simple field accessors, and
//! one pure math helper per type (utilization, nominal_debt). IRM compute,
//! health factor, and interest accrual land in Stages 19c–19e.

use serde::{Deserialize, Serialize};

/// A lending market is uniquely identified by a small numeric ID.
///
/// v0 ships a single market (USDC collateral, ETH borrow) but the type
/// is multi-market ready by design — `BTreeMap<MarketId, Market>` works
/// the same with one entry or one hundred.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct MarketId(pub u32);

/// An asset ID — references collateral or underlying.
///
/// v0 ships two assets (USDC + ETH). Asset registry is bridge-owned;
/// this crate only needs the opaque identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AssetId(pub u32);

/// Basis points (1/100 of a percent). 10_000 bps = 100%.
///
/// Used for liquidation thresholds, liquidation bonuses, reserve factors,
/// and utilization ratios. u16 caps at 65_535 which is wider than any
/// meaningful bps value (10_000 = 100%).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Bps(pub u16);

impl Bps {
    pub const ZERO: Bps = Bps(0);
    pub const ONE_HUNDRED_PERCENT: Bps = Bps(10_000);

    #[must_use]
    pub fn as_fraction(self) -> f64 {
        f64::from(self.0) / 10_000.0
    }
}

/// Cumulative interest index, RAY-scaled (1.0 = 10^27).
///
/// Following Aave convention. The borrow_index and supply_index on each
/// `Market` are monotonically non-decreasing across blocks; scaled position
/// balances multiplied by this index give the nominal value at the current
/// block. This lets per-position interest math stay O(1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Index(pub u128);

impl Index {
    /// 1.0 in RAY units — Aave convention, 10^27.
    pub const RAY: u128 = 1_000_000_000_000_000_000_000_000_000;
    pub const ONE: Index = Index(Self::RAY);
}

/// Kinked interest-rate-model parameters.
///
/// - utilization < kink → rate = base + slope_below × (utilization / kink)
/// - utilization ≥ kink → rate = base + slope_below + slope_above × ((utilization - kink) / (100% - kink))
///
/// All rate fields are per-block RAY-scaled values (per-block ≈ per-second
/// for current 1-block-per-second target). IRM compute itself lands in Stage 19c.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct IrmParams {
    pub base_rate_per_block: u128,
    pub slope_below_kink_per_block: u128,
    pub slope_above_kink_per_block: u128,
    pub kink_bps: Bps,
}

/// Per-market state. Owned by the bridge; mutated through `apply_*` functions
/// (added in Stage 19e+). This struct is pure data — no methods that mutate
/// reserves/totals/indices except via the explicit pure transitions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Market {
    pub id: MarketId,
    pub underlying: AssetId,
    pub collateral_type: AssetId,
    pub total_supplied: u128,
    pub total_borrowed: u128,
    pub reserves: u128,
    pub borrow_index: Index,
    pub supply_index: Index,
    pub last_accrual_block: u64,
    pub irm_params: IrmParams,
    pub liquidation_threshold: Bps,
    pub liquidation_bonus: Bps,
    pub reserve_factor: Bps,
}

/// Per-account position in a specific market.
///
/// Index-based accounting: `scaled_debt × borrow_index ÷ RAY = nominal debt`.
/// When a user borrows N units at index I, scaled_debt is incremented by
/// `N × RAY ÷ I`. When index later grows to I', the nominal debt is
/// `scaled_debt × I' ÷ RAY` — interest has accrued without touching the position.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub market_id: MarketId,
    pub collateral_amount: u128,
    pub scaled_debt: u128,
}

impl Market {
    /// Construct a fresh market with zero state at the given starting block.
    /// Indices start at 1.0 (RAY). Reserves and totals at zero.
    #[must_use]
    pub fn new(
        id: MarketId,
        underlying: AssetId,
        collateral_type: AssetId,
        irm_params: IrmParams,
        liquidation_threshold: Bps,
        liquidation_bonus: Bps,
        reserve_factor: Bps,
        at_block: u64,
    ) -> Self {
        Market {
            id,
            underlying,
            collateral_type,
            total_supplied: 0,
            total_borrowed: 0,
            reserves: 0,
            borrow_index: Index::ONE,
            supply_index: Index::ONE,
            last_accrual_block: at_block,
            irm_params,
            liquidation_threshold,
            liquidation_bonus,
            reserve_factor,
        }
    }

    /// Utilization ratio in basis points: `total_borrowed / total_supplied`,
    /// capped at 10_000 (100%). Returns 0 if total_supplied is 0.
    ///
    /// Cap protects downstream IRM compute from exceeding 100% in edge cases
    /// where total_borrowed temporarily exceeds total_supplied (e.g., immediately
    /// after a bad-debt write-down).
    #[must_use]
    pub fn utilization_bps(&self) -> Bps {
        if self.total_supplied == 0 {
            return Bps::ZERO;
        }
        let scaled = self.total_borrowed.saturating_mul(10_000) / self.total_supplied;
        let capped = scaled.min(10_000);
        // Safe cast: capped is bounded at 10_000 which fits in u16.
        #[allow(clippy::cast_possible_truncation)]
        Bps(capped as u16)
    }
}

impl Position {
    /// Empty position in a market (no collateral, no debt).
    #[must_use]
    pub fn empty(market_id: MarketId) -> Self {
        Position { market_id, collateral_amount: 0, scaled_debt: 0 }
    }

    /// Nominal debt at the given borrow_index: `scaled_debt × borrow_index ÷ RAY`.
    /// Returns 0 if scaled_debt is 0.
    #[must_use]
    pub fn nominal_debt(&self, borrow_index: Index) -> u128 {
        if self.scaled_debt == 0 {
            return 0;
        }
        let product = self.scaled_debt.saturating_mul(borrow_index.0);
        product / Index::RAY
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_market() -> Market {
        Market::new(
            MarketId(0),
            AssetId(1), // ETH = underlying
            AssetId(0), // USDC = collateral
            IrmParams {
                base_rate_per_block: 0,
                slope_below_kink_per_block: Index::RAY / 10_000,
                slope_above_kink_per_block: Index::RAY / 1_000,
                kink_bps: Bps(8_000),
            },
            Bps(9_500), // LT 95%
            Bps(500),   // bonus 5%
            Bps(1_000), // reserve factor 10%
            0,
        )
    }

    #[test]
    fn market_starts_with_zero_totals_and_unit_indices() {
        let m = fresh_market();
        assert_eq!(m.total_supplied, 0);
        assert_eq!(m.total_borrowed, 0);
        assert_eq!(m.reserves, 0);
        assert_eq!(m.borrow_index, Index::ONE);
        assert_eq!(m.supply_index, Index::ONE);
        assert_eq!(m.last_accrual_block, 0);
    }

    #[test]
    fn utilization_is_zero_when_no_supply() {
        let m = fresh_market();
        assert_eq!(m.utilization_bps(), Bps::ZERO);
    }

    #[test]
    fn utilization_at_50_percent() {
        let mut m = fresh_market();
        m.total_supplied = 1_000;
        m.total_borrowed = 500;
        assert_eq!(m.utilization_bps(), Bps(5_000));
    }

    #[test]
    fn utilization_at_100_percent_exactly() {
        let mut m = fresh_market();
        m.total_supplied = 1_000;
        m.total_borrowed = 1_000;
        assert_eq!(m.utilization_bps(), Bps(10_000));
    }

    #[test]
    fn utilization_capped_at_100_percent() {
        let mut m = fresh_market();
        m.total_supplied = 1_000;
        m.total_borrowed = 2_000;
        assert_eq!(m.utilization_bps(), Bps(10_000));
    }

    #[test]
    fn empty_position_has_zero_state() {
        let p = Position::empty(MarketId(0));
        assert_eq!(p.collateral_amount, 0);
        assert_eq!(p.scaled_debt, 0);
        assert_eq!(p.nominal_debt(Index::ONE), 0);
    }

    #[test]
    fn nominal_debt_at_unit_index_equals_scaled() {
        let p = Position { market_id: MarketId(0), collateral_amount: 0, scaled_debt: 100 };
        assert_eq!(p.nominal_debt(Index::ONE), 100);
    }

    #[test]
    fn nominal_debt_scales_linearly_with_index() {
        let p = Position { market_id: MarketId(0), collateral_amount: 0, scaled_debt: 100 };
        let two_x = Index(Index::RAY * 2);
        assert_eq!(p.nominal_debt(two_x), 200);
        let half_x = Index(Index::RAY / 2);
        assert_eq!(p.nominal_debt(half_x), 50);
    }

    #[test]
    fn bps_fraction_conversion() {
        assert!((Bps::ONE_HUNDRED_PERCENT.as_fraction() - 1.0).abs() < f64::EPSILON);
        assert!((Bps(5_000).as_fraction() - 0.5).abs() < f64::EPSILON);
        assert!((Bps::ZERO.as_fraction() - 0.0).abs() < f64::EPSILON);
    }
}

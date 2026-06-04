//! ADR-010 Layer 3 — depositor-share haircut primitive.
//!
//! When the InsuranceFund depletes and the operator declares Layer 3,
//! supplier principal absorbs the residual `unfilled` amount pro-rata
//! across the market's `scaled_supply` positions. The mechanics happen
//! through the supplier-side accrual index: drop `supply_index` by
//! `(total_supplied − absorbed) / total_supplied`. Every position with
//! `scaled_supply > 0` sees its `nominal_supply(supply_index)` shrink
//! by the same percentage, without per-position writes.
//!
//! The bridge-owned implicit pool from `b1b5981`'s additive model
//! absorbs proportionally as well — its claim is also implicit in
//! `total_supplied`. This is the intended fairness model: every claim
//! against the market, depositor or bridge, takes the same haircut.
//!
//! ## Authorization
//!
//! This function is the pure-compute primitive that applies the
//! haircut once admission is granted. Verifying the operator's
//! signed [`SocializationDeclaration`](princeps_node::operator::SocializationDeclaration)
//! against the registered operator key is the caller's
//! responsibility (see `verify_socialization_declaration` in
//! `princeps-node`'s `operator` module). Mirrors how `repay` /
//! `withdraw_collateral` defer health checks to the bridge layer —
//! pure-compute crates don't see the things they don't need to.
//!
//! ## Chain-history event
//!
//! Callers that hold a `ChainHistoryStore` reference (the bin's
//! per-block hook owns one) should append a
//! `ChainEvent::Socialization` capturing the [`SocializationReport`]
//! after this function returns successfully. That's the audit trail
//! for the operation; the `unfilled`/`absorbed` fields here mirror
//! what goes into the event.

use crate::types::{Index, Market};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Report from a successful [`socialize_residual`] call. Captures the
/// before/after state for audit-log + chain-history-event consumers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SocializationReport {
    /// Market that was socialized.
    pub market_id: u32,
    /// Requested unfilled amount (the value passed in).
    pub requested_unfilled: u128,
    /// Actually socialized amount — equals `requested_unfilled` unless
    /// the request exceeded `total_supplied`, in which case it caps
    /// at `total_supplied` (you can't take more than exists).
    pub absorbed: u128,
    /// `supply_index` before the haircut.
    pub prior_supply_index: Index,
    /// `supply_index` after the haircut.
    pub new_supply_index: Index,
    /// `total_supplied` before the haircut.
    pub prior_total_supplied: u128,
    /// `total_supplied` after the haircut.
    pub new_total_supplied: u128,
}

/// Why a [`socialize_residual`] call was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum SocializationError {
    /// Caller invoked with `unfilled == 0`. Not a real socialization —
    /// the caller's gate (operator declaration) should not produce
    /// zero-amount declarations.
    #[error("unfilled is zero — caller should not invoke")]
    ZeroUnfilled,
    /// Market has nothing supplied, so there is nothing to socialize
    /// against. The caller should detect this earlier (Layer 1's halt
    /// already prevents drift into this state in normal flows).
    #[error("nothing to socialize against — market has zero total supply")]
    NoSupply,
}

/// Apply a Layer-3 socialization haircut to a market.
///
/// Math:
///
/// ```text
///   absorbed           = min(unfilled, total_supplied)
///   new_total_supplied = total_supplied − absorbed
///   new_supply_index   = supply_index × new_total_supplied / total_supplied
/// ```
///
/// The supply_index drop is computed BEFORE `total_supplied` is
/// updated in place, so the ratio uses the original `total_supplied`
/// (the value depositors had claims against pre-haircut). u128 math
/// throughout; saturating multiplication guards against pathological
/// inputs.
///
/// Per-account positions are repriced implicitly — each position's
/// `nominal_supply(new_supply_index)` is now proportionally smaller.
/// No per-position writes; the index is the only state that moves.
///
/// Returns a [`SocializationReport`] capturing before/after state,
/// suitable for logging + appending to chain history.
pub fn socialize_residual(
    market: &mut Market,
    unfilled: u128,
) -> Result<SocializationReport, SocializationError> {
    if unfilled == 0 {
        return Err(SocializationError::ZeroUnfilled);
    }
    if market.total_supplied == 0 {
        return Err(SocializationError::NoSupply);
    }

    let absorbed = unfilled.min(market.total_supplied);
    let prior_supply_index = market.supply_index;
    let prior_total_supplied = market.total_supplied;
    let new_total_supplied = prior_total_supplied - absorbed;

    // new_supply_index = supply_index × new_total_supplied / prior_total_supplied
    // u128 throughout. Saturating mul to keep the boundary-overflow
    // behavior obvious — at realistic chain values
    // (supply_index ~10^27, total_supplied ~10^18) the multiplication
    // fits in u128.
    let new_supply_index_raw = prior_supply_index
        .0
        .saturating_mul(new_total_supplied)
        / prior_total_supplied;
    let new_supply_index = Index(new_supply_index_raw);

    market.supply_index = new_supply_index;
    market.total_supplied = new_total_supplied;

    Ok(SocializationReport {
        market_id: market.id.0,
        requested_unfilled: unfilled,
        absorbed,
        prior_supply_index,
        new_supply_index,
        prior_total_supplied,
        new_total_supplied,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::position::supply;
    use crate::types::{AssetId, Bps, IrmParams, MarketId, Position};

    fn standard_market(total_supplied: u128, supply_index: Index) -> Market {
        let mut m = Market::new(
            MarketId(0),
            AssetId(1),
            AssetId(0),
            IrmParams {
                base_rate_per_block: 0,
                slope_below_kink_per_block: Index::RAY / 10_000,
                slope_above_kink_per_block: Index::RAY / 1_000,
                kink_bps: Bps(8_000),
            },
            Bps(9_500),
            Bps(500),
            Bps(1_000),
            0,
        );
        m.total_supplied = total_supplied;
        m.supply_index = supply_index;
        m
    }

    // ─── happy path ──────────────────────────────────────────────

    #[test]
    fn proportional_haircut_drops_index_by_correct_ratio() {
        // total_supplied = 1_000, unfilled = 100 → 10% haircut.
        // new_total_supplied = 900, new_supply_index = RAY * 900 / 1000 = 0.9 * RAY.
        let mut m = standard_market(1_000, Index::ONE);
        let report = socialize_residual(&mut m, 100).unwrap();
        assert_eq!(report.requested_unfilled, 100);
        assert_eq!(report.absorbed, 100);
        assert_eq!(report.prior_supply_index, Index::ONE);
        assert_eq!(report.new_supply_index.0, Index::RAY * 9 / 10);
        assert_eq!(report.prior_total_supplied, 1_000);
        assert_eq!(report.new_total_supplied, 900);
        // Market mutated in place.
        assert_eq!(m.total_supplied, 900);
        assert_eq!(m.supply_index.0, Index::RAY * 9 / 10);
    }

    #[test]
    fn full_drain_zeros_supply_side() {
        // unfilled == total_supplied → new_total_supplied = 0,
        // new_supply_index = 0 (everyone lost everything).
        let mut m = standard_market(500, Index::ONE);
        let report = socialize_residual(&mut m, 500).unwrap();
        assert_eq!(report.absorbed, 500);
        assert_eq!(report.new_total_supplied, 0);
        assert_eq!(report.new_supply_index.0, 0);
        assert_eq!(m.total_supplied, 0);
        assert_eq!(m.supply_index.0, 0);
    }

    #[test]
    fn cap_at_total_supplied_when_unfilled_exceeds() {
        // unfilled larger than total_supplied → absorbed caps at
        // total_supplied. Same result as full-drain.
        let mut m = standard_market(500, Index::ONE);
        let report = socialize_residual(&mut m, 10_000).unwrap();
        assert_eq!(report.requested_unfilled, 10_000);
        assert_eq!(report.absorbed, 500);
        assert_eq!(m.total_supplied, 0);
    }

    // ─── error cases ─────────────────────────────────────────────

    #[test]
    fn zero_unfilled_errors() {
        let mut m = standard_market(1_000, Index::ONE);
        assert_eq!(
            socialize_residual(&mut m, 0),
            Err(SocializationError::ZeroUnfilled)
        );
        // State unchanged.
        assert_eq!(m.total_supplied, 1_000);
        assert_eq!(m.supply_index, Index::ONE);
    }

    #[test]
    fn zero_total_supplied_errors() {
        let mut m = standard_market(0, Index::ONE);
        assert_eq!(
            socialize_residual(&mut m, 500),
            Err(SocializationError::NoSupply)
        );
        assert_eq!(m.total_supplied, 0);
        assert_eq!(m.supply_index, Index::ONE);
    }

    // ─── cross-field invariants ──────────────────────────────────

    #[test]
    fn depositor_nominal_supply_shrinks_proportionally() {
        // The whole point of the index mechanism: per-position state
        // is untouched, but each depositor's nominal_supply at the
        // post-haircut index is smaller by exactly the haircut ratio.
        let mut m = standard_market(1_000, Index::ONE);
        let mut depositor = Position::empty(MarketId(0));
        supply(&mut depositor, 200, m.supply_index).unwrap();
        let nominal_before = depositor.nominal_supply(m.supply_index);
        assert_eq!(nominal_before, 200);

        // 10% haircut.
        socialize_residual(&mut m, 100).unwrap();

        // Position state didn't move…
        assert_eq!(depositor.scaled_supply, 200);
        // …but nominal_supply at the new index is 10% smaller.
        let nominal_after = depositor.nominal_supply(m.supply_index);
        assert_eq!(nominal_after, 180);
    }

    #[test]
    fn borrow_side_untouched() {
        // Borrowers are not party to a Layer 3 haircut — their debt
        // is unchanged. borrow_index, total_borrowed, reserves all
        // stay put.
        let mut m = standard_market(1_000, Index::ONE);
        m.total_borrowed = 400;
        m.reserves = 50;
        let prior_borrow_index = m.borrow_index;
        let prior_borrowed = m.total_borrowed;
        let prior_reserves = m.reserves;

        socialize_residual(&mut m, 200).unwrap();

        assert_eq!(m.borrow_index, prior_borrow_index);
        assert_eq!(m.total_borrowed, prior_borrowed);
        assert_eq!(m.reserves, prior_reserves);
    }

    #[test]
    fn report_captures_pre_and_post_for_audit_trail() {
        // Audit-shaped — every field on the report matches one of the
        // (pre, post) sides, with consistent before/after labeling.
        let mut m = standard_market(2_000, Index(Index::RAY * 2));
        let report = socialize_residual(&mut m, 500).unwrap();
        // Pre fields match the market state at call time.
        assert_eq!(report.prior_total_supplied, 2_000);
        assert_eq!(report.prior_supply_index.0, Index::RAY * 2);
        // Post fields match the mutated market.
        assert_eq!(report.new_total_supplied, m.total_supplied);
        assert_eq!(report.new_supply_index, m.supply_index);
        // Math sanity: ratio absorbed/prior_total_supplied = 25%, so
        // new index should be 75% of prior.
        // new_supply_index = (Index::RAY * 2) * 1500 / 2000 = Index::RAY * 3 / 2
        assert_eq!(report.new_supply_index.0, Index::RAY * 3 / 2);
    }

    #[test]
    fn haircut_preserves_conservation_within_rounding() {
        // Conservation: sum of (per-position nominal_supply + bridge
        // implicit pool) should equal `total_supplied` both before
        // AND after the haircut. The bridge implicit pool is
        // `total_supplied - sum(per-position nominal_supply)` by
        // definition; both sides of the equation are scaled by the
        // same ratio, so the equation holds across the haircut.
        let mut m = standard_market(10_000, Index::ONE);
        let mut a = Position::empty(MarketId(0));
        let mut b = Position::empty(MarketId(0));
        supply(&mut a, 1_000, m.supply_index).unwrap();
        supply(&mut b, 2_000, m.supply_index).unwrap();
        let nominal_a_before = a.nominal_supply(m.supply_index);
        let nominal_b_before = b.nominal_supply(m.supply_index);
        let bridge_before = m.total_supplied - (nominal_a_before + nominal_b_before);
        // Sanity: at construction, depositors claim 3_000 and the
        // bridge implicit pool is 7_000.
        assert_eq!(bridge_before, 7_000);

        // 20% haircut.
        let report = socialize_residual(&mut m, 2_000).unwrap();
        assert_eq!(report.absorbed, 2_000);
        // Total dropped to 8_000.
        assert_eq!(m.total_supplied, 8_000);

        let nominal_a_after = a.nominal_supply(m.supply_index);
        let nominal_b_after = b.nominal_supply(m.supply_index);
        let bridge_after = m.total_supplied - (nominal_a_after + nominal_b_after);

        // Each claim shrunk by 20% within unit-step rounding.
        assert_eq!(nominal_a_after, 800);
        assert_eq!(nominal_b_after, 1_600);
        // The bridge's implicit pool also shrunk by 20%, from 7_000 → 5_600.
        assert_eq!(bridge_after, 5_600);
        // Conservation: bridge + depositors == total_supplied.
        assert_eq!(
            bridge_after + nominal_a_after + nominal_b_after,
            m.total_supplied,
        );
    }
}

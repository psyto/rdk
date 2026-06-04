//! Position-side pure operations. Stage 19b.
//!
//! Pure functions that mutate a single `Position` in place. The bridge calls
//! these from its precompile handlers (Stage 21) after validating caller
//! authority, oracle freshness, and portfolio-margin health.
//!
//! ## What these functions do NOT check
//!
//! - **Health factor**: withdraw / borrow could leave the position unhealthy.
//!   The bridge computes portfolio health (Stage 23) before calling these.
//! - **Market totals**: callers must update `Market::total_borrowed` /
//!   `total_supplied` separately. These functions don't see the Market;
//!   they only touch the Position.
//! - **Account balances**: callers (the bridge) move the underlying tokens
//!   in `Account` (from `rdk-clearing`) before calling.
//!
//! Keeping these functions narrow is intentional: pure, deterministic,
//! microsecond-fast unit-testable. All cross-cutting concerns belong
//! to the bridge layer.

use crate::types::{Index, Position};
use thiserror::Error;

/// All error conditions for position-level lending operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum LendingError {
    #[error("amount is zero")]
    ZeroAmount,
    #[error("borrow index is zero (uninitialized market?)")]
    ZeroIndex,
    #[error("amount too small — rounds to zero scaled debt at current index")]
    AmountTooSmall,
    #[error("amount too large — would overflow scaled-debt arithmetic")]
    AmountTooLarge,
    #[error("no outstanding debt to repay")]
    NoOutstandingDebt,
    #[error("insufficient collateral to withdraw")]
    InsufficientCollateral,
    #[error("no outstanding supply to withdraw")]
    NoOutstandingSupply,
}

/// Borrow `nominal_amount` of underlying against the position's collateral.
///
/// Converts to scaled debt at the current `borrow_index`: `scaled_delta = nominal × RAY ÷ borrow_index`.
/// Returns the scaled delta added so the caller can log it / mirror into
/// market-level scaled totals if it tracks those.
///
/// Health is NOT checked here — the bridge does portfolio health upstream.
pub fn borrow(
    position: &mut Position,
    nominal_amount: u128,
    borrow_index: Index,
) -> Result<u128, LendingError> {
    if nominal_amount == 0 {
        return Err(LendingError::ZeroAmount);
    }
    if borrow_index.0 == 0 {
        return Err(LendingError::ZeroIndex);
    }
    let product = nominal_amount
        .checked_mul(Index::RAY)
        .ok_or(LendingError::AmountTooLarge)?;
    let scaled_delta = product / borrow_index.0;
    if scaled_delta == 0 {
        return Err(LendingError::AmountTooSmall);
    }
    position.scaled_debt = position.scaled_debt.saturating_add(scaled_delta);
    Ok(scaled_delta)
}

/// Repay up to `nominal_amount` of outstanding debt.
///
/// If `nominal_amount` exceeds current debt, the actual repay is capped
/// at the current debt (no over-repayment). Returns the nominal amount
/// actually repaid (caller settles tokens accordingly).
///
/// Edge: due to floor division when computing `scaled_delta`, tiny dust
/// debt may remain after a "full repay". Bridge can offer a force-close
/// sweep in a later stage; not blocking for v0.
pub fn repay(
    position: &mut Position,
    nominal_amount: u128,
    borrow_index: Index,
) -> Result<u128, LendingError> {
    if nominal_amount == 0 {
        return Err(LendingError::ZeroAmount);
    }
    if borrow_index.0 == 0 {
        return Err(LendingError::ZeroIndex);
    }
    let current_nominal = position.nominal_debt(borrow_index);
    if current_nominal == 0 {
        return Err(LendingError::NoOutstandingDebt);
    }
    let actual_repaid = nominal_amount.min(current_nominal);

    let product = actual_repaid
        .checked_mul(Index::RAY)
        .ok_or(LendingError::AmountTooLarge)?;
    let scaled_delta = product / borrow_index.0;
    position.scaled_debt = position.scaled_debt.saturating_sub(scaled_delta);
    Ok(actual_repaid)
}

/// Deposit `amount` of collateral into the position.
pub fn deposit_collateral(position: &mut Position, amount: u128) -> Result<(), LendingError> {
    if amount == 0 {
        return Err(LendingError::ZeroAmount);
    }
    position.collateral_amount = position.collateral_amount.saturating_add(amount);
    Ok(())
}

/// Withdraw `amount` of collateral from the position.
///
/// Does NOT check post-withdraw health — the bridge runs portfolio health
/// (Stage 23) before calling. This function only enforces that the position
/// has enough collateral to physically debit.
pub fn withdraw_collateral(position: &mut Position, amount: u128) -> Result<(), LendingError> {
    if amount == 0 {
        return Err(LendingError::ZeroAmount);
    }
    if amount > position.collateral_amount {
        return Err(LendingError::InsufficientCollateral);
    }
    position.collateral_amount -= amount;
    Ok(())
}

/// Supply `nominal_amount` of underlying into the market.
///
/// Mirrors [`borrow`] on the supplier side. Converts to scaled supply at
/// the current `supply_index`: `scaled_delta = nominal × RAY ÷ supply_index`.
/// Returns the scaled delta so the caller can log it / mirror into
/// market-level scaled totals if it tracks those.
///
/// Liquidity availability (does the market have free reserves to repay
/// this supply if the supplier withdraws immediately?) is NOT checked
/// here — that is a market-level concern the bridge handles upstream.
pub fn supply(
    position: &mut Position,
    nominal_amount: u128,
    supply_index: Index,
) -> Result<u128, LendingError> {
    if nominal_amount == 0 {
        return Err(LendingError::ZeroAmount);
    }
    if supply_index.0 == 0 {
        return Err(LendingError::ZeroIndex);
    }
    let product = nominal_amount
        .checked_mul(Index::RAY)
        .ok_or(LendingError::AmountTooLarge)?;
    let scaled_delta = product / supply_index.0;
    if scaled_delta == 0 {
        return Err(LendingError::AmountTooSmall);
    }
    position.scaled_supply = position.scaled_supply.saturating_add(scaled_delta);
    Ok(scaled_delta)
}

/// Withdraw up to `nominal_amount` of supply from the position.
///
/// Mirrors [`repay`] on the supplier side. If `nominal_amount` exceeds the
/// supplier's current nominal supply, the actual withdraw is capped at
/// the current supply (no over-withdraw). Returns the nominal amount
/// actually withdrawn.
///
/// Market-level liquidity gating (is there enough free liquidity to
/// fulfill this withdraw without affecting borrower-side utilization?)
/// is NOT checked here — that is bridge-level work, mirroring how the
/// borrower-side `withdraw_collateral` defers health to the bridge.
pub fn withdraw_supply(
    position: &mut Position,
    nominal_amount: u128,
    supply_index: Index,
) -> Result<u128, LendingError> {
    if nominal_amount == 0 {
        return Err(LendingError::ZeroAmount);
    }
    if supply_index.0 == 0 {
        return Err(LendingError::ZeroIndex);
    }
    let current_nominal = position.nominal_supply(supply_index);
    if current_nominal == 0 {
        return Err(LendingError::NoOutstandingSupply);
    }
    let actual_withdrawn = nominal_amount.min(current_nominal);

    let product = actual_withdrawn
        .checked_mul(Index::RAY)
        .ok_or(LendingError::AmountTooLarge)?;
    let scaled_delta = product / supply_index.0;
    position.scaled_supply = position.scaled_supply.saturating_sub(scaled_delta);
    Ok(actual_withdrawn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::MarketId;

    fn fresh_position() -> Position {
        Position::empty(MarketId(0))
    }

    // --- borrow ---

    #[test]
    fn borrow_at_unit_index_scales_one_to_one() {
        let mut p = fresh_position();
        let scaled = borrow(&mut p, 100, Index::ONE).unwrap();
        assert_eq!(scaled, 100);
        assert_eq!(p.scaled_debt, 100);
        assert_eq!(p.nominal_debt(Index::ONE), 100);
    }

    #[test]
    fn borrow_at_double_index_halves_scaled() {
        let mut p = fresh_position();
        let two_x = Index(Index::RAY * 2);
        let scaled = borrow(&mut p, 100, two_x).unwrap();
        assert_eq!(scaled, 50);
        assert_eq!(p.scaled_debt, 50);
        // Nominal at current index = 50 × 2 = 100 (round-trip)
        assert_eq!(p.nominal_debt(two_x), 100);
    }

    #[test]
    fn borrow_accumulates_across_calls() {
        let mut p = fresh_position();
        borrow(&mut p, 100, Index::ONE).unwrap();
        borrow(&mut p, 50, Index::ONE).unwrap();
        assert_eq!(p.scaled_debt, 150);
    }

    #[test]
    fn borrow_zero_amount_errors() {
        let mut p = fresh_position();
        assert_eq!(borrow(&mut p, 0, Index::ONE), Err(LendingError::ZeroAmount));
        assert_eq!(p.scaled_debt, 0);
    }

    #[test]
    fn borrow_zero_index_errors() {
        let mut p = fresh_position();
        assert_eq!(borrow(&mut p, 100, Index(0)), Err(LendingError::ZeroIndex));
    }

    #[test]
    fn borrow_amount_too_small_errors() {
        // At a huge index, a tiny nominal amount rounds to zero scaled.
        let mut p = fresh_position();
        let huge = Index(u128::MAX);
        // amount=1, RAY=10^27, huge index → scaled = 10^27 / u128::MAX → 0
        assert_eq!(borrow(&mut p, 1, huge), Err(LendingError::AmountTooSmall));
    }

    #[test]
    fn borrow_amount_too_large_errors() {
        // nominal_amount × RAY overflows u128 when nominal > ~3.4 × 10^11
        let mut p = fresh_position();
        let too_big = u128::MAX / Index::RAY + 1;
        assert_eq!(borrow(&mut p, too_big, Index::ONE), Err(LendingError::AmountTooLarge));
    }

    // --- repay ---

    #[test]
    fn repay_full_debt_zeros_scaled() {
        let mut p = fresh_position();
        borrow(&mut p, 100, Index::ONE).unwrap();
        let repaid = repay(&mut p, 100, Index::ONE).unwrap();
        assert_eq!(repaid, 100);
        assert_eq!(p.scaled_debt, 0);
    }

    #[test]
    fn repay_partial_leaves_remainder() {
        let mut p = fresh_position();
        borrow(&mut p, 100, Index::ONE).unwrap();
        let repaid = repay(&mut p, 30, Index::ONE).unwrap();
        assert_eq!(repaid, 30);
        assert_eq!(p.scaled_debt, 70);
    }

    #[test]
    fn repay_over_amount_caps_at_current_debt() {
        let mut p = fresh_position();
        borrow(&mut p, 100, Index::ONE).unwrap();
        let repaid = repay(&mut p, 1_000_000, Index::ONE).unwrap();
        assert_eq!(repaid, 100); // capped
        assert_eq!(p.scaled_debt, 0);
    }

    #[test]
    fn repay_zero_amount_errors() {
        let mut p = fresh_position();
        borrow(&mut p, 100, Index::ONE).unwrap();
        assert_eq!(repay(&mut p, 0, Index::ONE), Err(LendingError::ZeroAmount));
    }

    #[test]
    fn repay_with_no_debt_errors() {
        let mut p = fresh_position();
        assert_eq!(repay(&mut p, 100, Index::ONE), Err(LendingError::NoOutstandingDebt));
    }

    // --- deposit_collateral ---

    #[test]
    fn deposit_collateral_accumulates() {
        let mut p = fresh_position();
        deposit_collateral(&mut p, 1_000).unwrap();
        deposit_collateral(&mut p, 500).unwrap();
        assert_eq!(p.collateral_amount, 1_500);
    }

    #[test]
    fn deposit_collateral_zero_errors() {
        let mut p = fresh_position();
        assert_eq!(deposit_collateral(&mut p, 0), Err(LendingError::ZeroAmount));
    }

    // --- withdraw_collateral ---

    #[test]
    fn withdraw_collateral_decrements() {
        let mut p = fresh_position();
        deposit_collateral(&mut p, 1_000).unwrap();
        withdraw_collateral(&mut p, 300).unwrap();
        assert_eq!(p.collateral_amount, 700);
    }

    #[test]
    fn withdraw_collateral_exact_balance_zeros() {
        let mut p = fresh_position();
        deposit_collateral(&mut p, 1_000).unwrap();
        withdraw_collateral(&mut p, 1_000).unwrap();
        assert_eq!(p.collateral_amount, 0);
    }

    #[test]
    fn withdraw_collateral_over_balance_errors() {
        let mut p = fresh_position();
        deposit_collateral(&mut p, 1_000).unwrap();
        assert_eq!(
            withdraw_collateral(&mut p, 1_001),
            Err(LendingError::InsufficientCollateral)
        );
        // State unchanged
        assert_eq!(p.collateral_amount, 1_000);
    }

    #[test]
    fn withdraw_collateral_zero_errors() {
        let mut p = fresh_position();
        deposit_collateral(&mut p, 1_000).unwrap();
        assert_eq!(withdraw_collateral(&mut p, 0), Err(LendingError::ZeroAmount));
    }

    // --- supply ---

    #[test]
    fn supply_at_unit_index_scales_one_to_one() {
        let mut p = fresh_position();
        let scaled = supply(&mut p, 100, Index::ONE).unwrap();
        assert_eq!(scaled, 100);
        assert_eq!(p.scaled_supply, 100);
        assert_eq!(p.nominal_supply(Index::ONE), 100);
    }

    #[test]
    fn supply_at_double_index_halves_scaled() {
        let mut p = fresh_position();
        let two_x = Index(Index::RAY * 2);
        let scaled = supply(&mut p, 100, two_x).unwrap();
        assert_eq!(scaled, 50);
        assert_eq!(p.scaled_supply, 50);
        assert_eq!(p.nominal_supply(two_x), 100);
    }

    #[test]
    fn supply_accumulates_across_calls() {
        let mut p = fresh_position();
        supply(&mut p, 100, Index::ONE).unwrap();
        supply(&mut p, 50, Index::ONE).unwrap();
        assert_eq!(p.scaled_supply, 150);
    }

    #[test]
    fn supply_zero_amount_errors() {
        let mut p = fresh_position();
        assert_eq!(supply(&mut p, 0, Index::ONE), Err(LendingError::ZeroAmount));
        assert_eq!(p.scaled_supply, 0);
    }

    #[test]
    fn supply_zero_index_errors() {
        let mut p = fresh_position();
        assert_eq!(supply(&mut p, 100, Index(0)), Err(LendingError::ZeroIndex));
    }

    #[test]
    fn supply_amount_too_small_errors() {
        let mut p = fresh_position();
        let huge = Index(u128::MAX);
        assert_eq!(supply(&mut p, 1, huge), Err(LendingError::AmountTooSmall));
    }

    #[test]
    fn supply_amount_too_large_errors() {
        let mut p = fresh_position();
        let too_big = u128::MAX / Index::RAY + 1;
        assert_eq!(supply(&mut p, too_big, Index::ONE), Err(LendingError::AmountTooLarge));
    }

    #[test]
    fn supply_does_not_touch_debt_fields() {
        // Cross-field invariant: supply mutates only scaled_supply.
        let mut p = fresh_position();
        deposit_collateral(&mut p, 5_000).unwrap();
        borrow(&mut p, 1_000, Index::ONE).unwrap();
        supply(&mut p, 200, Index::ONE).unwrap();
        assert_eq!(p.scaled_supply, 200);
        assert_eq!(p.scaled_debt, 1_000);
        assert_eq!(p.collateral_amount, 5_000);
    }

    // --- withdraw_supply ---

    #[test]
    fn withdraw_supply_full_zeros_scaled() {
        let mut p = fresh_position();
        supply(&mut p, 100, Index::ONE).unwrap();
        let withdrawn = withdraw_supply(&mut p, 100, Index::ONE).unwrap();
        assert_eq!(withdrawn, 100);
        assert_eq!(p.scaled_supply, 0);
    }

    #[test]
    fn withdraw_supply_partial_leaves_remainder() {
        let mut p = fresh_position();
        supply(&mut p, 100, Index::ONE).unwrap();
        let withdrawn = withdraw_supply(&mut p, 30, Index::ONE).unwrap();
        assert_eq!(withdrawn, 30);
        assert_eq!(p.scaled_supply, 70);
    }

    #[test]
    fn withdraw_supply_over_amount_caps_at_current_supply() {
        let mut p = fresh_position();
        supply(&mut p, 100, Index::ONE).unwrap();
        let withdrawn = withdraw_supply(&mut p, 1_000_000, Index::ONE).unwrap();
        assert_eq!(withdrawn, 100);
        assert_eq!(p.scaled_supply, 0);
    }

    #[test]
    fn withdraw_supply_zero_amount_errors() {
        let mut p = fresh_position();
        supply(&mut p, 100, Index::ONE).unwrap();
        assert_eq!(withdraw_supply(&mut p, 0, Index::ONE), Err(LendingError::ZeroAmount));
    }

    #[test]
    fn withdraw_supply_with_no_supply_errors() {
        let mut p = fresh_position();
        assert_eq!(
            withdraw_supply(&mut p, 100, Index::ONE),
            Err(LendingError::NoOutstandingSupply),
        );
    }

    #[test]
    fn withdraw_supply_zero_index_errors() {
        let mut p = fresh_position();
        supply(&mut p, 100, Index::ONE).unwrap();
        assert_eq!(
            withdraw_supply(&mut p, 50, Index(0)),
            Err(LendingError::ZeroIndex),
        );
    }

    #[test]
    fn supply_then_grown_index_yields_more_than_supplied() {
        // Mirror of the borrower-side accrual contract: after supply at
        // index_0, if supply_index grows to index_1 > index_0, the
        // supplier's nominal supply is proportionally larger — the index
        // mechanism captures yield without touching the position.
        let mut p = fresh_position();
        supply(&mut p, 1_000, Index::ONE).unwrap();
        assert_eq!(p.nominal_supply(Index::ONE), 1_000);
        let grown = Index(Index::RAY * 11 / 10); // +10%
        assert_eq!(p.nominal_supply(grown), 1_100);
    }
}

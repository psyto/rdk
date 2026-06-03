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
}

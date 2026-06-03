//! `princeps-portfolio` — unified portfolio risk for the DeFi prime broker.
//!
//! ## What this crate is
//!
//! The pure-math kernel of Princeps's cross-margin engine — the prime
//! broker thesis. Given a normalized view of an account's perp position
//! (collateral, unrealized PnL, IM requirement) and lending positions
//! (adjusted collateral value, debt value), compute the **single
//! portfolio health metric** that drives:
//!
//! - Liquidation triggers (`is_healthy`)
//! - Free-equity / buying power (`compute_free_equity`)
//! - Net equity (`compute_net_equity`) for reporting / observability
//!
//! ## What this crate is NOT
//!
//! - **A position aggregator.** The bridge (`princeps-evm`) walks its
//!   own per-account state, normalizes everything to a common quote unit
//!   (USDC at v0), and assembles a [`PortfolioInputs`]. This crate only
//!   does the arithmetic on already-normalized inputs.
//! - **A price oracle integration.** Caller pulls prices, applies them,
//!   and hands the resulting values in. Keeps this crate pure +
//!   microsecond-testable.
//! - **A multi-asset normalization layer.** v0 assumes all values arrive
//!   pre-normalized to one quote unit. Multi-currency support (with
//!   FX-rate considerations) is a v1 concern.
//!
//! ## The prime broker insight
//!
//! In TradFi prime brokerage, a single account holds all positions and
//! has a single margin pool. A perp profit can offset a lending margin
//! call; lending collateral can back a perp position. Cross-margin
//! eliminates the artificial siloing that forces capital to be
//! over-provisioned across separate sub-accounts.
//!
//! v0 Princeps applies this to the lending + perp pair. v1+ extends to
//! options, structured products, and beyond — but the math stays the
//! same shape: sum equities, subtract obligations, compare to zero.

use serde::{Deserialize, Serialize};

/// Pre-normalized inputs to the portfolio-health computation.
///
/// All fields are in a single quote unit (USDC at v0). The bridge layer
/// is responsible for applying prices + per-market liquidation thresholds
/// BEFORE assembling this struct.
///
/// Signed `i128` throughout — perp PnL can be negative; collateral can
/// theoretically be negative under extreme drawdowns; the portfolio math
/// works whether the account is in profit or in deficit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PortfolioInputs {
    /// Perp account's quote-currency collateral balance (signed).
    /// Sourced from `rdk_clearing::Account::collateral`.
    pub perp_collateral: i128,

    /// Perp account's unrealized PnL at the current mark (signed).
    /// Sourced from `rdk_clearing::unrealized_pnl(account, mark)`.
    pub perp_unrealized_pnl: i128,

    /// Perp account's initial-margin requirement at current mark.
    /// Sourced from `rdk_clearing::initial_margin_requirement_at(...)`.
    pub perp_im_req: i128,

    /// Total quote-currency value of lending collateral, AFTER applying
    /// per-market liquidation thresholds.
    ///
    /// For each lending position the bridge computes:
    /// `collateral_amount × collateral_price × LT_bps / 10_000`
    /// and sums them. The LT haircut is applied here (not later) so the
    /// math below is straightforward subtraction.
    pub lending_adjusted_collateral_value: i128,

    /// Total quote-currency outstanding nominal debt across all lending
    /// positions.
    ///
    /// For each lending position the bridge computes:
    /// `nominal_debt(borrow_index) × debt_price` and sums them.
    pub lending_debt_value: i128,
}

/// Compute the account's **free equity** — how much value can be
/// withdrawn / used as buying power before any margin constraint is hit.
///
/// Formula:
/// ```text
///   perp_free    = (perp_collateral + perp_unrealized_pnl) - perp_im_req
///   lending_free = lending_adjusted_collateral_value - lending_debt_value
///   free_equity  = perp_free + lending_free
/// ```
///
/// Returns signed `i128`:
/// - `>= 0`: the account is healthy; the value represents extractable equity
/// - `< 0`: the account is liquidatable; the negative value is the deficit
///
/// Saturating arithmetic; no panics on extreme inputs.
#[must_use]
pub fn compute_free_equity(inputs: &PortfolioInputs) -> i128 {
    let perp_equity = inputs
        .perp_collateral
        .saturating_add(inputs.perp_unrealized_pnl);
    let perp_free = perp_equity.saturating_sub(inputs.perp_im_req);
    let lending_free = inputs
        .lending_adjusted_collateral_value
        .saturating_sub(inputs.lending_debt_value);
    perp_free.saturating_add(lending_free)
}

/// Is the portfolio healthy (free equity >= 0)?
///
/// Convenience wrapper around [`compute_free_equity`]. Returns `true`
/// when the account is at or above the liquidation threshold across the
/// combined perp + lending positions.
#[must_use]
pub fn is_healthy(inputs: &PortfolioInputs) -> bool {
    compute_free_equity(inputs) >= 0
}

/// Compute net equity (total assets - total liabilities), ignoring
/// margin requirements. Useful for reporting / observability (e.g.,
/// "net worth on platform") but NOT for liquidation decisions —
/// use [`compute_free_equity`] for that.
///
/// Formula:
/// ```text
///   perp_equity    = perp_collateral + perp_unrealized_pnl
///   lending_equity = lending_adjusted_collateral_value - lending_debt_value
///   net_equity     = perp_equity + lending_equity
/// ```
#[must_use]
pub fn compute_net_equity(inputs: &PortfolioInputs) -> i128 {
    let perp_equity = inputs
        .perp_collateral
        .saturating_add(inputs.perp_unrealized_pnl);
    let lending_equity = inputs
        .lending_adjusted_collateral_value
        .saturating_sub(inputs.lending_debt_value);
    perp_equity.saturating_add(lending_equity)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zero() -> PortfolioInputs {
        PortfolioInputs::default()
    }

    #[test]
    fn empty_portfolio_has_zero_free_equity_and_is_healthy() {
        let p = zero();
        assert_eq!(compute_free_equity(&p), 0);
        assert!(is_healthy(&p));
        assert_eq!(compute_net_equity(&p), 0);
    }

    // ===== Perp-only scenarios =====

    #[test]
    fn perp_only_healthy_with_excess_collateral() {
        // Collateral 1000, no unrealized PnL, IM_req 100 → free = 900
        let p = PortfolioInputs {
            perp_collateral: 1_000,
            perp_im_req: 100,
            ..zero()
        };
        assert_eq!(compute_free_equity(&p), 900);
        assert!(is_healthy(&p));
    }

    #[test]
    fn perp_only_unhealthy_when_im_req_exceeds_equity() {
        // Collateral 100, no PnL, IM_req 200 → free = -100 (liquidatable)
        let p = PortfolioInputs {
            perp_collateral: 100,
            perp_im_req: 200,
            ..zero()
        };
        assert_eq!(compute_free_equity(&p), -100);
        assert!(!is_healthy(&p));
    }

    #[test]
    fn perp_only_profit_increases_free_equity() {
        // Collateral 500, uPnL +200, IM_req 100 → free = 600
        let p = PortfolioInputs {
            perp_collateral: 500,
            perp_unrealized_pnl: 200,
            perp_im_req: 100,
            ..zero()
        };
        assert_eq!(compute_free_equity(&p), 600);
        assert!(is_healthy(&p));
    }

    #[test]
    fn perp_only_loss_can_trigger_unhealthy() {
        // Collateral 500, uPnL -400, IM_req 200 → equity 100, free = -100
        let p = PortfolioInputs {
            perp_collateral: 500,
            perp_unrealized_pnl: -400,
            perp_im_req: 200,
            ..zero()
        };
        assert_eq!(compute_free_equity(&p), -100);
        assert!(!is_healthy(&p));
    }

    // ===== Lending-only scenarios =====

    #[test]
    fn lending_only_healthy_when_collateral_covers_debt() {
        // adjusted_collateral 950, debt 500 → free = 450
        let p = PortfolioInputs {
            lending_adjusted_collateral_value: 950,
            lending_debt_value: 500,
            ..zero()
        };
        assert_eq!(compute_free_equity(&p), 450);
        assert!(is_healthy(&p));
    }

    #[test]
    fn lending_only_unhealthy_when_debt_exceeds_adjusted_collateral() {
        // adjusted_collateral 800, debt 1000 → free = -200
        let p = PortfolioInputs {
            lending_adjusted_collateral_value: 800,
            lending_debt_value: 1_000,
            ..zero()
        };
        assert_eq!(compute_free_equity(&p), -200);
        assert!(!is_healthy(&p));
    }

    // ===== Cross-margin: the prime broker scenarios =====

    #[test]
    fn cross_margin_perp_profit_covers_lending_deficit() {
        // Lending side underwater by 200; perp profit of 300 covers it.
        // Total free = -200 + 300 = 100 → healthy
        let p = PortfolioInputs {
            perp_collateral: 0,
            perp_unrealized_pnl: 300,
            perp_im_req: 0,
            lending_adjusted_collateral_value: 800,
            lending_debt_value: 1_000,
        };
        assert_eq!(compute_free_equity(&p), 100);
        assert!(is_healthy(&p));
    }

    #[test]
    fn cross_margin_lending_collateral_covers_perp_margin_shortfall() {
        // Perp side: collateral 100, no PnL, IM_req 200 → perp_free = -100
        // Lending side: 500 adjusted collateral, 0 debt → lending_free = 500
        // Total free = -100 + 500 = 400 → healthy
        let p = PortfolioInputs {
            perp_collateral: 100,
            perp_im_req: 200,
            lending_adjusted_collateral_value: 500,
            ..zero()
        };
        assert_eq!(compute_free_equity(&p), 400);
        assert!(is_healthy(&p));
    }

    #[test]
    fn both_sides_underwater_is_severely_unhealthy() {
        // Perp underwater by 300, lending underwater by 500 → free = -800
        let p = PortfolioInputs {
            perp_collateral: 50,
            perp_unrealized_pnl: -200,
            perp_im_req: 150,
            lending_adjusted_collateral_value: 200,
            lending_debt_value: 700,
        };
        assert_eq!(compute_free_equity(&p), -800);
        assert!(!is_healthy(&p));
    }

    #[test]
    fn cross_margin_demonstrates_prime_broker_benefit() {
        // Same account viewed siloed vs unified:
        //   Siloed perp: free = -100 (liquidatable in perp-only world)
        //   Siloed lending: free = +400 (healthy in lending-only world)
        //   Unified: free = +300 (healthy)
        // The prime broker thesis: don't liquidate the perp position when
        // there's lending collateral that could back it.
        let p = PortfolioInputs {
            perp_collateral: 100,
            perp_im_req: 200,
            lending_adjusted_collateral_value: 500,
            lending_debt_value: 100,
            ..zero()
        };
        assert_eq!(compute_free_equity(&p), 300);
        assert!(is_healthy(&p));
        // Net equity (asset-liability view): perp 100 + lending (500-100) = 500
        assert_eq!(compute_net_equity(&p), 500);
    }

    // ===== Edge cases =====

    #[test]
    fn saturating_arithmetic_does_not_panic_at_extreme_values() {
        let p = PortfolioInputs {
            perp_collateral: i128::MAX / 2,
            perp_unrealized_pnl: i128::MAX / 2,
            perp_im_req: 0,
            lending_adjusted_collateral_value: i128::MAX / 2,
            lending_debt_value: 0,
        };
        let free = compute_free_equity(&p);
        assert_eq!(free, i128::MAX); // saturated
    }

    #[test]
    fn extreme_negative_pnl_saturates_to_min() {
        let p = PortfolioInputs {
            perp_collateral: 0,
            perp_unrealized_pnl: i128::MIN,
            perp_im_req: 0,
            ..zero()
        };
        let free = compute_free_equity(&p);
        // perp_equity = 0 + i128::MIN saturates to MIN; perp_free = MIN - 0 = MIN
        assert_eq!(free, i128::MIN);
        assert!(!is_healthy(&p));
    }

    #[test]
    fn net_equity_distinct_from_free_equity_when_im_req_nonzero() {
        // The two metrics differ by perp_im_req:
        //   net_equity ignores margin requirement
        //   free_equity subtracts it
        let p = PortfolioInputs {
            perp_collateral: 1_000,
            perp_im_req: 300,
            ..zero()
        };
        assert_eq!(compute_free_equity(&p), 700);
        assert_eq!(compute_net_equity(&p), 1_000);
    }
}

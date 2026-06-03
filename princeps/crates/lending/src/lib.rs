//! `princeps-lending` — lending market state, position bookkeeping, and pure compute primitives.
//!
//! ## What this crate is
//!
//! The pure state machine for the lending side of Princeps:
//! - per-market state (reserves, total borrowed/supplied, IRM params, indices)
//! - per-position state (collateral, scaled debt)
//! - IRM compute (utilization → borrow rate) — Stage 19c
//! - Health factor compute (collateral × LT vs debt) — Stage 19d
//! - Interest accrual (per-block, index-based) — Stage 19e
//!
//! ## What this crate is not
//!
//! No I/O. The bridge (`princeps-evm` / `bin/princeps`) owns the live state and routes
//! mutations through this crate's `apply_*` functions. Same pattern as `rdk-clob`,
//! `rdk-funding`, `rdk-liquidation`, `rdk-clearing`.
//!
//! ## Index-based accounting
//!
//! Following Aave's RAY-scaled index model: each position stores a `scaled_debt` value;
//! nominal debt at any moment is `scaled_debt × borrow_index ÷ RAY`. Per-block interest
//! accrual updates the global index once; per-position math stays O(1).
//!
//! See `docs/plans/v0-lending.md` for the full v0 build plan.

pub mod accrual;
pub mod health;
pub mod irm;
pub mod position;
pub mod types;

pub use accrual::{accrue_interest, InterestAccrualReport};
pub use health::{compute_health_factor, compute_health_factor_from_values, is_liquidatable};
pub use irm::compute_borrow_rate;
pub use position::{borrow, deposit_collateral, repay, withdraw_collateral, LendingError};
pub use types::{AssetId, Bps, Index, IrmParams, Market, MarketId, Position};

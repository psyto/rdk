//! `rdk-vault` — share-based vault primitive.
//!
//! Pure compute + small state machine, same architectural shape as
//! `rdk-funding`, `rdk-liquidation`, and `rdk-oracle`:
//! `types → compute → state`. Every validator must arrive at the same
//! `(shares_minted, assets_returned)` from the same inputs, so all
//! arithmetic is integer + saturating and the share/asset conversion
//! is deterministic.
//!
//! ### What a vault does
//!
//! A vault pools depositor collateral into one fungible share token,
//! and lets a manager run a trading strategy over the pooled capital.
//! Depositors mint shares at the current NAV; their share count then
//! tracks the manager's `PnL` pro-rata. Withdrawers burn shares and
//! receive proportional assets. The vault never sees individual
//! positions — it sees only its current `total_assets` (collateral +
//! marked `PnL`) and `total_shares` (sum of outstanding mints).
//!
//! ### Stage 12 scope
//!
//! - [`compute::assets_to_shares`] — deterministic share-mint math.
//! - [`compute::shares_to_assets`] — deterministic share-burn math.
//! - [`compute::share_price_bps`] — NAV per share in basis points.
//! - [`state::VaultState`] — `(total_shares, total_assets)` state
//!   machine with `deposit`, `withdraw`, `mark_to_market` operations.
//!
//! ### What `mark_to_market` is for
//!
//! The vault doesn't compute its own `PnL`. The bridge layer holds
//! the vault's actual perp positions, runs them through
//! `rdk_liquidation::margin_ratio` /
//! `rdk_funding::compute_premium` each block, and calls
//! [`state::VaultState::mark_to_market`] with the updated
//! `total_assets`. The vault then prices subsequent deposits and
//! withdrawals against the marked value. No shares move during
//! `mark_to_market`; only the per-share value changes.
//!
//! ### Out of scope (future work)
//!
//! - **Performance fees / high-water marks.** Production vaults take
//!   a manager fee (e.g., 20% of `PnL` above the high-water mark).
//!   Stage 12 v0 is fee-free; the manager monetizes elsewhere or
//!   extends `VaultParams` + a HWM-tracking field.
//! - **Lock-up windows.** Real vaults often impose a cooling period
//!   between deposit and withdrawal. Add a `last_deposit_at` field
//!   per depositor (Stage 12c) when needed.
//! - **Per-depositor accounting.** Stage 12 tracks only the aggregate
//!   `total_shares`. The bridge layer maintains
//!   `BTreeMap<AccountId, Shares>` if it needs per-depositor balances.
//!   This keeps the crate boundary tight: vault is the share math,
//!   not the bookkeeping table.
//! - **Multi-vault / cross-vault.** One [`VaultState`] = one vault.
//!   Bridge owns the `{VaultId, VaultState}` map.
//! - **Manager identity / authorization.** Stage 12 v0 has no notion
//!   of "manager". The bridge enforces who can call which method.
//!
//! ### Why fixed-point integers, not floats
//!
//! Same answer as the other rdk crates: consensus determinism.
//! Every validator must arrive at the same `shares_minted` from the
//! same inputs, and float arithmetic varies bit-for-bit across
//! compilers and CPUs.

pub mod compute;
pub mod state;
pub mod types;

pub use compute::{assets_to_shares, share_price_bps, shares_to_assets};
pub use state::VaultState;
pub use types::{
    Assets, DepositError, DepositResult, Shares, VaultParams, WithdrawError, WithdrawResult,
    SHARE_PRICE_BPS_SCALE,
};

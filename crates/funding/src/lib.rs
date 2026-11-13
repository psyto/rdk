//! `rdk-funding` — funding-rate state machine.
//!
//! Pure state machine: no I/O, no async, no networking. Funding is applied
//! deterministically on a fixed cadence (see [`FundingClock`]); every tick is
//! a pure function over `(now, mark, index, positions)` → settlements.
//!
//! ### Hyperliquid-shape funding, in one paragraph
//!
//! Perpetual contracts don't expire, so the mark price can drift arbitrarily
//! from the spot ("index") price. Funding payments push it back: when mark >
//! index (longs are overpaying), longs pay shorts; when mark < index, shorts
//! pay longs. The premium `(mark - index) / index` is divided by a divisor
//! (HL: 8, because HL quotes an 8-hour rate and pays one-eighth of it every
//! hour) to derive a per-interval rate, capped at a network-set absolute max.
//! HL also adds a fixed interest term (0.01% per 8 hours) through a clamp;
//! this crate leaves that term out and uses the premium alone. At each tick
//! every account with an open position settles `position_size * mark * rate`
//! in quote currency.
//!
//! Integration with the rest of rdk happens at the EVM bridge: settlement
//! deltas become balance updates that the bridge bundles into payloads. That
//! integration lives in `crates/evm/`; the rate math and tick gating are here.

pub mod clock;
pub mod compute;
pub mod types;

pub use clock::{FundingClock, FundingTick};
pub use compute::{apply_funding, compute_premium, compute_rate};
pub use types::{
    AccountId,
    FundingParams, FundingRate, IndexPrice, MarkPrice, Notional, Position, PositionSize,
    Premium, Settlement, RATE_SCALE,
};

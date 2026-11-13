//! Core types for the funding state machine.
//!
//! Pure data — no I/O, no allocation beyond what's needed for settlements.
//! Every type is `Copy`-friendly (or, in the case of `Position`, `Clone +
//! Copy`) so callers can pass snapshots without lifetime gymnastics.
//!
//! ### Why fixed-point integers, not floats
//!
//! Consensus determinism — every validator must compute the *same* funding
//! rate from the *same* inputs. Float arithmetic gives different bit patterns
//! across compilers and CPUs (FMA, rounding mode, denormal handling); the
//! moment two validators disagree on a single LSB they fork. We use signed
//! integers scaled by [`RATE_SCALE`] (parts-per-billion) for rates and
//! premiums, and a separate `Notional` type for quote-currency deltas.

pub use rdk_units::{AccountId, IndexPrice, MarkPrice, Notional, PositionSize};

/// Scale factor for [`FundingRate`] and [`Premium`]. A raw value of
/// `RATE_SCALE` represents `1.0` (i.e., 100%). With `1e9` we get 9 decimal
/// digits of precision — more than enough for funding rates that typically
/// sit in the ±0.01% to ±0.05% per interval band.
pub const RATE_SCALE: i64 = 1_000_000_000;

/// Premium = `(mark - index) / index`, scaled by [`RATE_SCALE`].
///
/// Sign convention: positive when mark > index (longs are overpaying,
/// funding will be positive → longs pay shorts).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Premium(pub i64);

/// Per-interval funding rate. Same scale as [`Premium`]; positive means
/// longs pay shorts. A rate of `RATE_SCALE / 100` = 1% per interval.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FundingRate(pub i64);

/// A single account's net position on the market. The funding state machine
/// treats positions as a per-tick *snapshot* — it never owns or mutates
/// them. The owning layer (vault / clearing) is responsible for tracking
/// `Position` over time and producing snapshots at each tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Position {
    pub account: AccountId,
    pub size: PositionSize,
}

/// Output of applying a funding rate to one position. The bridge layer
/// translates these into balance updates against each account's quote
/// balance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settlement {
    pub account: AccountId,
    pub delta: Notional,
}

/// Network parameters that govern funding cadence and magnitude.
///
/// `divisor` turns the premium into a per-interval rate: HL quotes funding as
/// an 8-hour rate and pays one-eighth of it every hour, so with a 1-hour
/// interval `premium / 8` is the hourly rate. A higher divisor gives a smaller
/// rate per tick.
///
/// `rate_cap` is the absolute maximum |rate| per interval. Production
/// networks set this to bound the worst-case payment an extreme oracle
/// dislocation can produce. Zero `rate_cap` disables funding entirely.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FundingParams {
    pub interval_secs: u64,
    pub rate_cap: FundingRate,
    pub divisor: u32,
}

impl FundingParams {
    /// Hyperliquid-style defaults: 1-hour interval, 8× divisor (the 8-hour
    /// rate paid hourly), ±0.5%/hour cap, as HL's funding docs state
    /// (<https://hyperliquid.gitbook.io/hyperliquid-docs/trading/funding>,
    /// checked 2026-11-13). HL's interest term is not modelled (see the crate
    /// docs).
    #[must_use]
    pub const fn hyperliquid_default() -> Self {
        Self {
            interval_secs: 3600,
            // 0.5% per interval = 5_000_000 ppb (since 0.005 × 1e9 = 5e6).
            rate_cap: FundingRate(5_000_000),
            divisor: 8,
        }
    }
}

//! Shared unit types for the rdk trading crates.
//!
//! Accounts, prices, quantities and signed amounts, each a distinct newtype so
//! an orderbook price can't be fed where a mark or index price is expected.
//! Every other rdk trading crate depends on this one and nothing heavier, so
//! using `rdk-oracle` or `rdk-liquidation` alone does not pull in the CLOB or
//! the funding engine. `rdk-clob` and `rdk-funding` re-export these types, so
//! `rdk_clob::AccountId` and `rdk_funding::MarkPrice` keep working and name the
//! same types as here.

use core::fmt;

use serde::{Deserialize, Serialize};

/// Account identifier. Opaque to the CLOB; chain integration maps these to
/// EVM addresses, validator addresses, or whatever the chain uses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AccountId(pub u64);

/// Price in minor units. For a USDC market, `Price(1_000_000) = $1.00`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Price(pub u64);

/// Quantity in minor units.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Qty(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    #[must_use]
    pub const fn opposite(self) -> Self {
        match self {
            Self::Buy => Self::Sell,
            Self::Sell => Self::Buy,
        }
    }
}

/// Mark price in minor units. Same scale convention as [`Price`], but a
/// distinct type so callers can't accidentally feed an orderbook price into
/// the funding math where an index/oracle price is expected.
///
/// `MarkPrice` is a single u64 not a signed-fixed-point, because prices are
/// always positive (zero or negative price would be a system invariant
/// violation handled upstream, not here).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct MarkPrice(pub u64);

/// Index price (off-chain oracle reference). Same scale as [`MarkPrice`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct IndexPrice(pub u64);

/// Signed position size in base units. Positive = long, negative = short,
/// zero = flat.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PositionSize(pub i64);

/// Signed quote-currency delta. Positive = account receives, negative =
/// account pays.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Notional(pub i64);

impl fmt::Display for Price {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl fmt::Display for Qty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

use core::fmt;

use informalsystems_malachitebft_core_types::Height;
use serde::{Deserialize, Serialize};

/// Block height — a monotonic u64 counter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PrincepsHeight(pub u64);

impl fmt::Display for PrincepsHeight {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Height for PrincepsHeight {
    const ZERO: Self = PrincepsHeight(0);
    const INITIAL: Self = PrincepsHeight(1);

    fn increment_by(&self, n: u64) -> Self {
        PrincepsHeight(self.0.saturating_add(n))
    }

    fn decrement_by(&self, n: u64) -> Option<Self> {
        self.0.checked_sub(n).map(PrincepsHeight)
    }

    fn as_u64(&self) -> u64 {
        self.0
    }
}

use informalsystems_malachitebft_core_types::{Proposal, Round};
use serde::{Deserialize, Serialize};

use crate::context::PrincepsContext;
use crate::types::{PrincepsAddress, PrincepsHeight, PrincepsValue};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrincepsProposal {
    pub height: PrincepsHeight,
    pub round: Round,
    pub value: PrincepsValue,
    pub pol_round: Round,
    pub address: PrincepsAddress,
}

impl Proposal<PrincepsContext> for PrincepsProposal {
    fn height(&self) -> PrincepsHeight {
        self.height
    }

    fn round(&self) -> Round {
        self.round
    }

    fn value(&self) -> &PrincepsValue {
        &self.value
    }

    fn take_value(self) -> PrincepsValue {
        self.value
    }

    fn pol_round(&self) -> Round {
        self.pol_round
    }

    fn validator_address(&self) -> &PrincepsAddress {
        &self.address
    }
}

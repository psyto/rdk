use informalsystems_malachitebft_core_types::{
    NilOrVal, Round, SignedExtension, VoteType, Vote as VoteTrait,
};
use rdk_types::BlockHash;
use serde::{Deserialize, Serialize};

use crate::context::PrincepsContext;
use crate::types::{PrincepsAddress, PrincepsHeight};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PrincepsVote {
    pub height: PrincepsHeight,
    pub round: Round,
    pub value_id: NilOrVal<BlockHash>,
    pub vote_type: VoteType,
    pub address: PrincepsAddress,
}

impl VoteTrait<PrincepsContext> for PrincepsVote {
    fn height(&self) -> PrincepsHeight {
        self.height
    }

    fn round(&self) -> Round {
        self.round
    }

    fn value(&self) -> &NilOrVal<BlockHash> {
        &self.value_id
    }

    fn take_value(self) -> NilOrVal<BlockHash> {
        self.value_id
    }

    fn vote_type(&self) -> VoteType {
        self.vote_type
    }

    fn validator_address(&self) -> &PrincepsAddress {
        &self.address
    }

    fn extension(&self) -> Option<&SignedExtension<PrincepsContext>> {
        None
    }

    fn take_extension(&mut self) -> Option<SignedExtension<PrincepsContext>> {
        None
    }

    fn extend(self, _extension: SignedExtension<PrincepsContext>) -> Self {
        self
    }
}

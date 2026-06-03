//! `PrincepsContext` — the central abstraction Malachite uses to know about our chain.
//!
//! Once this trait is implemented, the entire `malachitebft-core-consensus` and
//! `malachitebft-engine` machinery can drive consensus over our types.

use informalsystems_malachitebft_core_types::{
    Context, NilOrVal, Round, ValidatorSet as _, ValueId, VoteType,
};
use informalsystems_malachitebft_signing_ed25519::Ed25519;

use crate::types::{
    PrincepsAddress, PrincepsHeight, PrincepsProposal, PrincepsProposalPart, PrincepsValidator,
    PrincepsValidatorSet, PrincepsValue, PrincepsVote,
};

#[derive(Clone, Debug, Default)]
pub struct PrincepsContext;

impl Context for PrincepsContext {
    type Address = PrincepsAddress;
    type Height = PrincepsHeight;
    type ProposalPart = PrincepsProposalPart;
    type Proposal = PrincepsProposal;
    type Validator = PrincepsValidator;
    type ValidatorSet = PrincepsValidatorSet;
    type Value = PrincepsValue;
    type Vote = PrincepsVote;
    type Extension = ();
    type SigningScheme = Ed25519;

    /// Round-robin proposer selection by (height + round) modulo validator-set size.
    ///
    /// Validators are pre-sorted by (`voting_power` desc, address asc) in
    /// `PrincepsValidatorSet::new`, so every honest node picks the same proposer
    /// for the same (height, round) — the determinism the contract requires.
    fn select_proposer<'a>(
        &self,
        validator_set: &'a Self::ValidatorSet,
        height: Self::Height,
        round: Round,
    ) -> &'a Self::Validator {
        let count = validator_set.count();
        assert!(count > 0, "validator set is empty");
        let round_u64 = u64::try_from(round.as_i64().max(0)).unwrap_or(0);
        let index_u64 = height.0.wrapping_add(round_u64);
        let index = usize::try_from(index_u64).unwrap_or(usize::MAX) % count;
        validator_set
            .get_by_index(index)
            .expect("index < count by construction")
    }

    fn new_proposal(
        &self,
        height: Self::Height,
        round: Round,
        value: Self::Value,
        pol_round: Round,
        address: Self::Address,
    ) -> Self::Proposal {
        PrincepsProposal { height, round, value, pol_round, address }
    }

    fn new_prevote(
        &self,
        height: Self::Height,
        round: Round,
        value_id: NilOrVal<ValueId<Self>>,
        address: Self::Address,
    ) -> Self::Vote {
        PrincepsVote {
            height,
            round,
            value_id,
            vote_type: VoteType::Prevote,
            address,
        }
    }

    fn new_precommit(
        &self,
        height: Self::Height,
        round: Round,
        value_id: NilOrVal<ValueId<Self>>,
        address: Self::Address,
    ) -> Self::Vote {
        PrincepsVote {
            height,
            round,
            value_id,
            vote_type: VoteType::Precommit,
            address,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use informalsystems_malachitebft_core_types::{
        Height as HeightTrait, Proposal as ProposalTrait, Validator, ValidatorSet,
        Vote as VoteTrait,
    };
    use informalsystems_malachitebft_signing_ed25519::PrivateKey;
    use rdk_types::BlockHash;
    use rand::rngs::OsRng;

    fn validator(addr_byte: u8, power: u64) -> PrincepsValidator {
        let private = PrivateKey::generate(OsRng);
        let public = private.public_key();
        PrincepsValidator::new(PrincepsAddress([addr_byte; 20]), public, power)
    }

    #[test]
    fn validator_set_is_sorted_by_power_then_address() {
        let set = PrincepsValidatorSet::new(vec![
            validator(0x01, 100),
            validator(0x02, 300),
            validator(0x03, 200),
        ]);
        let powers: Vec<u64> = set
            .validators()
            .iter()
            .map(Validator::voting_power)
            .collect();
        assert_eq!(powers, vec![300, 200, 100]);
        assert_eq!(set.total_voting_power(), 600);
        assert_eq!(set.count(), 3);
    }

    #[test]
    fn select_proposer_round_robins_deterministically() {
        let ctx = PrincepsContext;
        let set = PrincepsValidatorSet::new(vec![
            validator(0x01, 100),
            validator(0x02, 100),
            validator(0x03, 100),
        ]);
        // Same height + round → same proposer across calls
        let h = PrincepsHeight(7);
        let p1 = ctx.select_proposer(&set, h, Round::new(0)).address;
        let p2 = ctx.select_proposer(&set, h, Round::new(0)).address;
        assert_eq!(p1, p2);

        // height + 1 picks the next validator in the rotation
        let p3 = ctx.select_proposer(&set, h.increment(), Round::new(0)).address;
        assert_ne!(p1, p3);
    }

    #[test]
    fn new_proposal_round_trips_fields() {
        let ctx = PrincepsContext;
        let addr = PrincepsAddress([0xaa; 20]);
        let value = PrincepsValue(BlockHash([0xbb; 32]));
        let proposal = ctx.new_proposal(
            PrincepsHeight(5),
            Round::new(1),
            value,
            Round::Nil,
            addr,
        );
        assert_eq!(ProposalTrait::height(&proposal), PrincepsHeight(5));
        assert_eq!(*ProposalTrait::value(&proposal), value);
        assert_eq!(*ProposalTrait::validator_address(&proposal), addr);
    }

    #[test]
    fn new_prevote_and_precommit_have_distinct_types() {
        let ctx = PrincepsContext;
        let addr = PrincepsAddress([0xaa; 20]);
        let vid: NilOrVal<BlockHash> = NilOrVal::Val(BlockHash([0xbb; 32]));
        let prevote = ctx.new_prevote(PrincepsHeight(5), Round::new(0), vid, addr);
        let precommit = ctx.new_precommit(PrincepsHeight(5), Round::new(0), vid, addr);
        assert_eq!(VoteTrait::vote_type(&prevote), VoteType::Prevote);
        assert_eq!(VoteTrait::vote_type(&precommit), VoteType::Precommit);
    }

    #[test]
    fn height_increment_and_decrement() {
        let h = PrincepsHeight::INITIAL;
        assert_eq!(h.as_u64(), 1);
        assert_eq!(h.increment().as_u64(), 2);
        assert_eq!(PrincepsHeight::ZERO.decrement(), None);
        assert_eq!(PrincepsHeight(5).decrement().unwrap().as_u64(), 4);
    }
}

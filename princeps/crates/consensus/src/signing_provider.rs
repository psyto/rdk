//! `SigningProvider` implementation — the trait the Malachite engine plugs in.
//!
//! Holds our private key as state; delegates the actual signing to
//! [`crate::signing`]'s canonical encoding so the wire format and the engine
//! interface stay consistent.

use informalsystems_malachitebft_core_types::{SignedMessage, SigningProvider};
use informalsystems_malachitebft_signing_ed25519::{PrivateKey, PublicKey, Signature};

use crate::context::PrincepsContext;
use crate::signing::{
    proposal_signing_bytes, sign_proposal as sign_proposal_with,
    sign_vote as sign_vote_with, vote_signing_bytes,
};
use crate::types::{PrincepsProposal, PrincepsProposalPart, PrincepsVote};

#[derive(Debug)]
pub struct PrincepsSigningProvider {
    private_key: PrivateKey,
}

impl PrincepsSigningProvider {
    #[must_use]
    pub const fn new(private_key: PrivateKey) -> Self {
        Self { private_key }
    }

    #[must_use]
    pub fn public_key(&self) -> PublicKey {
        self.private_key.public_key()
    }
}

impl SigningProvider<PrincepsContext> for PrincepsSigningProvider {
    fn sign_vote(&self, vote: PrincepsVote) -> SignedMessage<PrincepsContext, PrincepsVote> {
        sign_vote_with(vote, &self.private_key)
    }

    fn verify_signed_vote(
        &self,
        vote: &PrincepsVote,
        signature: &Signature,
        public_key: &PublicKey,
    ) -> bool {
        public_key.verify(&vote_signing_bytes(vote), signature).is_ok()
    }

    fn sign_proposal(
        &self,
        proposal: PrincepsProposal,
    ) -> SignedMessage<PrincepsContext, PrincepsProposal> {
        sign_proposal_with(proposal, &self.private_key)
    }

    fn verify_signed_proposal(
        &self,
        proposal: &PrincepsProposal,
        signature: &Signature,
        public_key: &PublicKey,
    ) -> bool {
        public_key
            .verify(&proposal_signing_bytes(proposal), signature)
            .is_ok()
    }

    fn sign_proposal_part(
        &self,
        part: PrincepsProposalPart,
    ) -> SignedMessage<PrincepsContext, PrincepsProposalPart> {
        // Stage 18a: parts now carry the proposer's encoded block bytes.
        // Sign a serde-JSON serialization so the receiving validator can
        // verify the proposer authored these exact bytes. Matches the
        // codec's wire format for `PrincepsProposalPart`.
        let bytes = serde_json::to_vec(&part).expect("proposal-part serialisation");
        let sig = self.private_key.sign(&bytes);
        SignedMessage::new(part, sig)
    }

    fn verify_signed_proposal_part(
        &self,
        part: &PrincepsProposalPart,
        signature: &Signature,
        public_key: &PublicKey,
    ) -> bool {
        let Ok(bytes) = serde_json::to_vec(part) else {
            return false;
        };
        public_key.verify(&bytes, signature).is_ok()
    }

    fn sign_vote_extension(&self, ext: ()) -> SignedMessage<PrincepsContext, ()> {
        // Vote extensions are unused at v0 (Context::Extension = ()).
        let sig = self.private_key.sign(&[]);
        SignedMessage::new(ext, sig)
    }

    fn verify_signed_vote_extension(
        &self,
        _ext: &(),
        signature: &Signature,
        public_key: &PublicKey,
    ) -> bool {
        public_key.verify(&[], signature).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{PrincepsAddress, PrincepsHeight, PrincepsValue};
    use informalsystems_malachitebft_core_types::{NilOrVal, Round, VoteType};
    use rdk_types::BlockHash;
    use rand::rngs::OsRng;

    fn provider() -> (PrincepsSigningProvider, PublicKey) {
        let sk = PrivateKey::generate(OsRng);
        let pk = sk.public_key();
        (PrincepsSigningProvider::new(sk), pk)
    }

    fn sample_vote() -> PrincepsVote {
        PrincepsVote {
            height: PrincepsHeight(1),
            round: Round::new(0),
            value_id: NilOrVal::Val(BlockHash([0x42; 32])),
            vote_type: VoteType::Prevote,
            address: PrincepsAddress([0xaa; 20]),
        }
    }

    fn sample_proposal() -> PrincepsProposal {
        PrincepsProposal {
            height: PrincepsHeight(1),
            round: Round::new(0),
            value: PrincepsValue(BlockHash([0x42; 32])),
            pol_round: Round::Nil,
            address: PrincepsAddress([0xaa; 20]),
        }
    }

    #[test]
    fn vote_sign_verify_round_trips() {
        let (sp, pk) = provider();
        let vote = sample_vote();
        let signed = sp.sign_vote(vote.clone());
        assert!(sp.verify_signed_vote(&vote, &signed.signature, &pk));
    }

    #[test]
    fn vote_tamper_detected() {
        let (sp, pk) = provider();
        let vote = sample_vote();
        let signed = sp.sign_vote(vote.clone());
        let mut tampered = vote;
        tampered.value_id = NilOrVal::Val(BlockHash([0x43; 32]));
        assert!(!sp.verify_signed_vote(&tampered, &signed.signature, &pk));
    }

    #[test]
    fn proposal_sign_verify_round_trips() {
        let (sp, pk) = provider();
        let proposal = sample_proposal();
        let signed = sp.sign_proposal(proposal.clone());
        assert!(sp.verify_signed_proposal(&proposal, &signed.signature, &pk));
    }

    #[test]
    fn proposal_tamper_detected() {
        let (sp, pk) = provider();
        let proposal = sample_proposal();
        let signed = sp.sign_proposal(proposal.clone());
        let mut tampered = proposal;
        tampered.value = PrincepsValue(BlockHash([0x99; 32]));
        assert!(!sp.verify_signed_proposal(&tampered, &signed.signature, &pk));
    }

    #[test]
    fn proposal_part_sign_verify_round_trips() {
        use informalsystems_malachitebft_core_types::Round;
        let (sp, pk) = provider();
        let part = PrincepsProposalPart {
            height: PrincepsHeight(5),
            round: Round::new(0),
            pol_round: Round::Nil,
            proposer: PrincepsAddress([0xaa; 20]),
            block_bytes: vec![1, 2, 3, 4],
        };
        let signed = sp.sign_proposal_part(part.clone());
        assert!(sp.verify_signed_proposal_part(&part, &signed.signature, &pk));

        // Tampering with the block bytes invalidates the signature.
        let mut tampered = part.clone();
        tampered.block_bytes.push(99);
        assert!(!sp.verify_signed_proposal_part(&tampered, &signed.signature, &pk));
    }

    #[test]
    fn vote_extension_sign_verify_round_trips() {
        let (sp, pk) = provider();
        let signed = sp.sign_vote_extension(());
        assert!(sp.verify_signed_vote_extension(&(), &signed.signature, &pk));
    }

    #[test]
    fn signature_from_one_provider_does_not_verify_under_another() {
        let (sp1, _pk1) = provider();
        let (_sp2, pk2) = provider();
        let vote = sample_vote();
        let signed = sp1.sign_vote(vote.clone());
        // Signed by provider 1, verified against provider 2's public key — must fail.
        assert!(!sp1.verify_signed_vote(&vote, &signed.signature, &pk2));
    }
}

//! JSON codec for the Princeps consensus messages.
//!
//! Each `Codec<T>` impl serializes a Malachite message into a JSON blob and
//! back. Several Malachite types (`SignedMessage`, `PolkaCertificate`,
//! `RoundCertificate`, `StreamMessage`, etc.) use `derive_where` rather than
//! serde derives, so we route those through `Raw*` shim structs that hold
//! their fields explicitly. The shims live entirely inside this file — they
//! are not part of the public API.
//!
//! Stage 13m: this used to be a fully stubbed `CodecStub` set; with the
//! stubs in place the consensus engine could compile but a two-validator
//! libp2p run could never form a quorum because votes/proposals refused to
//! serialize.

use bytes::Bytes;
use informalsystems_malachitebft_app::types::codec::Codec;
use informalsystems_malachitebft_app::types::streaming::{StreamId, StreamMessage};
use informalsystems_malachitebft_app::types::sync::{RawDecidedValue, Request, Response, Status};
use informalsystems_malachitebft_app::types::{PeerId, ProposedValue, SignedConsensusMsg};
use informalsystems_malachitebft_engine::util::streaming::StreamContent;
use informalsystems_malachitebft_sync::{ValueRequest, ValueResponse};
use informalsystems_malachitebft_core_consensus::LivenessMsg;
use informalsystems_malachitebft_core_types::{
    CommitCertificate, CommitSignature, NilOrVal, PolkaCertificate, PolkaSignature, Round,
    RoundCertificate, RoundCertificateType, RoundSignature, SignedMessage, Validity, VoteType,
};
use informalsystems_malachitebft_signing_ed25519::Signature;
use rdk_types::BlockHash;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::context::PrincepsContext;
use crate::types::{
    PrincepsAddress, PrincepsHeight, PrincepsProposal, PrincepsProposalPart, PrincepsValue, PrincepsVote,
};

#[derive(Copy, Clone, Debug, Default)]
pub struct PrincepsCodec;

#[derive(Debug, Error)]
pub enum CodecError {
    #[error("json codec error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid peer id bytes: {0}")]
    PeerId(String),
}

// ---- ProposalPart (Stage 18a: now carries the proposer's block bytes) ----

impl Codec<PrincepsProposalPart> for PrincepsCodec {
    type Error = CodecError;

    fn decode(&self, bytes: Bytes) -> Result<PrincepsProposalPart, Self::Error> {
        Ok(serde_json::from_slice(&bytes)?)
    }

    fn encode(&self, msg: &PrincepsProposalPart) -> Result<Bytes, Self::Error> {
        Ok(Bytes::from(serde_json::to_vec(msg)?))
    }
}

// ---- SignedConsensusMsg (votes + proposals) ------------------------------

#[derive(Serialize, Deserialize)]
struct RawSignedVote {
    message: PrincepsVote,
    signature: Signature,
}

#[derive(Serialize, Deserialize)]
struct RawSignedProposal {
    message: PrincepsProposal,
    signature: Signature,
}

#[derive(Serialize, Deserialize)]
enum RawSignedConsensusMsg {
    Vote(RawSignedVote),
    Proposal(RawSignedProposal),
}

impl From<SignedConsensusMsg<PrincepsContext>> for RawSignedConsensusMsg {
    fn from(msg: SignedConsensusMsg<PrincepsContext>) -> Self {
        match msg {
            SignedConsensusMsg::Vote(v) => Self::Vote(RawSignedVote {
                message: v.message,
                signature: v.signature,
            }),
            SignedConsensusMsg::Proposal(p) => Self::Proposal(RawSignedProposal {
                message: p.message,
                signature: p.signature,
            }),
        }
    }
}

impl From<RawSignedConsensusMsg> for SignedConsensusMsg<PrincepsContext> {
    fn from(raw: RawSignedConsensusMsg) -> Self {
        match raw {
            RawSignedConsensusMsg::Vote(v) => {
                SignedConsensusMsg::Vote(SignedMessage::new(v.message, v.signature))
            }
            RawSignedConsensusMsg::Proposal(p) => {
                SignedConsensusMsg::Proposal(SignedMessage::new(p.message, p.signature))
            }
        }
    }
}

impl Codec<SignedConsensusMsg<PrincepsContext>> for PrincepsCodec {
    type Error = CodecError;

    fn decode(&self, bytes: Bytes) -> Result<SignedConsensusMsg<PrincepsContext>, Self::Error> {
        let raw: RawSignedConsensusMsg = serde_json::from_slice(&bytes)?;
        Ok(raw.into())
    }

    fn encode(&self, msg: &SignedConsensusMsg<PrincepsContext>) -> Result<Bytes, Self::Error> {
        let raw = RawSignedConsensusMsg::from(msg.clone());
        Ok(Bytes::from(serde_json::to_vec(&raw)?))
    }
}

// ---- LivenessMsg (gossip heartbeats + skip certs) ------------------------

#[derive(Serialize, Deserialize)]
struct RawPolkaSignature {
    address: PrincepsAddress,
    signature: Signature,
}

#[derive(Serialize, Deserialize)]
struct RawPolkaCertificate {
    height: PrincepsHeight,
    round: Round,
    value_id: BlockHash,
    polka_signatures: Vec<RawPolkaSignature>,
}

#[derive(Serialize, Deserialize)]
struct RawRoundSignature {
    vote_type: VoteType,
    value_id: NilOrVal<BlockHash>,
    address: PrincepsAddress,
    signature: Signature,
}

#[derive(Serialize, Deserialize)]
struct RawRoundCertificate {
    height: PrincepsHeight,
    round: Round,
    cert_type: RoundCertificateType,
    round_signatures: Vec<RawRoundSignature>,
}

#[derive(Serialize, Deserialize)]
enum RawLivenessMsg {
    Vote(RawSignedVote),
    PolkaCertificate(RawPolkaCertificate),
    SkipRoundCertificate(RawRoundCertificate),
}

impl From<LivenessMsg<PrincepsContext>> for RawLivenessMsg {
    fn from(msg: LivenessMsg<PrincepsContext>) -> Self {
        match msg {
            LivenessMsg::Vote(v) => Self::Vote(RawSignedVote {
                message: v.message,
                signature: v.signature,
            }),
            LivenessMsg::PolkaCertificate(c) => Self::PolkaCertificate(RawPolkaCertificate {
                height: c.height,
                round: c.round,
                value_id: c.value_id,
                polka_signatures: c
                    .polka_signatures
                    .into_iter()
                    .map(|s| RawPolkaSignature {
                        address: s.address,
                        signature: s.signature,
                    })
                    .collect(),
            }),
            LivenessMsg::SkipRoundCertificate(c) => {
                Self::SkipRoundCertificate(RawRoundCertificate {
                    height: c.height,
                    round: c.round,
                    cert_type: c.cert_type,
                    round_signatures: c
                        .round_signatures
                        .into_iter()
                        .map(|s| RawRoundSignature {
                            vote_type: s.vote_type,
                            value_id: s.value_id,
                            address: s.address,
                            signature: s.signature,
                        })
                        .collect(),
                })
            }
        }
    }
}

impl From<RawLivenessMsg> for LivenessMsg<PrincepsContext> {
    fn from(raw: RawLivenessMsg) -> Self {
        match raw {
            RawLivenessMsg::Vote(v) => LivenessMsg::Vote(SignedMessage::new(v.message, v.signature)),
            RawLivenessMsg::PolkaCertificate(c) => LivenessMsg::PolkaCertificate(PolkaCertificate {
                height: c.height,
                round: c.round,
                value_id: c.value_id,
                polka_signatures: c
                    .polka_signatures
                    .into_iter()
                    .map(|s| PolkaSignature {
                        address: s.address,
                        signature: s.signature,
                    })
                    .collect(),
            }),
            RawLivenessMsg::SkipRoundCertificate(c) => {
                LivenessMsg::SkipRoundCertificate(RoundCertificate {
                    height: c.height,
                    round: c.round,
                    cert_type: c.cert_type,
                    round_signatures: c
                        .round_signatures
                        .into_iter()
                        .map(|s| RoundSignature {
                            vote_type: s.vote_type,
                            value_id: s.value_id,
                            address: s.address,
                            signature: s.signature,
                        })
                        .collect(),
                })
            }
        }
    }
}

impl Codec<LivenessMsg<PrincepsContext>> for PrincepsCodec {
    type Error = CodecError;

    fn decode(&self, bytes: Bytes) -> Result<LivenessMsg<PrincepsContext>, Self::Error> {
        let raw: RawLivenessMsg = serde_json::from_slice(&bytes)?;
        Ok(raw.into())
    }

    fn encode(&self, msg: &LivenessMsg<PrincepsContext>) -> Result<Bytes, Self::Error> {
        let raw = RawLivenessMsg::from(msg.clone());
        Ok(Bytes::from(serde_json::to_vec(&raw)?))
    }
}

// ---- StreamMessage<ProposalPart> -----------------------------------------

#[derive(Serialize, Deserialize)]
enum RawStreamContent {
    Data(PrincepsProposalPart),
    Fin,
}

#[derive(Serialize, Deserialize)]
struct RawStreamMessage {
    stream_id: Bytes,
    sequence: u64,
    content: RawStreamContent,
}

impl From<StreamMessage<PrincepsProposalPart>> for RawStreamMessage {
    fn from(msg: StreamMessage<PrincepsProposalPart>) -> Self {
        Self {
            stream_id: msg.stream_id.to_bytes(),
            sequence: msg.sequence,
            content: match msg.content {
                StreamContent::Data(p) => RawStreamContent::Data(p),
                StreamContent::Fin => RawStreamContent::Fin,
            },
        }
    }
}

impl From<RawStreamMessage> for StreamMessage<PrincepsProposalPart> {
    fn from(raw: RawStreamMessage) -> Self {
        Self {
            stream_id: StreamId::new(raw.stream_id),
            sequence: raw.sequence,
            content: match raw.content {
                RawStreamContent::Data(p) => StreamContent::Data(p),
                RawStreamContent::Fin => StreamContent::Fin,
            },
        }
    }
}

impl Codec<StreamMessage<PrincepsProposalPart>> for PrincepsCodec {
    type Error = CodecError;

    fn decode(&self, bytes: Bytes) -> Result<StreamMessage<PrincepsProposalPart>, Self::Error> {
        let raw: RawStreamMessage = serde_json::from_slice(&bytes)?;
        Ok(raw.into())
    }

    fn encode(&self, msg: &StreamMessage<PrincepsProposalPart>) -> Result<Bytes, Self::Error> {
        let raw = RawStreamMessage::from(msg.clone());
        Ok(Bytes::from(serde_json::to_vec(&raw)?))
    }
}

// ---- ProposedValue (WAL) -------------------------------------------------

/// Mirror of [`Validity`] — the upstream enum only derives borsh, not serde,
/// so we round-trip it through this serde-friendly twin.
#[derive(Serialize, Deserialize)]
enum RawValidity {
    Valid,
    Invalid,
}

impl From<Validity> for RawValidity {
    fn from(v: Validity) -> Self {
        match v {
            Validity::Valid => Self::Valid,
            Validity::Invalid => Self::Invalid,
        }
    }
}

impl From<RawValidity> for Validity {
    fn from(r: RawValidity) -> Self {
        match r {
            RawValidity::Valid => Self::Valid,
            RawValidity::Invalid => Self::Invalid,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct RawProposedValue {
    height: PrincepsHeight,
    round: Round,
    valid_round: Round,
    proposer: PrincepsAddress,
    value: PrincepsValue,
    validity: RawValidity,
}

impl From<ProposedValue<PrincepsContext>> for RawProposedValue {
    fn from(v: ProposedValue<PrincepsContext>) -> Self {
        Self {
            height: v.height,
            round: v.round,
            valid_round: v.valid_round,
            proposer: v.proposer,
            value: v.value,
            validity: v.validity.into(),
        }
    }
}

impl From<RawProposedValue> for ProposedValue<PrincepsContext> {
    fn from(r: RawProposedValue) -> Self {
        Self {
            height: r.height,
            round: r.round,
            valid_round: r.valid_round,
            proposer: r.proposer,
            value: r.value,
            validity: r.validity.into(),
        }
    }
}

impl Codec<ProposedValue<PrincepsContext>> for PrincepsCodec {
    type Error = CodecError;

    fn decode(&self, bytes: Bytes) -> Result<ProposedValue<PrincepsContext>, Self::Error> {
        let raw: RawProposedValue = serde_json::from_slice(&bytes)?;
        Ok(raw.into())
    }

    fn encode(&self, msg: &ProposedValue<PrincepsContext>) -> Result<Bytes, Self::Error> {
        let raw = RawProposedValue::from(msg.clone());
        Ok(Bytes::from(serde_json::to_vec(&raw)?))
    }
}

// ---- sync::Status --------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct RawStatus {
    /// `PeerId`'s on-wire multihash form. The peer crate does provide a
    /// `serde` feature, but enabling it would require pulling that crate in
    /// as a workspace dep just for one type — round-tripping through
    /// `to_bytes`/`from_bytes` is simpler.
    peer_id: Bytes,
    tip_height: PrincepsHeight,
    history_min_height: PrincepsHeight,
}

impl From<Status<PrincepsContext>> for RawStatus {
    fn from(s: Status<PrincepsContext>) -> Self {
        Self {
            peer_id: Bytes::from(s.peer_id.to_bytes()),
            tip_height: s.tip_height,
            history_min_height: s.history_min_height,
        }
    }
}

impl TryFrom<RawStatus> for Status<PrincepsContext> {
    type Error = CodecError;

    fn try_from(r: RawStatus) -> Result<Self, Self::Error> {
        Ok(Self {
            peer_id: PeerId::from_bytes(&r.peer_id)
                .map_err(|e| CodecError::PeerId(e.to_string()))?,
            tip_height: r.tip_height,
            history_min_height: r.history_min_height,
        })
    }
}

impl Codec<Status<PrincepsContext>> for PrincepsCodec {
    type Error = CodecError;

    fn decode(&self, bytes: Bytes) -> Result<Status<PrincepsContext>, Self::Error> {
        let raw: RawStatus = serde_json::from_slice(&bytes)?;
        raw.try_into()
    }

    fn encode(&self, msg: &Status<PrincepsContext>) -> Result<Bytes, Self::Error> {
        let raw = RawStatus::from(msg.clone());
        Ok(Bytes::from(serde_json::to_vec(&raw)?))
    }
}

// ---- sync::Request -------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct RawValueRequest {
    height: PrincepsHeight,
}

#[derive(Serialize, Deserialize)]
enum RawRequest {
    ValueRequest(RawValueRequest),
}

impl From<Request<PrincepsContext>> for RawRequest {
    fn from(r: Request<PrincepsContext>) -> Self {
        match r {
            Request::ValueRequest(vr) => Self::ValueRequest(RawValueRequest { height: vr.height }),
        }
    }
}

impl From<RawRequest> for Request<PrincepsContext> {
    fn from(raw: RawRequest) -> Self {
        match raw {
            RawRequest::ValueRequest(vr) => Self::ValueRequest(ValueRequest::new(vr.height)),
        }
    }
}

impl Codec<Request<PrincepsContext>> for PrincepsCodec {
    type Error = CodecError;

    fn decode(&self, bytes: Bytes) -> Result<Request<PrincepsContext>, Self::Error> {
        let raw: RawRequest = serde_json::from_slice(&bytes)?;
        Ok(raw.into())
    }

    fn encode(&self, msg: &Request<PrincepsContext>) -> Result<Bytes, Self::Error> {
        let raw = RawRequest::from(msg.clone());
        Ok(Bytes::from(serde_json::to_vec(&raw)?))
    }
}

// ---- sync::Response ------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct RawCommitSignature {
    address: PrincepsAddress,
    signature: Signature,
}

#[derive(Serialize, Deserialize)]
struct RawCommitCertificate {
    height: PrincepsHeight,
    round: Round,
    value_id: BlockHash,
    commit_signatures: Vec<RawCommitSignature>,
}

#[derive(Serialize, Deserialize)]
struct RawDecided {
    value_bytes: Bytes,
    certificate: RawCommitCertificate,
}

#[derive(Serialize, Deserialize)]
struct RawValueResponse {
    height: PrincepsHeight,
    value: Option<RawDecided>,
}

#[derive(Serialize, Deserialize)]
enum RawResponse {
    ValueResponse(RawValueResponse),
}

impl From<Response<PrincepsContext>> for RawResponse {
    fn from(r: Response<PrincepsContext>) -> Self {
        match r {
            Response::ValueResponse(vr) => Self::ValueResponse(RawValueResponse {
                height: vr.height,
                value: vr.value.map(|d| RawDecided {
                    value_bytes: d.value_bytes,
                    certificate: RawCommitCertificate {
                        height: d.certificate.height,
                        round: d.certificate.round,
                        value_id: d.certificate.value_id,
                        commit_signatures: d
                            .certificate
                            .commit_signatures
                            .into_iter()
                            .map(|s| RawCommitSignature {
                                address: s.address,
                                signature: s.signature,
                            })
                            .collect(),
                    },
                }),
            }),
        }
    }
}

impl From<RawResponse> for Response<PrincepsContext> {
    fn from(raw: RawResponse) -> Self {
        match raw {
            RawResponse::ValueResponse(vr) => Self::ValueResponse(ValueResponse::new(
                vr.height,
                vr.value.map(|d| RawDecidedValue {
                    value_bytes: d.value_bytes,
                    certificate: CommitCertificate {
                        height: d.certificate.height,
                        round: d.certificate.round,
                        value_id: d.certificate.value_id,
                        commit_signatures: d
                            .certificate
                            .commit_signatures
                            .into_iter()
                            .map(|s| CommitSignature {
                                address: s.address,
                                signature: s.signature,
                            })
                            .collect(),
                    },
                }),
            )),
        }
    }
}

impl Codec<Response<PrincepsContext>> for PrincepsCodec {
    type Error = CodecError;

    fn decode(&self, bytes: Bytes) -> Result<Response<PrincepsContext>, Self::Error> {
        let raw: RawResponse = serde_json::from_slice(&bytes)?;
        Ok(raw.into())
    }

    fn encode(&self, msg: &Response<PrincepsContext>) -> Result<Bytes, Self::Error> {
        let raw = RawResponse::from(msg.clone());
        Ok(Bytes::from(serde_json::to_vec(&raw)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use informalsystems_malachitebft_app::types::codec::{
        ConsensusCodec, SyncCodec, WalCodec,
    };
    use informalsystems_malachitebft_signing_ed25519::PrivateKey;
    use rand::rngs::OsRng;

    fn assert_wal_codec<C: WalCodec<PrincepsContext>>() {}
    fn assert_consensus_codec<C: ConsensusCodec<PrincepsContext>>() {}
    fn assert_sync_codec<C: SyncCodec<PrincepsContext>>() {}

    #[test]
    fn rdk_codec_satisfies_all_three_super_traits() {
        assert_wal_codec::<PrincepsCodec>();
        assert_consensus_codec::<PrincepsCodec>();
        assert_sync_codec::<PrincepsCodec>();
    }

    fn sample_vote() -> PrincepsVote {
        PrincepsVote {
            height: PrincepsHeight(7),
            round: Round::new(2),
            value_id: NilOrVal::Val(BlockHash([0x42; 32])),
            vote_type: VoteType::Prevote,
            address: PrincepsAddress([0xaa; 20]),
        }
    }

    fn sample_proposal() -> PrincepsProposal {
        PrincepsProposal {
            height: PrincepsHeight(7),
            round: Round::new(2),
            value: PrincepsValue(BlockHash([0x11; 32])),
            pol_round: Round::Nil,
            address: PrincepsAddress([0xbb; 20]),
        }
    }

    fn sample_signature() -> Signature {
        let sk = PrivateKey::generate(OsRng);
        sk.sign(b"hello")
    }

    #[test]
    fn proposal_part_round_trips() {
        let codec = PrincepsCodec;
        let part = PrincepsProposalPart {
            height: PrincepsHeight(11),
            round: Round::new(0),
            pol_round: Round::Nil,
            proposer: PrincepsAddress([0xab; 20]),
            block_bytes: vec![0xde, 0xad, 0xbe, 0xef],
        };
        let bytes = Codec::<PrincepsProposalPart>::encode(&codec, &part).unwrap();
        let decoded: PrincepsProposalPart = codec.decode(bytes).unwrap();
        assert_eq!(decoded, part);
    }

    #[test]
    fn signed_consensus_msg_vote_round_trips() {
        let codec = PrincepsCodec;
        let msg = SignedConsensusMsg::<PrincepsContext>::Vote(SignedMessage::new(
            sample_vote(),
            sample_signature(),
        ));
        let bytes = codec.encode(&msg).unwrap();
        let decoded: SignedConsensusMsg<PrincepsContext> = codec.decode(bytes).unwrap();
        match (msg, decoded) {
            (SignedConsensusMsg::Vote(a), SignedConsensusMsg::Vote(b)) => {
                assert_eq!(a.message, b.message);
                assert_eq!(a.signature, b.signature);
            }
            _ => panic!("variant mismatch"),
        }
    }

    #[test]
    fn signed_consensus_msg_proposal_round_trips() {
        let codec = PrincepsCodec;
        let msg = SignedConsensusMsg::<PrincepsContext>::Proposal(SignedMessage::new(
            sample_proposal(),
            sample_signature(),
        ));
        let bytes = codec.encode(&msg).unwrap();
        let decoded: SignedConsensusMsg<PrincepsContext> = codec.decode(bytes).unwrap();
        match (msg, decoded) {
            (SignedConsensusMsg::Proposal(a), SignedConsensusMsg::Proposal(b)) => {
                assert_eq!(a.message, b.message);
                assert_eq!(a.signature, b.signature);
            }
            _ => panic!("variant mismatch"),
        }
    }

    #[test]
    fn liveness_vote_round_trips() {
        let codec = PrincepsCodec;
        let msg = LivenessMsg::<PrincepsContext>::Vote(SignedMessage::new(
            sample_vote(),
            sample_signature(),
        ));
        let bytes = codec.encode(&msg).unwrap();
        let decoded: LivenessMsg<PrincepsContext> = codec.decode(bytes).unwrap();
        match (msg, decoded) {
            (LivenessMsg::Vote(a), LivenessMsg::Vote(b)) => {
                assert_eq!(a.message, b.message);
                assert_eq!(a.signature, b.signature);
            }
            _ => panic!("variant mismatch"),
        }
    }

    #[test]
    fn proposed_value_round_trips() {
        let codec = PrincepsCodec;
        let msg = ProposedValue::<PrincepsContext> {
            height: PrincepsHeight(9),
            round: Round::new(0),
            valid_round: Round::Nil,
            proposer: PrincepsAddress([0xcc; 20]),
            value: PrincepsValue(BlockHash([0x77; 32])),
            validity: Validity::Valid,
        };
        let bytes = codec.encode(&msg).unwrap();
        let decoded: ProposedValue<PrincepsContext> = codec.decode(bytes).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn sync_request_round_trips() {
        let codec = PrincepsCodec;
        let msg = Request::<PrincepsContext>::ValueRequest(ValueRequest::new(PrincepsHeight(5)));
        let bytes = codec.encode(&msg).unwrap();
        let decoded: Request<PrincepsContext> = codec.decode(bytes).unwrap();
        match (msg, decoded) {
            (Request::ValueRequest(a), Request::ValueRequest(b)) => {
                assert_eq!(a.height, b.height);
            }
        }
    }

    #[test]
    fn stream_message_fin_round_trips() {
        let codec = PrincepsCodec;
        let msg = StreamMessage::<PrincepsProposalPart> {
            stream_id: StreamId::new(Bytes::from_static(b"princeps-stream-42")),
            sequence: 11,
            content: StreamContent::Fin,
        };
        let bytes = codec.encode(&msg).unwrap();
        let decoded: StreamMessage<PrincepsProposalPart> = codec.decode(bytes).unwrap();
        assert_eq!(decoded.stream_id, msg.stream_id);
        assert_eq!(decoded.sequence, msg.sequence);
        assert!(matches!(decoded.content, StreamContent::Fin));
    }
}

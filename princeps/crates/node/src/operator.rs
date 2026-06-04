//! Operator-key registry + Layer-3 socialization-declaration verification.
//!
//! ADR-010 Layer 3 dependency #3: lets a validator confirm that a
//! socialization declaration was produced by the holder of a registered
//! [`OperatorKey`], satisfying the "operator-sig admission" requirement
//! before Layer 3 mutates `market.supply_index` and appends a
//! [`crate::chain_history::ChainEvent::Socialization`].
//!
//! Mirrors the [`rdk_oracle::PublisherKey`] pattern exactly:
//! - SEC1-compressed secp256k1 public keys (33 bytes).
//! - IEEE P1363 fixed-format ECDSA signatures (64 bytes).
//! - Registry is a plain `BTreeMap<OperatorId, OperatorKey>`.
//! - Signed payload is canonical big-endian for cross-validator
//!   determinism.
//!
//! Why secp256k1 + same `[k256](https://docs.rs/k256)` configuration as
//! the oracle: every Ethereum-ecosystem operator already has secp256k1
//! keys, and reusing the configuration avoids forcing operators to
//! manage a second key scheme. Same rationale rdk-oracle uses for its
//! publisher path.
//!
//! Registry persistence: like the publisher registry, operator keys are
//! NOT included in [`crate::CoordinatorSnapshot`]. The binary
//! re-registers operators at boot from the deployment config. This
//! keeps the snapshot deterministic across deployments with different
//! operator sets, mirroring the "publishers are re-registered by the
//! binary at boot" comment on [`crate::PrincepsNode::snapshot`].

use std::collections::BTreeMap;

use k256::ecdsa::{signature::Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

/// Identifier for an operator. The integer carries no semantics —
/// the bridge picks per-deployment IDs (e.g., 1, 2, 3 for a 3-of-3
/// operator set). Determinism only requires every validator agree on
/// the mapping. Mirrors [`rdk_oracle::FeedId`].
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct OperatorId(pub u32);

/// SEC1-compressed secp256k1 public key (33 bytes).
///
/// Registered against an [`OperatorId`] in [`OperatorRegistry`].
/// [`verify_socialization_declaration`] checks declarations against
/// the registered key for the operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OperatorKey(pub [u8; 33]);

/// IEEE P1363 fixed-format ECDSA signature: `r || s` concatenated, 64
/// bytes. [`OperatorSignature::ZERO`] is the placeholder for unsigned
/// declarations — useful in tests but never accepted by
/// [`verify_socialization_declaration`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OperatorSignature(pub [u8; 64]);

impl OperatorSignature {
    /// All-zero signature. Used as the placeholder before signing and
    /// as a serialization default; never verifies.
    pub const ZERO: Self = Self([0u8; 64]);
}

impl Default for OperatorSignature {
    fn default() -> Self {
        Self::ZERO
    }
}

/// Operator-key registry. Bridge calls [`register`] at boot for each
/// configured operator; [`verify_socialization_declaration`] looks up
/// the operator's key at admission time. Mirrors how
/// [`rdk_oracle::OracleState`] owns its `PublisherKey` map.
///
/// Pure data — no I/O, no boot-time validation beyond key shape (which
/// happens lazily at verify time).
///
/// [`register`]: OperatorRegistry::register
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OperatorRegistry {
    by_id: BTreeMap<OperatorId, OperatorKey>,
}

impl OperatorRegistry {
    /// Empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register or replace the key for an operator. Idempotent —
    /// re-registering an `OperatorId` with the same key is a no-op;
    /// re-registering with a different key is a rotation (the prior
    /// key stops verifying immediately).
    pub fn register(&mut self, id: OperatorId, key: OperatorKey) {
        self.by_id.insert(id, key);
    }

    /// Look up the key for an operator, if registered.
    #[must_use]
    pub fn get(&self, id: OperatorId) -> Option<&OperatorKey> {
        self.by_id.get(&id)
    }

    /// Number of registered operators.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    /// `true` if no operators are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// Iterator over `(id, key)` pairs in `OperatorId` order. Useful
    /// for boot-time logging.
    pub fn iter(&self) -> impl Iterator<Item = (&OperatorId, &OperatorKey)> {
        self.by_id.iter()
    }
}

/// One operator's declaration that Layer 3 socialization should fire
/// for a given lending market: take `unfilled` units of supplier
/// principal pro-rata across the market's `scaled_supply` positions
/// (the haircut math lives in the lending crate, gated on this
/// declaration verifying).
///
/// Signed by the operator's secp256k1 key registered in
/// [`OperatorRegistry`]. The `block_height` field binds the
/// declaration to a specific block — a replay of the same declaration
/// at a later height fails to verify (same field, different bytes,
/// different signature).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SocializationDeclaration {
    /// Who is declaring.
    pub operator: OperatorId,
    /// Which lending market is socializing.
    pub market_id: u32,
    /// How much supplier principal is being written down. Aligned with
    /// the ADR-010 spec field type (`u128`).
    pub unfilled: u128,
    /// Block height at which the declaration is intended to apply.
    /// Binds the signature to a specific block so a captured
    /// signature can't be replayed.
    pub block_height: u64,
    /// ECDSA signature over [`Self::signed_bytes`].
    pub signature: OperatorSignature,
}

impl SocializationDeclaration {
    /// Construct a declaration with [`OperatorSignature::ZERO`] — the
    /// pre-signing shape used by [`demo_signing::sign_declaration`]
    /// and any production signer.
    #[must_use]
    pub const fn unsigned(
        operator: OperatorId,
        market_id: u32,
        unfilled: u128,
        block_height: u64,
    ) -> Self {
        Self {
            operator,
            market_id,
            unfilled,
            block_height,
            signature: OperatorSignature::ZERO,
        }
    }

    /// The exact bytes the operator signs.
    ///
    /// Layout (32 bytes total, big-endian throughout):
    /// ```text
    ///   [ 0..  4]  operator     (u32)
    ///   [ 4..  8]  market_id    (u32)
    ///   [ 8.. 24]  unfilled     (u128)
    ///   [24.. 32]  block_height (u64)
    /// ```
    ///
    /// Fixed-width + big-endian so every validator computes the same
    /// digest from the same fields. Mirrors
    /// [`rdk_oracle::PriceObservation::signed_bytes`]' approach.
    #[must_use]
    pub fn signed_bytes(&self) -> [u8; 32] {
        let mut buf = [0u8; 32];
        buf[0..4].copy_from_slice(&self.operator.0.to_be_bytes());
        buf[4..8].copy_from_slice(&self.market_id.to_be_bytes());
        buf[8..24].copy_from_slice(&self.unfilled.to_be_bytes());
        buf[24..32].copy_from_slice(&self.block_height.to_be_bytes());
        buf
    }
}

/// Why a [`SocializationDeclaration`] was rejected at verify time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperatorAuthError {
    /// The declaration's `operator` field doesn't match any registered
    /// key. Either the registry hasn't been seeded for that operator,
    /// or the declaration cites a fabricated ID.
    UnknownOperator { operator: OperatorId },
    /// The signature did not verify against the registered key. Either
    /// the signature is malformed, the signed message doesn't match
    /// the declaration's fields, the operator's private key has been
    /// rotated without a registry update, or the signature is the
    /// `OperatorSignature::ZERO` placeholder.
    InvalidSignature { operator: OperatorId },
}

/// Verify a [`SocializationDeclaration`] against the operator-key
/// registry. Returns the verified [`OperatorId`] on success — callers
/// can trust the `(market_id, unfilled, block_height)` fields
/// originated from the holder of the registered key.
///
/// Returns [`OperatorAuthError::UnknownOperator`] if the declaration's
/// operator field has no registered key. Returns
/// [`OperatorAuthError::InvalidSignature`] for any signature failure
/// (malformed signature, malformed pubkey, mismatched signer, zero
/// signature placeholder).
pub fn verify_socialization_declaration(
    declaration: &SocializationDeclaration,
    registry: &OperatorRegistry,
) -> Result<OperatorId, OperatorAuthError> {
    let pubkey = registry.get(declaration.operator).ok_or(
        OperatorAuthError::UnknownOperator {
            operator: declaration.operator,
        },
    )?;
    let Ok(verifying_key) = VerifyingKey::from_sec1_bytes(&pubkey.0) else {
        return Err(OperatorAuthError::InvalidSignature {
            operator: declaration.operator,
        });
    };
    let Ok(signature) = k256::ecdsa::Signature::from_slice(&declaration.signature.0) else {
        return Err(OperatorAuthError::InvalidSignature {
            operator: declaration.operator,
        });
    };
    verifying_key
        .verify(&declaration.signed_bytes(), &signature)
        .map_err(|_| OperatorAuthError::InvalidSignature {
            operator: declaration.operator,
        })?;
    Ok(declaration.operator)
}

/// Deterministic in-process signing helpers for **dev / demo / test
/// paths only** — never use in production.
///
/// Production deployments sign declarations off-band (operators run
/// external signing infrastructure; the node only verifies). These
/// helpers exist so the v0 CLI demo path (`princeps socialize`),
/// integration tests, and the operator-module unit tests can build
/// + sign authentic declarations in-process.
///
/// Same pattern as `rdk_oracle::verify::test_signing`, but exposed
/// publicly because the bin's `socialize` subcommand needs an
/// end-to-end demo flow that includes signing.
pub mod demo_signing {
    use super::{OperatorId, OperatorKey, OperatorSignature, SocializationDeclaration};
    use k256::ecdsa::{signature::Signer, SigningKey};

    /// Build a deterministic [`SigningKey`] from a 1..=255 seed.
    /// Same seed always yields the same secp256k1 keypair, so demo
    /// flows can register the corresponding [`OperatorKey`] and then
    /// produce signatures that verify.
    #[must_use]
    pub fn demo_signing_key(seed: u8) -> SigningKey {
        assert!(seed != 0, "demo_signing_key seed must be non-zero");
        let bytes = [seed; 32];
        SigningKey::from_slice(&bytes).expect("seed bytes form a valid secp256k1 scalar")
    }

    /// Compute the SEC1-compressed public key (33 bytes) for a
    /// demo signing key. Register this against an `OperatorId` in
    /// the registry; declarations signed with the matching
    /// [`demo_signing_key`] of the same seed will verify.
    #[must_use]
    pub fn demo_operator_key(seed: u8) -> OperatorKey {
        let sk = demo_signing_key(seed);
        let vk = sk.verifying_key();
        let compressed = vk.to_encoded_point(true);
        let bytes = compressed.as_bytes();
        let mut out = [0u8; 33];
        out.copy_from_slice(bytes);
        OperatorKey(out)
    }

    /// Sign a fresh declaration with the given signing key. Returns
    /// the declaration with the `signature` field populated.
    #[must_use]
    pub fn sign_declaration(
        operator: OperatorId,
        market_id: u32,
        unfilled: u128,
        block_height: u64,
        signing_key: &SigningKey,
    ) -> SocializationDeclaration {
        let unsigned =
            SocializationDeclaration::unsigned(operator, market_id, unfilled, block_height);
        let msg = unsigned.signed_bytes();
        let sig: k256::ecdsa::Signature = signing_key.sign(&msg);
        let bytes = sig.to_bytes();
        let mut sig_array = [0u8; 64];
        sig_array.copy_from_slice(&bytes);
        SocializationDeclaration {
            signature: OperatorSignature(sig_array),
            ..unsigned
        }
    }
}

#[cfg(test)]
mod tests {
    use super::demo_signing::{demo_operator_key, demo_signing_key, sign_declaration};
    use super::*;

    fn registry_with_operator(seed: u8, id: OperatorId) -> OperatorRegistry {
        let mut r = OperatorRegistry::new();
        r.register(id, demo_operator_key(seed));
        r
    }

    // ─── happy path ──────────────────────────────────────────────

    #[test]
    fn round_trip_verifies() {
        let sk = demo_signing_key(1);
        let registry = registry_with_operator(1, OperatorId(7));
        let decl = sign_declaration(OperatorId(7), 0, 5_000, 1_234, &sk);
        let verified = verify_socialization_declaration(&decl, &registry).expect("verifies");
        assert_eq!(verified, OperatorId(7));
    }

    // ─── tampering rejections ────────────────────────────────────

    #[test]
    fn tampered_market_id_rejected() {
        let sk = demo_signing_key(1);
        let registry = registry_with_operator(1, OperatorId(7));
        let mut decl = sign_declaration(OperatorId(7), 0, 5_000, 1_234, &sk);
        decl.market_id = 999;
        let err =
            verify_socialization_declaration(&decl, &registry).expect_err("tamper rejected");
        assert_eq!(err, OperatorAuthError::InvalidSignature { operator: OperatorId(7) });
    }

    #[test]
    fn tampered_unfilled_rejected() {
        let sk = demo_signing_key(1);
        let registry = registry_with_operator(1, OperatorId(7));
        let mut decl = sign_declaration(OperatorId(7), 0, 5_000, 1_234, &sk);
        decl.unfilled = 5_001;
        let err =
            verify_socialization_declaration(&decl, &registry).expect_err("tamper rejected");
        assert_eq!(err, OperatorAuthError::InvalidSignature { operator: OperatorId(7) });
    }

    #[test]
    fn tampered_block_height_rejected() {
        let sk = demo_signing_key(1);
        let registry = registry_with_operator(1, OperatorId(7));
        let mut decl = sign_declaration(OperatorId(7), 0, 5_000, 1_234, &sk);
        decl.block_height = 9_999;
        let err =
            verify_socialization_declaration(&decl, &registry).expect_err("tamper rejected");
        assert_eq!(err, OperatorAuthError::InvalidSignature { operator: OperatorId(7) });
    }

    #[test]
    fn tampered_operator_id_rejected_as_unknown_when_not_registered() {
        let sk = demo_signing_key(1);
        let registry = registry_with_operator(1, OperatorId(7));
        let mut decl = sign_declaration(OperatorId(7), 0, 5_000, 1_234, &sk);
        decl.operator = OperatorId(8);
        let err =
            verify_socialization_declaration(&decl, &registry).expect_err("tamper rejected");
        assert_eq!(err, OperatorAuthError::UnknownOperator { operator: OperatorId(8) });
    }

    #[test]
    fn tampered_operator_id_rejected_as_invalid_when_other_registered() {
        // Operator 7 signs, but the declaration claims operator 8.
        // Operator 8 IS registered with a different key, so we hit the
        // InvalidSignature branch rather than UnknownOperator.
        let sk = demo_signing_key(1);
        let mut registry = OperatorRegistry::new();
        registry.register(OperatorId(7), demo_operator_key(1));
        registry.register(OperatorId(8), demo_operator_key(2));
        let mut decl = sign_declaration(OperatorId(7), 0, 5_000, 1_234, &sk);
        decl.operator = OperatorId(8);
        let err =
            verify_socialization_declaration(&decl, &registry).expect_err("rejected");
        assert_eq!(err, OperatorAuthError::InvalidSignature { operator: OperatorId(8) });
    }

    // ─── key-related rejections ──────────────────────────────────

    #[test]
    fn wrong_pubkey_rejected() {
        let sk = demo_signing_key(1);
        // Registry has operator 7 keyed to seed=2, but signer uses seed=1.
        let mut registry = OperatorRegistry::new();
        registry.register(OperatorId(7), demo_operator_key(2));
        let decl = sign_declaration(OperatorId(7), 0, 5_000, 1_234, &sk);
        let err =
            verify_socialization_declaration(&decl, &registry).expect_err("wrong key rejected");
        assert_eq!(err, OperatorAuthError::InvalidSignature { operator: OperatorId(7) });
    }

    #[test]
    fn malformed_pubkey_rejected() {
        let sk = demo_signing_key(1);
        let mut registry = OperatorRegistry::new();
        registry.register(OperatorId(7), OperatorKey([0xFF; 33]));
        let decl = sign_declaration(OperatorId(7), 0, 5_000, 1_234, &sk);
        let err = verify_socialization_declaration(&decl, &registry)
            .expect_err("malformed pubkey rejected");
        assert_eq!(err, OperatorAuthError::InvalidSignature { operator: OperatorId(7) });
    }

    #[test]
    fn malformed_signature_rejected() {
        let registry = registry_with_operator(1, OperatorId(7));
        let decl = SocializationDeclaration {
            operator: OperatorId(7),
            market_id: 0,
            unfilled: 5_000,
            block_height: 1_234,
            signature: OperatorSignature([0xFF; 64]),
        };
        let err = verify_socialization_declaration(&decl, &registry)
            .expect_err("malformed sig rejected");
        assert_eq!(err, OperatorAuthError::InvalidSignature { operator: OperatorId(7) });
    }

    #[test]
    fn zero_signature_rejected() {
        // The unsigned placeholder must not pass verify.
        let registry = registry_with_operator(1, OperatorId(7));
        let unsigned = SocializationDeclaration::unsigned(OperatorId(7), 0, 5_000, 1_234);
        let err = verify_socialization_declaration(&unsigned, &registry)
            .expect_err("zero sig rejected");
        assert_eq!(err, OperatorAuthError::InvalidSignature { operator: OperatorId(7) });
    }

    #[test]
    fn unknown_operator_rejected_distinct_from_invalid_signature() {
        // Empty registry → any declaration is UnknownOperator, never
        // InvalidSignature, regardless of signature bytes.
        let registry = OperatorRegistry::new();
        let sk = demo_signing_key(1);
        let decl = sign_declaration(OperatorId(7), 0, 5_000, 1_234, &sk);
        let err = verify_socialization_declaration(&decl, &registry)
            .expect_err("unknown operator");
        assert_eq!(err, OperatorAuthError::UnknownOperator { operator: OperatorId(7) });
    }

    // ─── registry mechanics ──────────────────────────────────────

    #[test]
    fn registry_register_is_idempotent_then_rotation() {
        let mut registry = OperatorRegistry::new();
        let k1 = demo_operator_key(1);
        let k2 = demo_operator_key(2);
        registry.register(OperatorId(0), k1);
        registry.register(OperatorId(0), k1); // idempotent
        assert_eq!(registry.len(), 1);
        assert_eq!(registry.get(OperatorId(0)), Some(&k1));
        // Rotation: replace key. Prior key stops verifying immediately.
        registry.register(OperatorId(0), k2);
        assert_eq!(registry.len(), 1);
        assert_eq!(registry.get(OperatorId(0)), Some(&k2));
        // Signing with prior key now fails to verify.
        let sk1 = demo_signing_key(1);
        let decl = sign_declaration(OperatorId(0), 0, 100, 1, &sk1);
        let err = verify_socialization_declaration(&decl, &registry)
            .expect_err("rotated-away key rejected");
        assert_eq!(err, OperatorAuthError::InvalidSignature { operator: OperatorId(0) });
    }

    #[test]
    fn registry_iter_returns_in_id_order() {
        let mut registry = OperatorRegistry::new();
        registry.register(OperatorId(3), demo_operator_key(3));
        registry.register(OperatorId(1), demo_operator_key(1));
        registry.register(OperatorId(2), demo_operator_key(2));
        let ids: Vec<u32> = registry.iter().map(|(id, _)| id.0).collect();
        assert_eq!(ids, vec![1, 2, 3]);
    }

    // ─── replay protection via block_height binding ──────────────

    #[test]
    fn replay_at_different_block_height_rejected() {
        // Operator signs a declaration for height 100. An adversary
        // captures it and tries to replay at height 200 — same other
        // fields, different height, signature no longer verifies.
        let sk = demo_signing_key(1);
        let registry = registry_with_operator(1, OperatorId(7));
        let decl_100 = sign_declaration(OperatorId(7), 0, 5_000, 100, &sk);
        assert!(verify_socialization_declaration(&decl_100, &registry).is_ok());
        let mut replayed = decl_100;
        replayed.block_height = 200;
        let err = verify_socialization_declaration(&replayed, &registry)
            .expect_err("replay rejected");
        assert_eq!(err, OperatorAuthError::InvalidSignature { operator: OperatorId(7) });
    }
}

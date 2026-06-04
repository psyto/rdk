#![allow(unreachable_pub)]

//! Bridge-side orchestrator for ADR-010 Layer 3.
//!
//! Wires the three protocol primitives that landed earlier:
//!
//!   1. `princeps_node::operator::verify_socialization_declaration` —
//!      ECDSA-verifies the operator's signed declaration against an
//!      [`OperatorRegistry`].
//!   2. `princeps_lending::socialization::socialize_residual` —
//!      applies the supply_index haircut to the market, returns a
//!      [`SocializationReport`].
//!   3. `crate::chain_history::ChainHistoryStore::append_event` —
//!      records the resulting [`ChainEvent::Socialization`] at the
//!      declaration's `block_height` for audit + restart-replay.
//!
//! [`run_socialization`] is the single end-to-end entrypoint; the CLI
//! `princeps socialize` subcommand and any future RPC / precompile
//! entrypoint all route through it.

use eyre::eyre;

use princeps_lending::{socialize_residual, Market, SocializationReport};
use princeps_node::operator::{
    verify_socialization_declaration, OperatorRegistry, SocializationDeclaration,
};

use crate::chain_history::{ChainEvent, ChainHistoryStore};

/// Run the Layer 3 orchestration end-to-end against a single market.
///
/// `declaration` is the operator's signed declaration (already built
/// + signed by the caller). `registry` is the operator-key registry
/// loaded from deployment config. `market` is the lending market the
/// haircut applies to (typically owned by the bridge state). `store`
/// is the chain-history store the resulting [`ChainEvent::Socialization`]
/// is appended to. `declared_by_label` is a short human-readable
/// string recorded in the event (e.g., `"operator-1"`); it's
/// informational and audit-friendly, not load-bearing on consensus.
///
/// On any failure the function returns an `eyre::Error` and the
/// market + chain history are left untouched (the haircut mutation
/// happens only after verification + market-id sanity, and the event
/// append happens only after the mutation succeeds).
pub fn run_socialization(
    declaration: SocializationDeclaration,
    registry: &OperatorRegistry,
    market: &mut Market,
    store: &ChainHistoryStore,
    declared_by_label: &str,
) -> eyre::Result<SocializationReport> {
    // 1. Operator-sig admission.
    verify_socialization_declaration(&declaration, registry)
        .map_err(|e| eyre!("operator authorization failed: {e:?}"))?;

    // 2. Sanity: the declaration's market_id must match the market
    //    we're applying it to. Prevents a typo / mis-routing where a
    //    declaration for market A lands against market B.
    if market.id.0 != declaration.market_id {
        return Err(eyre!(
            "market_id mismatch: declaration cites {} but market is {}",
            declaration.market_id,
            market.id.0,
        ));
    }

    // 3. Apply the haircut.
    let report = socialize_residual(market, declaration.unfilled)
        .map_err(|e| eyre!("socialize_residual failed: {e:?}"))?;

    // 4. Append the audit event. Use `absorbed` (the actual
    //    socialized amount post-cap) rather than the declaration's
    //    requested unfilled — they differ when the request exceeded
    //    total_supplied. The event records what actually happened.
    store.append_event(
        declaration.block_height,
        ChainEvent::Socialization {
            market_id: declaration.market_id,
            unfilled: report.absorbed,
            declared_by: declared_by_label.to_string(),
        },
    );

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use princeps_lending::{AssetId, Bps, Index, IrmParams, MarketId};
    use princeps_node::operator::{
        demo_signing::{demo_operator_key, demo_signing_key, sign_declaration},
        OperatorId,
    };

    fn seeded_market(total_supplied: u128) -> Market {
        let mut m = Market::new(
            MarketId(0),
            AssetId(1),
            AssetId(0),
            IrmParams {
                base_rate_per_block: 0,
                slope_below_kink_per_block: Index::RAY / 10_000,
                slope_above_kink_per_block: Index::RAY / 1_000,
                kink_bps: Bps(8_000),
            },
            Bps(9_500),
            Bps(500),
            Bps(1_000),
            0,
        );
        m.total_supplied = total_supplied;
        m
    }

    fn registry_with(seed: u8, id: OperatorId) -> OperatorRegistry {
        let mut r = OperatorRegistry::new();
        r.register(id, demo_operator_key(seed));
        r
    }

    // ─── happy path ──────────────────────────────────────────────

    #[test]
    fn end_to_end_happy_path_verifies_mutates_appends() {
        let sk = demo_signing_key(1);
        let registry = registry_with(1, OperatorId(1));
        let mut market = seeded_market(1_000);
        let store = ChainHistoryStore::empty();

        let decl = sign_declaration(OperatorId(1), 0, 100, 42, &sk);
        let report = run_socialization(decl, &registry, &mut market, &store, "operator-1")
            .expect("orchestrates");

        // Report sanity.
        assert_eq!(report.requested_unfilled, 100);
        assert_eq!(report.absorbed, 100);
        assert_eq!(report.new_total_supplied, 900);

        // Market mutated.
        assert_eq!(market.total_supplied, 900);

        // Chain history populated at the declaration's block height.
        let events = store.peek_at_height(42).expect("event recorded");
        assert_eq!(events.len(), 1);
        match &events[0] {
            ChainEvent::Socialization {
                market_id,
                unfilled,
                declared_by,
            } => {
                assert_eq!(*market_id, 0);
                assert_eq!(*unfilled, 100);
                assert_eq!(declared_by, "operator-1");
            }
        }
    }

    // ─── rejection branches leave state untouched ────────────────

    #[test]
    fn invalid_signature_rejects_and_does_not_mutate() {
        // Operator 1's key in registry; declaration claims operator 1
        // but is signed with a different key (seed=2).
        let bad_sk = demo_signing_key(2);
        let registry = registry_with(1, OperatorId(1));
        let mut market = seeded_market(1_000);
        let store = ChainHistoryStore::empty();

        let decl = sign_declaration(OperatorId(1), 0, 100, 42, &bad_sk);
        let err = run_socialization(decl, &registry, &mut market, &store, "x")
            .expect_err("auth fails");
        assert!(err.to_string().contains("operator authorization failed"));

        // Market untouched.
        assert_eq!(market.total_supplied, 1_000);
        // No chain event.
        assert!(store.peek_at_height(42).is_none());
    }

    #[test]
    fn unknown_operator_rejects_and_does_not_mutate() {
        let sk = demo_signing_key(1);
        // Empty registry.
        let registry = OperatorRegistry::new();
        let mut market = seeded_market(1_000);
        let store = ChainHistoryStore::empty();

        let decl = sign_declaration(OperatorId(1), 0, 100, 42, &sk);
        let err =
            run_socialization(decl, &registry, &mut market, &store, "x").expect_err("auth fails");
        assert!(err.to_string().contains("operator authorization failed"));

        assert_eq!(market.total_supplied, 1_000);
        assert!(store.peek_at_height(42).is_none());
    }

    #[test]
    fn market_id_mismatch_rejects_and_does_not_mutate() {
        // Declaration cites market 99, but the market we have is market 0.
        // Verification succeeds (declaration is well-signed for market 99),
        // but the orchestrator refuses to apply it to a different market.
        let sk = demo_signing_key(1);
        let mut registry = OperatorRegistry::new();
        registry.register(OperatorId(1), demo_operator_key(1));
        let mut market = seeded_market(1_000);
        let store = ChainHistoryStore::empty();

        let decl = sign_declaration(OperatorId(1), 99, 100, 42, &sk);
        let err = run_socialization(decl, &registry, &mut market, &store, "x")
            .expect_err("market mismatch");
        assert!(err.to_string().contains("market_id mismatch"));

        assert_eq!(market.total_supplied, 1_000);
        assert!(store.peek_at_height(42).is_none());
    }

    #[test]
    fn zero_unfilled_propagates_socialize_error() {
        let sk = demo_signing_key(1);
        let registry = registry_with(1, OperatorId(1));
        let mut market = seeded_market(1_000);
        let store = ChainHistoryStore::empty();

        let decl = sign_declaration(OperatorId(1), 0, 0, 42, &sk);
        let err = run_socialization(decl, &registry, &mut market, &store, "x")
            .expect_err("zero rejected");
        assert!(err.to_string().contains("socialize_residual failed"));
        assert!(err.to_string().contains("ZeroUnfilled"));

        assert_eq!(market.total_supplied, 1_000);
        assert!(store.peek_at_height(42).is_none());
    }

    // ─── cap-at-total feeds the audit event accurately ──────────

    #[test]
    fn over_request_caps_and_event_records_actually_absorbed() {
        // Declaration cites unfilled=10_000 against a market with
        // total_supplied=500. The haircut caps at 500; the audit
        // event records the actual amount absorbed (500), not the
        // requested (10_000).
        let sk = demo_signing_key(1);
        let registry = registry_with(1, OperatorId(1));
        let mut market = seeded_market(500);
        let store = ChainHistoryStore::empty();

        let decl = sign_declaration(OperatorId(1), 0, 10_000, 42, &sk);
        let report = run_socialization(decl, &registry, &mut market, &store, "op")
            .expect("orchestrates");
        assert_eq!(report.requested_unfilled, 10_000);
        assert_eq!(report.absorbed, 500);

        let events = store.peek_at_height(42).expect("event recorded");
        match &events[0] {
            ChainEvent::Socialization { unfilled, .. } => assert_eq!(*unfilled, 500),
        }
    }

    // ─── zero-signature placeholder rejected (defensive) ─────────

    #[test]
    fn zero_signature_placeholder_rejected() {
        let registry = registry_with(1, OperatorId(1));
        let mut market = seeded_market(1_000);
        let store = ChainHistoryStore::empty();

        let decl = SocializationDeclaration::unsigned(OperatorId(1), 0, 100, 42);
        // signature is ZERO — never verifies.
        let err = run_socialization(decl, &registry, &mut market, &store, "x")
            .expect_err("zero sig fails");
        assert!(err.to_string().contains("operator authorization failed"));

        assert_eq!(market.total_supplied, 1_000);
    }
}

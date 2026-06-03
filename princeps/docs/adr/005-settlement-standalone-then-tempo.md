# ADR-005 — Settlement: standalone L1 v0–v1, Tempo at v2

**Status**: Accepted (2026-05-31)
**Scope**: v0–v3 (platform lifecycle)

## Context

Settlement layer choice determines cross-chain composability, sequencer model dependencies, and the natural liquidity path for tenants. Options: settle to Ethereum (rollup model, slow), settle to a payment-grade chain (Tempo), or remain standalone (fastest ship, no cross-chain).

## Decision

Standalone Reth-based L1 for v0–v1 (no cross-chain settlement dependency). Add Tempo settlement integration at v2 (structured products phase), once Tempo testnet is mature and the existing zktempo light-client work is production-ready.

## Rationale

- Standalone = fastest ship, no cross-chain coordination cost.
- Tempo settlement at v2 makes architectural sense because: (a) zktempo light-client primitive is already ~80% done; (b) Tempo's Reth-based execution makes the integration shallow; (c) structured products specifically benefit from settling to a payment-grade chain.
- Postponing cross-chain settlement avoids over-investing in plumbing before product-market-fit on lending/options.

## Tradeoffs

Standalone v0–v1 means Princeps isn't immediately composable with Ethereum/Solana DeFi. Acceptable — composability is a v2+ concern. Mitigation: bridge primitives can be added at v2 alongside Tempo settlement.

## Forecloses

Nothing — adds Tempo integration later without prejudicing other settlement paths (Ethereum L1, other Reth-based chains, future ZK-bridge architectures).

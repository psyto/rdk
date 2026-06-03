# ADR-003 — Oracle: validator-quorum push in EL

**Status**: Accepted (2026-05-31)
**Scope**: v0–v3 (platform lifecycle)

## Context

DeFi lending and options both depend critically on oracle latency and integrity. Chainlink-style pull oracles have heartbeat delays (minutes) that cause bad debt during volatility — every major DeFi exploit involving "bad debt" in 2022–2024 had an oracle-latency component. HL-style push oracles in the EL solve this by making prices canonical state updated every block.

## Decision

Oracle as a first-class execution-layer primitive — validators sign price observations, aggregator (median-of-medians with deviation circuit breaker) lives in the EL. No Chainlink, no external oracle dependency at the protocol level.

## Rationale

- Already shipped in the kernel as Stage 11 (39 tests, median-of-medians aggregator) + Stage 11b (18 tests, secp256k1 ECDSA signed observations + publisher registry).
- HL-style push oracle = sub-second updates, no rent paid to external oracle networks, no oracle-manipulation MEV at the contract boundary.
- Critical for lending: tight oracle-to-liquidation gap is the difference between solvent protocol and bad debt during 50x cascades.
- Push oracle in EL is the only architecture that can support sub-second deterministic liquidations (ADR-001 finality + this oracle model together).

## Tradeoffs

Bootstrapping a publisher set is a real operational cost — need ≥7 independent publishers for credible aggregation at v1. Mitigation: v0 launches with 3–5 publishers covering the v0 collateral set (limited scope); expand to ≥7 before v1 options launch.

## Forecloses

Chainlink, Pyth, RedStone as protocol-level external oracle dependencies. Note: this does NOT foreclose using those as backup observation sources fed into the validator-quorum aggregator — that's a separate v1+ decision.

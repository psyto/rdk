# ADR-001 — Consensus: Malachite

**Status**: Accepted (2026-05-31)
**Scope**: v0–v3 (platform lifecycle)

## Context

Princeps needs a BFT consensus layer that integrates with Reth as the execution layer and provides instant finality (no reorgs). The consensus choice locks in security model, validator coordination semantics, and finality guarantees — all of which propagate into liquidation correctness, settlement assumptions, and the institutional-rails compliance story.

## Decision

Use [Malachite](https://github.com/informalsystems/malachite) (Informal Systems) as the BFT consensus library, integrated with Reth via the `ConsensusBridge` trait already implemented in the kernel.

## Rationale

- Already integrated and battle-tested in the kernel — 22+ consensus tests pass, Stages 1–13k shipped through 2026-05-23.
- Tendermint-family BFT semantics give instant finality with no reorgs — exactly what a prime broker needs (no liquidation can be reverted by reorg, no settlement undone).
- Rust-native, no FFI, no opcode-shim layer.
- Informal Systems is a mature team; library is v0.5.0 and actively maintained.

## Tradeoffs

BFT requires a known validator set — not permissionless validator participation. Acceptable for institutional positioning and v0–v1 ship speed. Revisit at v3 if jurisdictional decentralization becomes a hard requirement.

## Forecloses

HotStuff, Aptos-style consensus, Solana-style PoH, optimistic rollup paths.

# ADR-002 — Sequencer: centralized v0–v1, decentralize at v3

**Status**: Accepted (2026-05-31)
**Scope**: v0–v3 (platform lifecycle)

## Context

Sequencer model determines who can include transactions in blocks, who controls ordering, and the platform's centralization story. Hyperliquid ran ~18 months as a centralized sequencer before decentralizing — and won the market by shipping. The decision here is whether to follow that pattern or commit to decentralized from day one.

## Decision

Single sequencer (operated by Hiro / Fabrknt) for v0–v1. Decentralized validator set with stake-weighted block production at v3 (institutional rails phase).

## Rationale

- Single sequencer = simplest possible ship for v0 (lending). HL did exactly this for 18 months and won.
- Centralization risk at v0 scale is acceptable: low TVL, small blast radius, you're the only one who can ship fast enough.
- v3 institutional rails will require credibly-neutral block production for KYC/compliance scenarios — that's the natural moment to decentralize.
- The market consistently rewards shipping over purity in DeFi (HL, Berachain, every major L1 in this generation).

## Tradeoffs

Public criticism around "centralized L1" is unavoidable in v0–v1. Mitigation: be honest about the centralization in the README and announcement; commit to v3 decentralization as a load-bearing roadmap item.

## Forecloses

Nothing permanently — just defers decentralization to v3.

# ADR-006 — Identity: anon-default, KYC as Layer-3 overlay at v3

**Status**: Accepted (2026-05-31)
**Scope**: v0–v3 (platform lifecycle)

## Context

Identity model determines TAM (chain-wide KYC kills DeFi composability), regulatory positioning (institutional tenants need some compliance story), and the architecture of access control across the platform. Two extremes: anon-only (DeFi-pure but kills institutional path) vs. chain-wide KYC (institutional-ready but kills DeFi).

## Decision

Anon-default account model for v0–v2. Layer-3 KYC compliance wrapper at v3 for institutional tenants who need it. KYC is opt-in per-tenant, not chain-wide.

## Rationale

- v0–v2 user is DeFi-native (anon by default). Forcing KYC chain-wide kills composability and limits TAM.
- Layer-3 wrapper model lets institutional issuers opt into compliance for THEIR products only — e.g. a tokenized money-market fund issued by a regulated bank is KYC-only at the issuer's wrapper layer, but the underlying lending market remains anon-default.
- This is the Provenance / Polymesh model done correctly: compliance as overlay, not base.
- Per-product KYC is the actual standard in TradFi (144A-restricted vs. public securities) — the overlay model maps to this.

## Tradeoffs

Some regulators may want chain-wide KYC for institutional adoption. Counter-argument: per-product KYC is the actual TradFi standard. Risk: some institutional buyers reject the overlay model and require chain-wide KYC; if that demand materializes from a large enough tenant, revisit with an ADR amendment.

Target tenant pool for v3 institutional rails is global tradfi (EU banks, US prime brokers, Asia ex-Japan). Japan megabanks specifically are not targeted through Princeps — they remain on the Solana track via separate parallel work.

## Forecloses

Nothing — wrapper can be added per-product later, and chain-wide KYC could be added as an opt-in tenant constraint without disrupting anon-default for other tenants.

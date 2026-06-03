# Princeps ADRs

Architectural Decision Records for the Princeps platform. ADR-001 through ADR-007 are foundational decisions locked for the platform lifecycle (2026-05-31). ADR-008+ document subsequent decisions as the platform evolves.

## Status meanings

- **Proposed** — under consideration
- **Accepted** — currently in force
- **Superseded** — replaced by a later ADR
- **Deprecated** — no longer in force, not yet replaced

## Locked foundation (2026-05-31)

| # | Title | Status |
|---|---|---|
| [001](./001-consensus-malachite.md) | Consensus: Malachite BFT | Accepted |
| [002](./002-sequencer-centralized-then-decentralize.md) | Sequencer: centralized v0–v1, decentralize at v3 | Accepted |
| [003](./003-oracle-validator-quorum-push.md) | Oracle: validator-quorum push in EL | Accepted |
| [004](./004-base-unit-usd-stable.md) | Base unit: USD-stable | Accepted |
| [005](./005-settlement-standalone-then-tempo.md) | Settlement: standalone L1 v0–v1, Tempo at v2 | Accepted |
| [006](./006-identity-anon-default-kyc-overlay.md) | Identity: anon-default, KYC as L3 overlay at v3 | Accepted |
| [007](./007-token-none-until-revenue.md) | Token: none until real revenue | Accepted |

## Operational extensions

| # | Title | Status |
|---|---|---|
| [008](./008-pre-token-validator-policy.md) | Pre-token validator policy: permissioned + legally bound until tokenomics ships | Accepted |

## How to add a new ADR

1. Number sequentially (ADR-008, ADR-009, ...).
2. Filename: `NNN-kebab-case-title.md`.
3. Use the standard sections: Context, Decision, Rationale, Tradeoffs, Forecloses.
4. Set Status to **Proposed** while under discussion, **Accepted** once locked.
5. To supersede an earlier ADR, set the earlier one to **Superseded** with a link to the new ADR; set the new ADR's status to **Accepted** with a link back.

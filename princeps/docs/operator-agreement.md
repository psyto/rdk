# Princeps validator operator agreement (v0 template)

**Status**: v0 template, accepted 2026-06-04.
**Scope**: v0–v1 (pre-token, pre-stake). Sunsetted at v3 per [ADR-008](./adr/008-pre-token-validator-policy.md) §4 and [ADR-010](./adr/010-bad-debt-depletion-policy.md) §Forecloses.
**Audit role**: this template is part of the Q1 2027 audit handoff bundle (see [README roadmap](../README.md)). The threat model ([L-5, OP-1, OP-4](./threat-model.md)) cites it as the Layer-2 mitigation for bad-debt depletion and the structural pre-token deterrent against validator misbehavior.

## Why this document exists

The Princeps consensus layer is Malachite BFT, which controls fork safety but not oracle honesty, censorship, or extraction. Until ADR-009 (tokenomics + on-chain slashing) ships, validators are not bonded by stake — meaning the only deterrent against misbehavior is legal and reputational. [ADR-008](./adr/008-pre-token-validator-policy.md) made the gap explicit; this document is the contract that closes it. [ADR-010](./adr/010-bad-debt-depletion-policy.md) Layer 2 extends the same mechanism to cover bad-debt depletion incidents.

This document is the *template*; the actual legal contract is executed bilaterally between the project lead and each prospective operator before that operator receives validator keys. At v0 testnet there are no signed instances (no real funds are at risk); the first signatures land before v1 mainnet onboards operators alongside any actual capital.

## Parties

- **Project**: Princeps, operating under [legal entity name TBD before v1 mainnet].
- **Operator**: [entity name], a legal entity registered in [jurisdiction].
- **Project lead**: the signatory of record on behalf of Princeps. Identified in the executed instance of this template.

## 1. Identity disclosure ([ADR-008](./adr/008-pre-token-validator-policy.md) §1)

The Operator's real-world legal entity, principal jurisdiction, and authorized signatories are disclosed to the Project and, at execution time, to every other Operator currently in the active validator set. Pseudonymous or unincorporated operation is not eligible under v0–v1.

Operators commit to notifying the Project of any change in legal entity, control, or principal jurisdiction within 14 days of the change taking effect.

## 2. Operational discipline ([ADR-008](./adr/008-pre-token-validator-policy.md) §2)

The Operator agrees to:

- **Software version**: run only the released Princeps binary at the current or immediately prior tagged version. Deviation requires advance notice (≥ 48 hours) and Project acknowledgement. Custom forks, unreleased builds, and debug instrumentation in the validator path are prohibited.
- **Oracle publisher registration**: if the Operator also runs an oracle publisher, registration follows the published v0 operator manual (publisher key generation, rotation cadence, observation submission cadence).
- **Key rotation**: validator keys are rotated at minimum every 12 months, or sooner on Project notice. Rotation procedures are documented in the v0 operator manual.
- **Availability**: best-effort 99% monthly uptime SLA on the validator host. Sustained absence (> 24 hours unannounced) triggers Section 3 remedies.
- **Hosting posture**: validator keys live on an air-gapped key-generation host with off-host encrypted backup. HSM use is recommended at v0 and required at v1 mainnet per [OP-1 in the threat model](./threat-model.md).

## 3. Misconduct remedies ([ADR-008](./adr/008-pre-token-validator-policy.md) §3)

The following actions constitute misconduct under this agreement:

1. **Equivocation** — voting for two different blocks at the same height (threat-model row C-1).
2. **Censorship** — provably omitting a specific user's transactions from blocks the Operator proposes (threat-model row C-4).
3. **Oracle manipulation** — submitting observations the Operator knows or should reasonably know to be inaccurate, or coordinating observation submission with other parties for price-impact effect (threat-model rows O-1, O-3).
4. **Software fork** — running modified Princeps binaries without the disclosure in §2.
5. **Sustained absence** — > 24 hours offline without notice during a deployed validator window (threat-model row C-2).

Confirmed misconduct triggers:

- **Immediate removal** from the active validator set. Removal does not require notice or process — the Project may revoke validator credentials and the Operator's stake (when ADR-009 ships) without further action.
- **Recovery of caused losses** — the Operator is liable to the Project for direct losses attributable to the misconduct, capped at the lesser of (a) the Operator's then-current operator-treasury balance or (b) two times the Project's monthly operating budget at the time of incident. Punitive damages are out of scope.
- **Public disclosure** — the incident, its mitigation, and the Operator's response are published in the Project's incident log within 72 hours per Section 5.

## 4. Lending halt obligations ([ADR-010](./adr/010-bad-debt-depletion-policy.md) Layer 2)

During any period when `PrincepsNode::is_lending_halted` returns `true` (the algorithmic Layer-1 halt is armed), each Operator agrees to either:

a. **Make-whole obligation**. Re-capitalize the on-chain InsuranceFund from operator treasury (single Operator, by mutual agreement of all Operators, or pro-rata across the active set — terms negotiated at incident response) before the active halt window expires. "Re-capitalize" means raising the fund balance to at least the [`LendingHaltParams`](../crates/node/src/lib.rs) threshold computed at halt time, such that an immediate re-evaluation would not re-arm.

b. **Trigger Layer 3**. Jointly declare to the Project that make-whole is infeasible, accept Layer-3 socialization as the Project documents it in [ADR-010](./adr/010-bad-debt-depletion-policy.md), and publish the disclosure required by Section 5 within 72 hours of the declaration. The Project may not initiate Layer-3 unilaterally — operator declaration is the human-in-the-loop pre-token equivalent of governance.

Each Operator declares their per-incident treasury capacity to the Project at agreement execution and updates it on any material change (50% drawdown or more, in either direction). At v0 testnet this declaration is informational; at v1 mainnet it is contractually binding as the cap on (a) above.

This Section does not extend the Operator's liability beyond the cap in §3. The aggregate of misconduct-derived losses (§3) and lending-halt make-whole obligations (this §4) is bounded by that cap.

## 5. Disclosure obligations ([ADR-010](./adr/010-bad-debt-depletion-policy.md) Layer 2)

For each event of the following type, the Operator (or, where the Operator is unavailable, the Project) publishes a post-mortem within 72 hours of the event:

- A confirmed misconduct incident under §3.
- A lending-halt event under §4 that resulted in Layer-3 socialization (Layer-1-only halts that resolved within the algorithmic window without operator action are publicized via the on-chain `lending_halt_until` field and do not require a separate post-mortem).
- An InsuranceFund deviation greater than 25% from the prior-day closing balance attributable to a single incident.

Post-mortems are published to the Project's incident log (URL TBD; placeholder `github.com/psyto/rdk/blob/main/princeps/docs/incidents/` until the project incident-log host is set). They include: timeline, root cause as best understood, mitigation taken, depositor impact, and any Operator-treasury actions taken under §4(a).

## 6. Sunset and supersession ([ADR-008](./adr/008-pre-token-validator-policy.md) §4)

This agreement is in force only until ADR-009 (reserved per the [ADR index](./adr/README.md), forthcoming) defines the on-chain tokenomics + slashing model. At ADR-009 acceptance, this template is superseded by:

- **On-chain stake** replaces the operator-treasury cap in §3 and the make-whole obligation in §4(a). Slashing becomes structurally enforceable through the protocol rather than contractually enforceable through this agreement.
- **On-chain governance** replaces the manual Layer-3 declaration in §4(b). Socialization, if invoked, routes through staked positions via protocol-defined parameters.
- **Permissionless validator set** replaces the §1 identity-disclosure regime, in lockstep with the v3 sequencer decentralization per [ADR-002](./adr/002-sequencer-centralized-then-decentralize.md).

Until that transition, this agreement remains the binding instrument. Operators who decline to migrate to the stake-backed regime at v3 exit the validator set on the transition block.

## 7. Execution

Each executed instance of this template carries:

- The Operator's legal entity name, jurisdiction, registration number, and authorized signatory.
- The Project's signatory of record.
- The Operator's declared per-incident treasury capacity (§4).
- The Operator's HSM posture declaration (§2).
- Counter-signature date.

Disputes are governed by [jurisdiction TBD before v1 mainnet — likely the Project's principal jurisdiction], with non-binding mediation as the first step before any litigation or arbitration. The Project commits to fixed mediator selection (single mediator from a mutually-agreed roster, drawn at random) to avoid forum-shopping incentives.

## Status notes

- **v0 testnet** — this template is published but not signed. No validator credentials are issued; the multi-validator `scripts/devnet-3.sh` runs informally for engineering purposes. Audit-bundle artifact only.
- **v1 mainnet onboarding** — first signed instances. Pre-condition for issuing any production validator key.
- **v3** — superseded by ADR-009 (see [ADR index](./adr/README.md)). This document is archived; on-chain stake takes over.

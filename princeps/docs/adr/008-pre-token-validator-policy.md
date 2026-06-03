# ADR-008 — Pre-token validator policy

**Status**: Accepted (2026-06-03)
**Scope**: v0–v1 (until tokenomics ships)

## Context

ADR-007 defers the protocol token until real fee revenue exists — earliest consideration post-v1. ADR-002 defers full validator decentralization to v3.

The gap between those two decisions is load-bearing and was not previously documented: from v0 testnet launch until token-and-stake economics are live, BFT consensus alone does not produce economic deterrence against validator misbehavior. Slashing requires something with value to slash; absent a token, the only deterrent left is legal and reputational. That works fine for a small known validator set, and fails open-ended for anyone who can join.

This ADR makes the pre-token validator policy explicit so the gap can't be exploited by silently growing the validator set before the economic backstop exists.

## Decision

Until the protocol token ships, the Princeps validator set is **permissioned** and validator operators are **legally bound** by an off-chain operator agreement covering:

1. **Identity disclosure** — each operator's real-world legal entity is known to the project lead and to the other operators.
2. **Operational discipline** — software version, oracle publisher registration, and key rotation cadence follow the published v0 operator manual; deviation requires advance notice.
3. **Misconduct remedies** — equivocation, censorship, or oracle manipulation triggers immediate removal from the validator set and is actionable under the operator agreement (recovery of any caused losses).
4. **Sunset condition** — this policy is in force only until ADR-009 (forthcoming, gating mainnet launch) defines the tokenomics + on-chain slashing model. At that point validators bond stake, slashing becomes on-chain enforceable, and the permissioned-set restriction is lifted.

Concretely:

- **v0 public testnet** — single sequencer (per ADR-002) or a small operator-set running `scripts/devnet-3.sh`-style; testnet has no real money so the operator agreement is informal.
- **v1 lending mainnet** — gated on tokenomics design completion (ADR-009) and at least one independent audit (see roadmap audit window). Until that gate is met, the v1 deployment is also permissioned: operator agreements signed before any operator receives validator keys.
- **v3** — sequencer decentralizes (per ADR-002), bonded stake replaces operator agreements (per ADR-009), this ADR is superseded.

## Rationale

- BFT consensus controls fork safety; it does not by itself control oracle honesty, censorship, or extraction. ADR-003 (validator-quorum oracle push) makes this gap especially relevant — validators control what price the lending engine sees.
- Hyperliquid ran a single sequencer for ~18 months before decentralizing and shipped successfully through that window. The precedent for "permissioned then token then decentralize" is well-established.
- Pre-token slashing via off-chain operator agreements is a known pattern (early Polygon CDK chains, Cosmos Hub during the first six months, every L2 with a centralized sequencer today). It is not novel, just usually undocumented — making it explicit is the credibility win.
- The alternative (permissionless validator set with no slashing) is what the external critique correctly flagged as a structural risk; this ADR closes that gap without re-opening the tokenomics question.

## Tradeoffs

- "Permissioned validator set" is a public criticism vector. Mitigation: same as ADR-002 — be explicit in the README, gate on ADR-009 for sunset.
- Operator agreement enforcement depends on jurisdiction; cross-border operators complicate the legal remedy. Mitigation: prefer operators in jurisdictions where the project lead can credibly pursue contract enforcement.
- Off-chain agreements don't scale to large validator counts. That is by design — the sunset happens precisely when validator counts need to scale, via ADR-009's on-chain stake mechanism.

## Forecloses

Nothing permanent. ADR-009 supersedes this once tokenomics + on-chain slashing are designed and audited.

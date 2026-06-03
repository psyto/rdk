# Princeps threat model (v0)

**Status**: Living document — updated alongside each ADR or stage that changes the trust surface.
**Last updated**: 2026-06-03

This document enumerates the attacker scenarios Princeps is designed to withstand at v0, the current mitigations, and the known gaps. It is the artifact that should be picked up first by anyone (auditor, prospective operator, prospective integrator) trying to understand what Princeps does and does not protect against. It complements the ADRs — ADRs record decisions; this document records why each decision is or isn't enough.

The audience is technical: it assumes familiarity with BFT consensus, EVM precompiles, oracle median aggregation, and standard DeFi attack patterns. For a higher-level overview see the [README](../README.md) and the [ADR index](./adr/README.md).

## Scope

**In scope:**

- v0 lending kernel: market state, IRM, health factor, liquidation, bad-debt absorption (see [docs/plans/v0-lending.md](./plans/v0-lending.md))
- v0 perp + funding kernel inherited from openhl
- Cross-margin portfolio engine ([Stage 23](./plans/v0-lending.md))
- Oracle aggregation and validator-quorum push ([ADR-003](./adr/003-oracle-validator-quorum-push.md))
- BFT consensus + validator set management
- EVM precompile boundary (`crates/evm`) including revert-guard semantics
- Bridge / coordinator state persistence

**Out of scope at v0 (revisited at later stages):**

- Token economics, on-chain slashing, bonded stake — gated on ADR-009 (see [ADR-008](./adr/008-pre-token-validator-policy.md))
- Options primitives (Black-Scholes, IV surface, Greeks) — v1
- Structured product vaults — v2
- KYC overlay / institutional rails — v3
- Multi-asset collateral, flash loans, governance — v1+ ([v0 plan](./plans/v0-lending.md#what-v0-does-not-ship))

## Trust assumptions

| Assumption | Source | Pre-token state (v0) | Post-token state |
| :--- | :--- | :--- | :--- |
| <2/3 validators are honest | BFT safety bound | Permissioned set + operator agreement ([ADR-008](./adr/008-pre-token-validator-policy.md)) | Bonded stake + on-chain slashing (ADR-009, forthcoming) |
| <1/3 validators are byzantine | BFT liveness bound | Same as above | Same as above |
| Oracle publishers act honestly | Per-publisher signed observations ([Stage 11b](../crates/oracle/src/lib.rs)) | Same operator set, same legal binding | Publisher registry + slashing on equivocation |
| Sequencer doesn't censor | [ADR-002](./adr/002-sequencer-centralized-then-decentralize.md) | Single operator-run sequencer; legal remedy in operator agreement | Decentralized validator set (v3) |
| Validator key files are protected | Operator OS hardening | 0o600 file perms on `validator-key.json`, off-host backups encouraged | Same + HSM recommended for stake-bearing validators |

The pre-token column is the v0 threat surface. The post-token column is what ADR-009 will need to deliver.

## Attack scenarios

Severity scale: **🔴 Critical** (funds at risk / system-wide), **🟠 High** (single-account loss / sustained DoS), **🟡 Medium** (degraded UX / transient), **🟢 Low** (observable but bounded).

Mitigation status: **✅ in place**, **🚧 partial**, **🛑 gap**.

### Oracle manipulation

| # | Scenario | Attacker capability | Severity | Mitigation | Reference |
| :--- | :--- | :--- | :--- | :--- | :--- |
| O-1 | Validator-quorum collusion pushes a fabricated price to trigger mass liquidations | >2/3 validators collude | 🔴 | ✅ Pre-token: permissioned operator agreement makes collusion legally and reputationally catastrophic. Post-token: on-chain slashing of equivocating publishers. | [ADR-003](./adr/003-oracle-validator-quorum-push.md), [ADR-008](./adr/008-pre-token-validator-policy.md) |
| O-2 | Single oracle publisher submits malicious observation | One publisher key compromised | 🟡 | ✅ Median-of-medians aggregation + deviation cap filter discard outliers; signed observations require publisher key. | `crates/oracle/src/lib.rs`, [Stage 11b commit](https://github.com/psyto/princeps/commit/main) |
| O-3 | Coordinated fast-move price push to trigger liquidations before users can react | <1/3 collusion or external feed manipulation | 🟠 | ✅ Per-block deviation guard on the aggregated oracle price (`CircuitBreakerParams`, v0 default 20% per-block / 50-block halt). Trip is detected in `PrincepsNode::tick` and surfaced through `is_oracle_halted`; the lending bad-debt loop in `bin/princeps` skips its run while halted. Halt window persists across restart via `CoordinatorSnapshot::oracle_halt_until`. Liquidations and repayments are intentionally not gated — repay is always safe and forced-liquidation suppression during a real attack is the whole point. | `crates/node/src/lib.rs`, LD-006 in [v0 plan](./plans/v0-lending.md#ld-006-usdc-oracle-pegged-1-with-depeg-circuit-breaker) |
| O-4 | Stale-price exploitation: feed goes silent, last good price drifts further from market | Network partition or publisher outage | 🟡 | ✅ Oracle aggregator's staleness window rejects observations older than configured threshold. Insufficient fresh feeds → aggregation fails (returns `Err`) and `funding` / `liquidation` use last cached value with explicit `cached_oracle_price` semantics. | `crates/oracle/src/lib.rs`, [Stage 16d cached aggregate](../crates/oracle/src/lib.rs) |

### Consensus attacks

| # | Scenario | Attacker capability | Severity | Mitigation | Reference |
| :--- | :--- | :--- | :--- | :--- | :--- |
| C-1 | Equivocation: validator votes for two different blocks at the same height | One validator | 🟠 | ✅ Pre-token: detectable from gossip; triggers operator-agreement removal. Post-token: slashable on-chain. | Malachite BFT |
| C-2 | Liveness halt: >1/3 validators offline | Multiple operators down | 🟡 | ✅ BFT correctly halts (no progress) rather than forks. Operator manual specifies SLA + paging. Multi-validator setup verified at N=3 via [`scripts/devnet-3.sh`](../scripts/devnet-3.sh). | [docs/testing.md](./testing.md) |
| C-3 | Fork: >1/3 byzantine validators cause safety violation | Byzantine quorum | 🔴 | ✅ Pre-token: permissioned + legally bound (ADR-008) makes this scenario require coordinated multi-operator misconduct. Detection: external observers monitoring committed-head divergence. | [ADR-008](./adr/008-pre-token-validator-policy.md) |
| C-4 | Censorship: sequencer omits a specific user's transactions | Single sequencer (per ADR-002) | 🟠 | 🚧 **Partial**: pre-v3 there is no consensus-layer censorship resistance. Mitigation is operational (operator agreement + reputation). v3 decentralization is the structural fix. | [ADR-002](./adr/002-sequencer-centralized-then-decentralize.md) |

### Lending economic attacks

| # | Scenario | Attacker capability | Severity | Mitigation | Reference |
| :--- | :--- | :--- | :--- | :--- | :--- |
| L-1 | Borrow against collateral, manipulate oracle to drop collateral price, force own liquidation at bad price | Oracle manipulation (see O-1, O-3) | 🟠 | ✅/🛑 Inherits mitigation status of O-1/O-3. The lending engine itself is correct given honest oracle; the attack vector lives at the oracle layer. | (see Oracle row) |
| L-2 | Repay manipulation: borrower repays partial debt at a different `borrow_index` than expected | None (timing only) | 🟢 | ✅ Aave-style scaled-debt accounting: position stores `scaled_debt` (nominal_debt ÷ borrow_index at borrow time). Repay/withdraw use current index, mathematically consistent. | `crates/lending/src/position.rs` |
| L-3 | Donation attack: send collateral directly to a market account to skew utilization | EVM call surface | 🟢 | ✅ Bridge-owned `Arc<Mutex<BTreeMap<...>>>` is not addressable from EVM accounts; only the six precompiles mutate market state. No "direct token transfer to market" path exists. | `crates/evm/src/live_node.rs` |
| L-4 | Liquidate own position at favorable bonus to extract value (self-liquidation grief) | Any user with capital | 🟢 | ✅ Liquidation bonus (5%) is parameterized; self-liquidation is allowed but the bonus is bounded and applies to all liquidators equally. Economic — not protocol-level — exploit. | LD-003 in [v0 plan](./plans/v0-lending.md#ld-003-health-factor--collateral_value--lt--debt_value) |
| L-5 | Bad debt accumulation: cascade of underwater positions exceeds insurance fund | Sustained market crash | 🟠 | 🚧 **Partial**: `PrincepsNode::absorb_lending_bad_debt` routes shortfall to InsuranceFund. If fund depletes, the bridge surfaces `WithdrawOutcome::Depleted`; **handling beyond depletion is operator policy** (halt new borrows? socialize loss?). v1 will need an explicit ADR. | [Stage 22c](./plans/v0-lending.md), `crates/liquidation/src/insurance_fund.rs` |

### Liquidation / MEV

| # | Scenario | Attacker capability | Severity | Mitigation | Reference |
| :--- | :--- | :--- | :--- | :--- | :--- |
| M-1 | Sandwich at funding settlement: bracket the funding tick with positions sized to extract funding payments | EVM call surface, mempool visibility | 🟡 | 🚧 **Partial**: funding settlement is per-block deterministic; positions are settled in `(account_id)` order. Sandwich requires control of position-open timing across blocks. Mitigation: funding interval is configurable; per-block accrual smooths the surface. | `crates/funding/src/lib.rs` |
| M-2 | Liquidator gas race: multiple liquidators compete to call `lending_liquidate` on the same flagged position | Public mempool | 🟢 | ✅ First-come-first-served by EVM ordering; liquidation bonus is fixed (5%) so the race extracts the bonus, not the underlying. No Dutch auction at v0 (LD-004 explicit choice). | LD-004 in [v0 plan](./plans/v0-lending.md#ld-004-liquidation-as-state-transition-not-auction) |
| M-3 | MEV at oracle update: sandwich the price push with positions sized to profit from the resulting mark | Mempool + validator collusion | 🟡 | 🚧 **Partial**: oracle pushes are not mempool transactions; they are signed observations ingested by the bridge between blocks. Validator collusion required (see O-1). Independent vector from M-1. | [Stage 11b](../crates/oracle/src/lib.rs) |

### EVM precompile / bridge

| # | Scenario | Attacker capability | Severity | Mitigation | Reference |
| :--- | :--- | :--- | :--- | :--- | :--- |
| E-1 | Precompile mutation persists after EVM revert | Contract call that reverts after a precompile call | 🔴 | ✅ `PrincepsRevertGuard` + `BridgeStateSnapshot` snapshot bridge state pre-call, restore on revert. Covers accounts, book, fills, **and** markets + positions (Stage 23 extension). | [Stage 17i](../crates/evm/src/live_node.rs), [Stage 22 revert guard extension](https://github.com/psyto/princeps/commit/main) |
| E-2 | Snapshot inconsistency: bridge restart restores partial state | Process kill mid-write, disk failure | 🟠 | ✅ Snapshot writes are atomic (write to temp + rename); restart loads or starts fresh. Stage 23c portfolio-gated borrow/withdraw use simulate-then-commit so partial application can't leak past a revert. | `crates/evm/src/live_node.rs`, [Stage 13g](../bin/princeps/src/main.rs) |
| E-3 | Lending precompile fuzz exposes overflow / panic that halts the EVM | Adversarial input via Solidity caller | 🟠 | 🚧 **Partial**: pure-compute crates have proptest coverage; precompile boundary has unit tests but no systematic fuzz harness. **Recommended for audit prep.** | `crates/lending/src/`, [docs/testing.md](./testing.md) |
| E-4 | Precompile gas pricing makes a useful call uneconomic, blocking liquidations | Reth gas schedule | 🟡 | 🛑 **Gap**: precompile gas costs are not yet set against benchmark data. Open question in [v0 plan](./plans/v0-lending.md#open-questions--risks). Resolution required before public testnet promotes to L1 mainnet. | (open) |

### Operational

| # | Scenario | Attacker capability | Severity | Mitigation | Reference |
| :--- | :--- | :--- | :--- | :--- | :--- |
| OP-1 | Validator private key exfiltrated from operator host | Host compromise | 🟠 | 🚧 **Partial**: `validator-key.json` written 0o600. Operator manual recommends air-gapped key generation + off-host backup. HSM not required at v0; will be at v1 mainnet (per ADR-008 sunset). | `bin/princeps/src/main.rs:1078` |
| OP-2 | Oracle publisher private key exfiltrated | Host compromise | 🟡 | 🚧 **Partial**: same posture as OP-1 for publisher hosts. Median aggregation tolerates one compromised publisher. | `crates/oracle/src/lib.rs` |
| OP-3 | Bridge state corruption from disk failure goes undetected | Hardware fault | 🟢 | ✅ Bridge snapshot includes a content hash; load mismatch is `eyre::Err` with a clear message. | `crates/evm/src/live_node.rs` |
| OP-4 | Operator runs unsupported / forked software | Operator misconduct | 🟠 | 🚧 **Partial**: ADR-008 operator agreement requires version discipline. No on-chain enforcement until ADR-009. | [ADR-008](./adr/008-pre-token-validator-policy.md) |

## Known gaps

These are the items where the table above shows 🛑 or 🚧 **partial** and the project agrees with the assessment. They are listed here for visibility, not as an admission they will be left unfixed.

1. **Bad-debt depletion policy beyond InsuranceFund (L-5)** — bridge surfaces `WithdrawOutcome::Depleted` but the operator-level "what next" is undefined. Needs ADR before v1 mainnet.
2. **Lending precompile fuzz harness (E-3)** — proptest covers pure-compute; precompile boundary needs an adversarial-input fuzzer. Audit-prep work; targeted at Q4 2026.
3. **Precompile gas pricing (E-4)** — open question in the v0 lending plan. Resolution required before public testnet promotes to L1 mainnet.
4. **Censorship resistance pre-v3 (C-4)** — structural; mitigation is operational only until v3 sequencer decentralization. No interim plan.
5. **HSM / key custody for validators (OP-1, OP-2)** — software-level mitigations in place; HSM requirement deferred to v1 mainnet.

The previously-listed "ETH oracle deviation circuit breaker (O-3)" gap was closed in this commit cycle — see the O-3 row above for the new ✅ status.

## Out of scope

These are decisions that knowingly accept a risk, not gaps to close:

- **Single sequencer at v0–v1** ([ADR-002](./adr/002-sequencer-centralized-then-decentralize.md)) — accepted in exchange for shipping velocity; sunsetted at v3.
- **No protocol token at v0–v1** ([ADR-007](./adr/007-token-none-until-revenue.md)) — accepted; ADR-008 covers the gap during this window.
- **Self-liquidation (L-4)** — economic behavior, not a protocol bug.
- **MEV at funding (M-1)** — bounded by per-block accrual; perfect resistance requires non-deterministic ordering, accepted tradeoff.

## Reviewers / external eyes

This document explicitly invites independent challenge. If you have read it and disagree with a severity rating, a mitigation status, or believe a scenario is missing, open an issue against [github.com/psyto/princeps](https://github.com/psyto/princeps) — adversarial review is the point.

A Q1 2027 audit window is committed in the [README roadmap](../README.md#roadmap) as the gate between v0 testnet and v1 mainnet; this document is the working brief that will be handed to the auditors.

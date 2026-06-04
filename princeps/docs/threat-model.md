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
| O-2 | Single oracle publisher submits malicious observation | One publisher key compromised | 🟡 | ✅ Median-of-medians aggregation + deviation cap filter discard outliers; signed observations require publisher key. | `crates/oracle/src/lib.rs`, [Stage 11b commit (openhl)](https://github.com/psyto/openhl/commit/6495ffd) |
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
| L-5 | Bad debt accumulation: cascade of underwater positions exceeds insurance fund | Sustained market crash | 🟠 | ✅ **All three layers in place**. **Layer 1 (algorithmic halt) ✅**: `PrincepsNode::absorb_lending_bad_debt` routes shortfall to InsuranceFund; `LendingHaltParams` (v0 default 50% running-coverage threshold / 200-block halt / 128-block window) arms `lending_halt_until` at end-of-tick when the fund balance can no longer cover a halt window's typical shortfall; bin/princeps skips the unified scan + bridge-side bad-debt absorption loop while halted. Halt persists across restart via `CoordinatorSnapshot::lending_halt_until`. Repays + liquidations + healthy-position withdraws remain allowed — they reduce risk. **Layer 2 (operator-cap commitment + 72-hour disclosure) ✅ template**: codified in [`operator-agreement.md`](./operator-agreement.md) §4 + §5; template in place, signed instances awaited at v1 mainnet onboarding. **Layer 3 (manual-declaration `socialize_residual`) ✅**: pure-compute primitive in `princeps/crates/lending/src/socialization.rs` mutates `market.supply_index` and `total_supplied` proportionally (`new_supply_index = supply_index × (total_supplied − absorbed) / total_supplied`). Per-account positions are repriced implicitly via the index. Bridge-owned implicit pool absorbs proportionally with depositors (additive model from `b1b5981` makes the conservation hold across the haircut). Admission gated on ECDSA-verified `SocializationDeclaration` (`princeps-node`'s `operator` module); `block_height` field is the replay-protection binding. Event emitted via `ChainHistoryStore` (`princeps/bin/princeps/src/chain_history.rs`) for audit + restart-replay. v0 scope limit: precompile-level borrow-during-halt enforcement is v1 work (same constraint as oracle CB row O-3). Bridge-side wiring to expose `socialize_residual` via CLI / EVM precompile is operational follow-up; the protocol primitive is in place. | [ADR-010](./adr/010-bad-debt-depletion-policy.md), [`operator-agreement.md`](./operator-agreement.md), `princeps/crates/node/src/lib.rs::evaluate_lending_halt`, `princeps/crates/lending/src/socialization.rs::socialize_residual`, `princeps/crates/node/src/operator.rs::verify_socialization_declaration`, `princeps/bin/princeps/src/chain_history.rs`, `crates/liquidation/src/insurance.rs` |

### Liquidation / MEV

| # | Scenario | Attacker capability | Severity | Mitigation | Reference |
| :--- | :--- | :--- | :--- | :--- | :--- |
| M-1 | Sandwich at funding settlement: bracket the funding tick with positions sized to extract funding payments | EVM call surface, mempool visibility | 🟡 | 🚧 **Partial**: funding settlement is per-block deterministic; positions are settled in `(account_id)` order. Sandwich requires control of position-open timing across blocks. Mitigation: funding interval is configurable; per-block accrual smooths the surface. | `crates/funding/src/lib.rs` |
| M-2 | Liquidator gas race: multiple liquidators compete to call `lending_liquidate` on the same flagged position | Public mempool | 🟢 | ✅ First-come-first-served by EVM ordering; liquidation bonus is fixed (5%) so the race extracts the bonus, not the underlying. No Dutch auction at v0 (LD-004 explicit choice). | LD-004 in [v0 plan](./plans/v0-lending.md#ld-004-liquidation-as-state-transition-not-auction) |
| M-3 | MEV at oracle update: sandwich the price push with positions sized to profit from the resulting mark | Mempool + validator collusion | 🟡 | 🚧 **Partial**: oracle pushes are not mempool transactions; they are signed observations ingested by the bridge between blocks. Validator collusion required (see O-1). Independent vector from M-1. | [Stage 11b](../crates/oracle/src/lib.rs) |

### EVM precompile / bridge

| # | Scenario | Attacker capability | Severity | Mitigation | Reference |
| :--- | :--- | :--- | :--- | :--- | :--- |
| E-1 | Precompile mutation persists after EVM revert | Contract call that reverts after a precompile call | 🔴 | ✅ `PrincepsRevertGuard` + `BridgeStateSnapshot` snapshot bridge state pre-call, restore on revert. Covers accounts, book, fills, **and** markets + positions (Stage 23 extension). | [Stage 17i](../crates/evm/src/live_node.rs), [Stage 23 revert-guard extension](https://github.com/psyto/princeps/commit/980c1bc) |
| E-2 | Snapshot inconsistency: bridge restart restores partial state | Process kill mid-write, disk failure | 🟠 | ✅ Snapshot writes are atomic (write to temp + rename); restart loads or starts fresh. Stage 23c portfolio-gated borrow/withdraw use simulate-then-commit so partial application can't leak past a revert. | `crates/evm/src/live_node.rs`, [Stage 13g](../bin/princeps/src/main.rs) |
| E-3 | Lending precompile fuzz exposes overflow / panic that halts the EVM | Adversarial input via Solidity caller | 🟠 | ✅/🚧 **proptest boundary fuzz in place**: `princeps/crates/evm/src/precompiles/mod.rs` ships a proptest harness with 9 properties (one per lending precompile) at 512 cases each — ~4,600 random-byte inputs per `cargo test` run, asserting (a) no panic on any input length / content, (b) 32-byte output shape, (c) all-zero when no process-global state is installed. Plus a boundary-length grid (`#[ignore]`, opt-in via `--ignored`) hitting every precompile across input lengths 0..256. Pure-compute crates (`princeps-lending`, `princeps-node`) already had proptest coverage; this closes the boundary gap. Follow-up: cargo-fuzz / libFuzzer for sustained adversarial fuzzing in the Q4 2026 audit-prep window. | `princeps/crates/evm/src/precompiles/mod.rs` (`fuzz_lending_*_boundary` properties), `crates/lending/src/`, [docs/testing.md](./testing.md) |
| E-4 | Precompile gas pricing makes a useful call uneconomic, blocking liquidations | Reth gas schedule | 🟡 | ✅/🚧 **Differentiated per-precompile costs + criterion benchmark harness landed.** Constants in `princeps/crates/evm/src/precompiles/mod.rs`: HEALTH 1_500 (read-only) → DEPOSIT 2_000 → REPAY/SUPPLY 2_500 → WITHDRAW_SUPPLY 3_000 → BORROW/WITHDRAW_COLL 4_000 → LIQUIDATE 6_000 → SOCIALIZE 8_000. Each handler uses the same constant for success AND zero-error paths so gas pricing doesn't leak validation outcome. Criterion harness at `princeps/crates/evm/benches/lending_precompiles.rs` confirms the relative ordering matches observed timings; 2026-06-04 baseline run found `socialize` is ~1,000× more expensive than the other 8 precompiles (ECDSA verify dominates at ~60µs vs 30-60ns for the rest). Real absolute-value recalibration against a public-testnet reference workload remains pre-mainnet — the bench file's doc comments record the methodology and observed baseline so re-runs on production hardware are reproducible. | `princeps/crates/evm/src/precompiles/mod.rs` (per-precompile `LENDING_*_GAS_COST` constants), `princeps/crates/evm/benches/lending_precompiles.rs` (criterion harness), [v0 plan](./plans/v0-lending.md#open-questions--risks) |

### Operational

| # | Scenario | Attacker capability | Severity | Mitigation | Reference |
| :--- | :--- | :--- | :--- | :--- | :--- |
| OP-1 | Validator private key exfiltrated from operator host | Host compromise | 🟠 | 🚧 **Partial**: `validator-key.json` written 0o600. Operator manual recommends air-gapped key generation + off-host backup. HSM not required at v0; will be at v1 mainnet (per ADR-008 sunset). | `bin/princeps/src/main.rs:1078` |
| OP-2 | Oracle publisher private key exfiltrated | Host compromise | 🟡 | 🚧 **Partial**: same posture as OP-1 for publisher hosts. Median aggregation tolerates one compromised publisher. | `crates/oracle/src/lib.rs` |
| OP-3 | Bridge state corruption from disk failure goes undetected | Hardware fault | 🟢 | ✅ Bridge snapshot includes a content hash; load mismatch is `eyre::Err` with a clear message. | `crates/evm/src/live_node.rs` |
| OP-4 | Operator runs unsupported / forked software | Operator misconduct | 🟠 | 🚧 **Partial**: ADR-008 operator agreement requires version discipline. No on-chain enforcement until ADR-009. | [ADR-008](./adr/008-pre-token-validator-policy.md) |

## Known gaps

These are the items where the table above shows 🛑 or 🚧 **partial** and the project agrees with the assessment. They are listed here for visibility, not as an admission they will be left unfixed.

1. **Lending precompile cargo-fuzz follow-up (E-3)** — proptest boundary harness landed (9 properties × 512 cases at the precompile dispatch layer). For Q4 2026 audit prep, add cargo-fuzz / libFuzzer for sustained adversarial fuzzing on top of proptest's structured generation.
2. **Precompile gas pricing absolute-value recalibration (E-4)** — per-precompile constants landed (9 differentiated values matching computational class). Criterion harness landed (`princeps/crates/evm/benches/lending_precompiles.rs`); 2026-06-04 baseline confirms the relative ordering. Absolute-value recalibration against a public-testnet reference workload (with per-iteration state reset on validator-grade hardware) remains pre-mainnet — the bench's doc comment records the methodology so re-runs are reproducible.
3. **Censorship resistance pre-v3 (C-4)** — structural; mitigation is operational only until v3 sequencer decentralization. No interim plan.
4. **HSM / key custody for validators (OP-1, OP-2)** — software-level mitigations in place; HSM requirement deferred to v1 mainnet.

The previously-listed "ETH oracle deviation circuit breaker (O-3)" gap was closed in an earlier commit cycle — see the O-3 row above. The L-5 "operator policy on InsuranceFund depletion" gap was closed in stages: Layer 1 (algorithmic halt) and Layer 2 (operator agreement, see [`operator-agreement.md`](./operator-agreement.md)) landed first; Layer 3 (manual-declaration socialization) was deferred to follow-up work because its three named dependencies — per-depositor `scaled_supply`, princeps-side chain history, and operator-sig admission — were not yet built at v0. Each of those landed (commits `b1b5981`, `406ba5a`, `906d659`), the pure-compute `socialize_residual` primitive followed (`e886444`), and bridge-side entrypoints (CLI `bd5b21b`, EVM precompile at 0x...0c27 `d6e05d8`, reth-devnet boot wiring `f65a025`) closed the operational loop. All three layers are now fully wired; see the [L-5 row](#lending-economic-attacks) above.

The E-3 "lending precompile fuzz harness" gap was partially closed in this commit cycle by adding a proptest boundary harness (9 properties × 512 cases each). cargo-fuzz / libFuzzer follow-up remains for sustained adversarial fuzzing in the Q4 2026 audit-prep window — see [E-3 row](#evm-precompile--bridge) above for the new ✅/🚧 status.

## Out of scope

These are decisions that knowingly accept a risk, not gaps to close:

- **Single sequencer at v0–v1** ([ADR-002](./adr/002-sequencer-centralized-then-decentralize.md)) — accepted in exchange for shipping velocity; sunsetted at v3.
- **No protocol token at v0–v1** ([ADR-007](./adr/007-token-none-until-revenue.md)) — accepted; ADR-008 covers the gap during this window.
- **Self-liquidation (L-4)** — economic behavior, not a protocol bug.
- **MEV at funding (M-1)** — bounded by per-block accrual; perfect resistance requires non-deterministic ordering, accepted tradeoff.

## Reviewers / external eyes

This document explicitly invites independent challenge. If you have read it and disagree with a severity rating, a mitigation status, or believe a scenario is missing, open an issue at [github.com/psyto/rdk/issues](https://github.com/psyto/rdk/issues) — adversarial review is the point. (The `psyto/princeps` repo this document originally pointed at was archived on 2026-06-04 when active development consolidated into the `psyto/rdk` monorepo; that repo remains read-only for the launch announcement and historical SHAs.)

A Q1 2027 audit window is committed in the [README roadmap](../README.md#roadmap) as the gate between v0 testnet and v1 mainnet; this document is the working brief that will be handed to the auditors.

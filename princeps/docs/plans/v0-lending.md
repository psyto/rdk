# Princeps v0 — Lending build plan

**Status**: Protocol work complete (as of 2026-06-05); public-testnet deployment pending.
**Target**: Q3 2026 public testnet of the lending primitive + cross-margin model
**Scope**: Single asset pair (USDC collateral, ETH borrow) with deterministic sub-second liquidations and portfolio margin. Post-Stage-24 hardening (ADR-010 bad-debt depletion, threat-model E-3/E-4) landed 2026-06-04/05 — see [Progress as of 2026-06-05](#progress-as-of-2026-06-05--post-stage-24-hardening-adr-010--threat-model).

## Progress as of 2026-06-03

| Stage | Status | Tests | Notes |
|---|---|---|---|
| 19a-e (`princeps-lending` crate, pure compute) | ✅ Complete | 61 | 5 modules: types, position helpers, IRM, health, interest accrual |
| 20a-d (bridge integration) | ✅ Complete | 13 | Markets + positions on bridge, lending tick, scan, 4 mutation methods |
| 21a-e (5 EVM precompiles) | ✅ Complete | 4 | deposit / borrow / repay / withdraw / health at `0x0c1f`–`0x0c23` |
| 22a (unified perp+lending scan report) | ✅ Complete | — | `LiveRethEvmBridge::scan_unified_health` joins perp + lending into one surface |
| 22b (liquidation precompile) | ✅ Complete | 5 | `lending_liquidate` bridge method + precompile at `0x0c24` |
| 22c (bad-debt absorption) | ✅ Complete | — | Bridge surfaces shortfall; `PrincepsNode` coordinator routes into `InsuranceFund` |
| 23a (`princeps-portfolio` crate) | ✅ Complete | 14 | Unified cross-margin compute (lending + perp → one health) |
| 23b (bridge unified-margin aggregator) | ✅ Complete | — | `compute_account_health` joins lending positions + perp state into `PortfolioHealth` |
| 23c (portfolio-gated borrow/withdraw) | ✅ Complete | — | Borrow + withdraw_collateral now check portfolio free-equity, not just lending HF |
| 24b (`princeps lending-demo`) | ✅ Complete | — | Alice's prime-broker scenario in ~1s (siloed vs unified verdict) |
| 24c (per-step lending CLI) | ✅ Complete | — | `princeps lending {init,deposit,borrow,repay,withdraw,health,scan,list}` |
| 24d (`princeps-lending-rpc-server`) | ✅ Complete | — | Read-only HTTP JSON RPC over in-process bridge, 5 seeded accounts |
| 24e (sample liquidator bot) | ✅ Complete | — | `princeps-liquidator-bot` — seeds, raises ETH, liquidates most-underwater first |
| 24a (USDC/ETH `reth-devnet` lending genesis) | ✅ Complete | — | `seed_v0_lending_markets` + `seed_v0_demo_accounts` registered on fresh chain; lending_prices=(1,1) wired into per-block scan |
| Multi-validator expansion (3+ validators) | ✅ Complete | — | Code path N-agnostic since Stage 13l; `scripts/devnet-3.sh` boots alice/bob/carol and diffs convergence; coordinator snapshots byte-identical |
| Public testnet deploy | ⏳ Pending | — | Validators, monitoring, faucet |

**Total v0 tests passing**: 464 across 13 crates (61 lending + 14 portfolio + 84 evm + 14 node + 90 liquidation + 22 funding + 57 oracle + 44 vault + 24 clearing + 12 clob + 42 consensus + 0 types/codec).

**v0 lending precompile suite (callable from any Solidity contract):**

| Address | Precompile |
|---|---|
| `0x...0c1f` | `princeps_lending_deposit_collateral` |
| `0x...0c20` | `princeps_lending_borrow` |
| `0x...0c21` | `princeps_lending_repay` |
| `0x...0c22` | `princeps_lending_withdraw_collateral` |
| `0x...0c23` | `princeps_lending_health` (staticcall-safe) |
| `0x...0c24` | `princeps_lending_liquidate` |
| `0x...0c25` | `princeps_lending_supply` (v1 multi-asset foundation, b1b5981/d46f379) |
| `0x...0c26` | `princeps_lending_withdraw_supply` (v1 multi-asset foundation) |
| `0x...0c27` | `princeps_lending_socialize` (ADR-010 Layer 3 EVM entrypoint, d6e05d8) |

## Progress as of 2026-06-05 — post-Stage-24 hardening (ADR-010 + threat-model)

The work below was scheduled as v1 / pre-mainnet hardening but pulled forward into a single push following the 2026-06-03 rdk migration. None of it changes the v0 ship scope below; it landed because the deferral risk was higher than the implementation cost once the migration was complete.

| Workstream | Status | Commits | Notes |
|---|---|---|---|
| ADR-010 Layer 1 — algorithmic lending halt | ✅ Complete | `b1d82f0`, `d293d1d` | `LendingHaltParams` (v0: 50% running-coverage / 200-block halt / 128-block window); `lending_halt_until` persists across restart via `CoordinatorSnapshot`; bin/princeps skips `scan_unified → absorb_lending_bad_debt` while halted. +12 tests in `princeps-node`. |
| ADR-010 Layer 2 — operator agreement | ✅ Template | `139a93c` | `princeps/docs/operator-agreement.md` — 7-section v0 template carrying ADR-008's 4 clause categories + ADR-010's 2 new clauses (§4 lending-halt make-whole, §5 72-hour disclosure). Combined liability cap. Audit-bundle ready; signed instances awaited at v1 mainnet onboarding. |
| ADR-010 Layer 3 — supplier-side foundation | ✅ Complete | `b1b5981`, `d46f379` | `scaled_supply: u128` on `lending::Position`; `nominal_supply` accessor; `position::supply` / `withdraw_supply`; supplier-side accrual (`supply_index` grows per Aave standard); supplier-side EVM precompiles at `0x...0c25` / `0x...0c26`. +9 tests in `princeps-lending`, +6 in `princeps-evm`. |
| ADR-010 Layer 3 — chain history | ✅ Complete | `406ba5a` | `princeps_node::chain_history::ChainHistoryStore` — unified replay + runtime append. `ChainEvent::Socialization` variant. JSON wire format; snapshot/restore for cross-restart audit log. +11 tests in `princeps-node`. |
| ADR-010 Layer 3 — operator-sig admission | ✅ Complete | `906d659` | `princeps_node::operator::{OperatorRegistry, OperatorKey, SocializationDeclaration, verify_socialization_declaration}` — secp256k1 ECDSA; replay-protected via `block_height`. +14 tests. |
| ADR-010 Layer 3 — primitive | ✅ Complete | `e886444` | `princeps_lending::socialize_residual` — pure-compute haircut. Per-account positions repriced via `supply_index`; bridge-implicit pool absorbs proportionally; conservation `sum(per-position nominal) + bridge_implicit ≡ total_supplied` holds across the haircut. +9 tests. |
| ADR-010 Layer 3 — entrypoints | ✅ Complete | `bd5b21b`, `d6e05d8`, `f65a025` | `princeps socialize` CLI subcommand; `princeps_lending_socialize` precompile at `0x...0c27`; boot-time install of `OperatorRegistry` + `ChainHistoryStore` from new `--reth-operator-*` / `--reth-chain-history-file` flags; chain-history persisted across restart alongside the bridge snapshot. +7 tests in `princeps-evm`. |
| Threat-model E-3 — precompile boundary fuzz | ✅/🚧 | `56cd549` | 9 proptest properties × 512 cases each (~4,600 random-byte inputs per `cargo test` run). Boundary-length grid (`#[ignore]`, opt-in). cargo-fuzz follow-up pending Q4 2026 audit prep. |
| Threat-model E-4 — gas pricing | ✅/🚧 | `7fad4d3`, `c71c99b` | Original uniform `LENDING_BASE_GAS_COST = 2_000` replaced with 9 differentiated constants (HEALTH 1_500 → ... → SOCIALIZE 8_000). Criterion harness at `princeps/crates/evm/benches/lending_precompiles.rs`; 2026-06-04 baseline confirms relative ordering. Absolute-value recalibration on validator-grade hardware pre-mainnet. |
| Threat-model L-5 closure | ✅ Complete | (rolled into ADR-010 commits above) | Row flipped from 🚧 Partial to ✅ across all three layers; Known gaps shrunk from 5 → 4. See `princeps/docs/threat-model.md` and `princeps/docs/adr/010-bad-debt-depletion-policy.md` for full audit trail. |

The original plan structure (below) is preserved as the source-of-truth for what each stage covers and the architectural decisions behind them. Update the Progress sections as stages ship.

---

## What v0 ships

- One lending market: **USDC collateral, ETH borrow** (single pair)
- Native EVM precompiles for deposit / borrow / repay / withdraw
- Per-block interest accrual (deterministic, no off-chain keepers)
- Per-block health-factor scan (extends existing liquidation scanner)
- Sub-second liquidation as state transition — no gas auction, no keeper race
- Portfolio margin engine — lending and perp positions share one risk model
- Insurance fund integration for bad debt (reuses Stage 10b primitive)
- Public testnet deployment with CLI demo

## What v0 does NOT ship

- Multi-asset collateral (USDT, BTC, etc.) → v1
- Variable IRM beyond single kink curve → v1+
- E-mode / correlated-asset bonuses → v2+
- Flash loans → v1 (intentional defer, design separately)
- Liquidation auctions (Dutch, etc.) → v1+ (v0 uses bonus-to-liquidator)
- Governance / parameter changes → v1+ (v0 params hardcoded)
- Web UI → indefinite (CLI sufficient)

## Architectural decisions

### LD-001 — Lending lives in a new `princeps-lending` crate

Pure compute (no I/O), following the established pattern of `princeps-clob`, `princeps-funding`, `princeps-liquidation`. Bridge owns per-market state and routes mutations through `apply_*` functions. Reusable model proven by Stages 16a–17k.

### LD-002 — Per-block interest accrual

Interest accrues every block as part of `PrincepsNode::tick`, not per-event.

- Deterministic: every validator computes identical interest
- Gas-cheap reads: position health doesn't need to re-derive interest
- Aave uses per-event which forces re-computation on every interaction; Princeps's per-block model is cleaner because consensus already orders blocks
- Index-based borrow accounting (à la Aave's `borrowIndex`) for O(1) per-position math

### LD-003 — Health factor = (collateral_value × LT) / debt_value

Standard Aave/Compound convention. Liquidation triggers at health < 1.0. v0 hardcoded params:

- USDC LT = 95% (stablecoin)
- USDC collateral haircut = 0%
- ETH borrow oracle = Princeps push oracle (Stage 11b signed observations)
- Liquidation bonus = 5%
- Partial liquidation = 50% of debt per call

### LD-004 — Liquidation as state transition, not auction

When health < 1.0 in per-block scan, position is flagged. Liquidation precompile atomically:

1. Repays X% of debt
2. Receives X% × (1 + bonus) of collateral
3. Position re-evaluated; can be re-liquidated if still < 1

If no liquidator transacts within Y blocks of flagging (Y = 100 blocks ≈ 100s), automatic liquidation routes through insurance fund (Stage 10b mechanism).

**This is the headline**: scan every block, flag immediate, partial liquidation completes in 1 block. Compare to Aave/Compound where keepers gas-race and liquidations take minutes during volatility.

### LD-005 — Cross-margin between lending and perps (the prime broker feature)

Single account health = `Σ(collateral × LT) − Σ(debt) − max(0, −perp_unrealized_pnl)`

- Profitable perp → more borrowing capacity
- Losing perp → less borrowing capacity (toward liquidation if severe)
- Lending collateral backs perp positions and vice versa
- **One unified margin engine across both products**

This is the differentiator vs HL (perp+spot cross-margin, no lending) and Aave (lending, no perp). v0 demonstrates this with the existing CLOB market + new lending market.

### LD-006 — USDC oracle: pegged-$1 with depeg circuit breaker

v0: `USDC = $1.00` hardcoded. Circuit breaker: external feed reports < $0.97 or > $1.03 for >5 min → halt new borrows/withdrawals (existing positions can repay/liquidate). v1+: integrate USDC oracle properly.

### LD-007 — Reserve factor: 10% of interest to insurance fund

10% of interest accrued routes to insurance fund as protocol reserves. 90% accrues to suppliers. This is the kernel of protocol revenue and the eventual basis for any tokenomics decision (per ADR-007: no token until real revenue exists).

## Stage plan

Following the established Stage N[a–k] pattern.

### Stage 19 — Lending markets crate (pure compute)

- **19a** — `Market` struct: `{ underlying, collateral_type, reserves, total_borrowed, total_supplied, irm_params, indices, last_accrual_block }`
- **19b** — `Position` struct: `{ collateral_amount, borrow_amount_shares, last_seen_index }` (index-based)
- **19c** — IRM compute: kinked utilization curve
- **19d** — Health factor compute (pure function)
- **19e** — Interest accrual function (per-block, updates `borrow_index` / `supply_index`)

Expected: ~30–40 tests across market state, IRM curve, health, interest math.

### Stage 20 — Bridge integration

- **20a** — Bridge owns `BTreeMap<MarketId, MarketState>` (single market for v0, multi-market ready)
- **20b** — Bridge owns `BTreeMap<(AccountId, MarketId), Position>`
- **20c** — Per-block tick: `apply_lending_interest` + `scan_lending_health` + `flag_unhealthy`
- **20d** — Bridge methods: `deposit_collateral` / `borrow` / `repay` / `withdraw_collateral`

Expected: ~20–30 tests, bridge mutations + tick integration + restart persistence.

### Stage 21 — EVM precompiles

- **21a** — `princeps_lending_deposit` (collateral in)
- **21b** — `princeps_lending_borrow` (debt out)
- **21c** — `princeps_lending_repay` (debt down)
- **21d** — `princeps_lending_withdraw` (collateral out, health-checked)
- **21e** — `princeps_lending_health` (read-only, callable by contracts)

Expected: ~15–20 tests, Solidity-side calls, revert-safe via `PrincepsRevertGuard`.

### Stage 22 — Liquidation engine extension

- **22a** — Extend liquidation scanner to include lending positions in unified scan loop
- **22b** — `princeps_lending_liquidate` precompile: atomic repay + seize_collateral with bonus
- **22c** — Bad-debt path: positions with health < 0 absorbed by insurance fund (Stage 10b mechanism)

Expected: ~15–20 tests, health → flag → liquidate cycles, bad debt absorption.

### Stage 23 — Cross-margin (prime broker feature)

- **23a** — Portfolio margin engine: `compute_account_health(account)` aggregates lending + perp + collateral
- **23b** — Scanner uses portfolio health, not per-product
- **23c** — Withdraw checks use portfolio health
- **23d** — Demo: deposit USDC → borrow ETH → open ETH perp → show single unified margin

Expected: ~10–15 tests, cross-product margin scenarios.

### Stage 24 — Demo + observability

- **24a** — Single-asset-pair devnet config (USDC/ETH) with 5 seeded accounts
- **24b** — `cargo run --bin princeps -- lending-demo` script: deposit → borrow → price crash → liquidation, <5 seconds end-to-end
- **24c** — CLI subcommands: `princeps lending deposit/borrow/repay/withdraw/health`
- **24d** — RPC endpoints for health / positions / markets
- **24e** — Sample liquidator bot (~100 lines Rust) for demo realism

Acceptance: third party clones repo, runs demo script, witnesses full lifecycle in <1 minute.

## Timeline (rough, single-developer pace)

| Stage | Scope | Weeks | Target |
|---|---|---|---|
| 19 | Lending crate (pure compute) | 2 | 2026-06-15 |
| 20 | Bridge integration | 2 | 2026-07-01 |
| 21 | EVM precompiles | 2 | 2026-07-15 |
| 22 | Liquidation extension | 1.5 | 2026-07-26 |
| 23 | Cross-margin | 1.5 | 2026-08-06 |
| 24 | Demo + observability | 2 | 2026-08-20 |
| Testnet deploy | Validators, monitoring | 2 | 2026-09-03 |
| **v0 ship** | | | **Q3 2026 (September)** |

~13 weeks active build + 2 weeks deployment = **September 2026** target for v0 public testnet.

## Acceptance criteria (v0 "done")

1. `princeps-lending` crate with ≥80 tests
2. Single lending market live on Princeps testnet (USDC/ETH)
3. `princeps lending-demo` runs full lifecycle in <1 minute
4. Sub-second liquidation latency demonstrated under stress
5. Portfolio margin: perp position reduces lending borrowable (and vice versa)
6. README status: `🚧 v0 lending` → `✅ v0 lending`
7. Public testnet faucet operational
8. ≥3 third-party developers complete the demo successfully (external validation)

## Open questions / risks

1. **Block time**: per-block tick assumes ~1s blocks. Liquidation-latency claims depend on this. Verify on the running devnet before locking the "sub-second" framing in marketing.
2. **USDC depeg policy**: hardcoded thresholds vs governance? v0 hardcoded (no governance yet per ADR-007).
3. ~~**Liquidator UX**~~ — closed by Stage 24e (`princeps-liquidator-bot` ships).
4. ~~**Reth EVM precompile gas pricing**~~ — closed by E-4 work: per-precompile constants in `7fad4d3` + criterion harness in `c71c99b`. Absolute-value recalibration on validator-grade hardware remains pre-mainnet (see threat-model E-4 ✅/🚧 status).
5. **Multi-market readiness**: v0 ships single-market but data structures (`BTreeMap<MarketId, ...>`) must not force a refactor for v1 multi-market. Validated during Stage 19/20 design + reinforced by `b1b5981`'s additive `scaled_supply` foundation (per-asset positions and accrual already index by `MarketId`).

Newer items added since 2026-06-04 (post-Stage-24 hardening):

6. **ADR-009 (tokenomics + on-chain stake)** — explicitly reserved in [ADR-008](./../adr/008-pre-token-validator-policy.md) §4 and [ADR-010](./../adr/010-bad-debt-depletion-policy.md) §Forecloses. Supersedes Layer 2 (operator-cap commitment) and Layer 3's manual declaration gate at v3+. Drafting deferred until tokenomics design starts.
7. **L-5 absolute liquidity bounds for socialization** — Layer 3's `socialize_residual` haircuts proportionally with no per-market cap. If a single-block oracle attack drives `unfilled` larger than depositors should plausibly absorb in one event, an additional cap (e.g., max 10% of `total_supplied` per declaration) would protect against operator-declaration abuse. Not a v0 blocker; revisit at v1 multi-asset onboarding when external depositors actually appear.
8. **Precompile-level halt enforcement** — both the oracle circuit breaker (threat-model O-3) and the lending halt (Layer 1) currently short-circuit only the coordinator-driven loop. Precompile-level enforcement (revert on borrow/withdraw during halt) is the same single piece of plumbing for both: precompiles need to read coordinator state. v1 work; named in ADR-010 §Tradeoffs and the LendingHaltParams doc comment.

## Suggested next action

**Public-testnet deploy preparation** is the natural next step. Everything the v0 ship scope listed above is now landed end-to-end, including the post-Stage-24 hardening that this plan didn't originally schedule. The remaining items on the "What v0 ships" list — public testnet deploy with validators, monitoring, faucet — are deployment work, not protocol work. Plan that next.

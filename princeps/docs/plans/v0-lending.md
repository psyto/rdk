# Princeps v0 — Lending build plan

**Status**: In progress, ~8 weeks ahead of plan
**Target**: Q3 2026 public testnet of the lending primitive + cross-margin model
**Scope**: Single asset pair (USDC collateral, ETH borrow) with deterministic sub-second liquidations and portfolio margin

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

The original plan structure (below) is preserved as the source-of-truth for what each stage covers and the architectural decisions behind them. Update this Progress section as stages ship.

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
3. **Liquidator UX**: sample liquidator bot in Stage 24e to make demo compelling — committed.
4. **Reth EVM precompile gas pricing**: need research for realistic gas. Open question for Stage 21.
5. **Multi-market readiness**: v0 ships single-market but data structures (`BTreeMap<MarketId, ...>`) must not force a refactor for v1 multi-market. Validate during Stage 19/20 design.

## Suggested first action

**Stage 19a — define `Market` and `Position` struct types** in a new `crates/lending/` directory, add workspace member, basic property tests. ~2 days of work, foundation for everything else. Mirrors how `princeps-clob` and `princeps-funding` started.

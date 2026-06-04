# ADR-010 — Bad-debt depletion policy

**Status**: Accepted (2026-06-04)
**Scope**: v0–v1 lending (until tokenomics + on-chain stake supersede)

> ADR-009 is reserved for tokenomics + on-chain slashing per the sunset clause in [ADR-008](./008-pre-token-validator-policy.md). This ADR uses the next free number.

## Context

The threat model row [L-5](../threat-model.md) ("bad-debt cascade exceeds insurance fund") is the most consequential 🚧 **Partial** entry in the lending section. Today's behavior:

1. `LiquidationScanner::scan_unified` flags underwater positions.
2. The bridge closes them, calling [`PrincepsNode::absorb_lending_bad_debt`](../../crates/node/src/lib.rs) (princeps `crates/node/src/lib.rs:381`).
3. That routes the shortfall to `InsuranceFund::withdraw_shortfall`, which returns one of:
   - `Covered { amount }` — fund had enough.
   - `PartiallyDrained { amount, unfilled }` — fund covered part; `unfilled` is the residual.
   - `Depleted { unfilled }` — fund was already empty; the full shortfall is unfunded.

The first outcome is the happy path. The other two leave the bridge holding a non-zero `unfilled` value and no defined protocol response. The doc-comment on `absorb_lending_bad_debt` itself flags this: "Stage 10d ADL territory or **protocol-level halt**" — i.e. the policy lives in this ADR.

This matters concretely because:

- **Depositor exposure**. Lenders supplied USDC against the implicit contract "your principal is protected by the insurance fund." Once the fund is empty, that contract is violated silently — no protocol-defined response — until the operator notices.
- **Audit readiness**. The Q1 2027 audit window ([README roadmap](../../README.md), gated v0→v1) will ask "what is the depletion policy?" An answer of "operator discretion" is not credible for an L1 handling real lending.
- **Pairs with the oracle circuit breaker**. ADR-008's companion commit (`oracle_halt_until`, `CircuitBreakerParams`) established the *mechanism* for protocol-level algorithmic halts. Bad-debt depletion is the second instance of the same pattern and should reuse it.

## Decision

For v0–v1 (until tokenomics ships), depletion is handled by a **two-layer policy**: an algorithmic halt enforced by the protocol, plus an operator-capitalization commitment enforced by the ADR-008 operator agreement. v3 (post-token) replaces the operator layer with stake-backed socialization but keeps the algorithmic halt.

### Layer 1 — algorithmic lending halt (protocol-enforced)

- A new `LendingHaltParams` struct on `PrincepsNodeConfig`, mirroring `CircuitBreakerParams`:
  - `depletion_threshold_bps: u32` — minimum InsuranceFund balance below which the halt arms, expressed in basis points of `fund.recent_avg_shortfall_per_block * halt_duration_blocks` (i.e. "we need at least N halt-windows of running coverage; if not, halt"). v0 default: `5_000` (50% of one halt window's worth of typical shortfall). `0` disables the guard.
  - `halt_duration_blocks: u64` — how long the halt persists once tripped. v0 default: `200` (≈ 200s at 1s blocks — long enough for operator-cap to react, short enough that benign blips clear themselves).
- A `lending_halt_until: Option<u64>` field on `CoordinatorSnapshot` and `PrincepsNode`, persisted across restart in the same way as `oracle_halt_until` (`#[serde(default)]` for old-snapshot compatibility).
- `PrincepsNode::tick` evaluates the depletion check at the end of each block, after `absorb_lending_bad_debt` would have run. If `InsuranceFund::balance() < threshold`, set `lending_halt_until = block_height + halt_duration_blocks`. Each subsequent block that still fails the check re-arms to the new maximum (a sustained shortfall extends rather than expires).
- A `PrincepsNode::is_lending_halted(block_height) -> bool` accessor, symmetric with `is_oracle_halted`.
- The `bin/princeps` per-block hook **skips the `scan_unified → absorb_lending_bad_debt` pass while halted** — the same exit pattern already used for `is_oracle_halted`. The motivation differs (oracle: untrusted price; lending: empty backstop) but the mechanic is identical.

### Layer 2 — operator-cap commitment (operator-agreement-enforced)

Per [ADR-008](./008-pre-token-validator-policy.md), validator operators are legally bound by an off-chain agreement. ADR-010 amends that agreement with two clauses:

1. **Make-whole obligation.** During a `lending_halt_until` window the operator-of-record commits to either (a) re-capitalize the InsuranceFund from operator treasury before the halt expires, or (b) trigger Layer-3 socialization (below) with public disclosure to depositors.
2. **Disclosure obligation.** Halt events are public (the snapshot already exposes them); the operator publishes a post-mortem within 72 hours of any halt that triggered Layer 3.

Operator solvency is not a protocol-enforceable property at v0–v1; this is the same trust posture ADR-008 already established for validator misbehavior. Mitigation is reputational + legal, sunsetted at v3.

### Layer 3 — socialized loss (fallback, requires operator declaration)

> **Implementation deferred (recorded 2026-06-04).** When this section was written, Layer 3 was specified against a v1+ surface that doesn't exist at v0:
>
> 1. **No per-depositor positions.** `lending::Position` tracks `collateral_amount` + `scaled_debt` (borrower-side only). No `scaled_supply` field. v0's supply side is the bridge itself — a pre-funded, bridge-owned pool. `accrual.rs` is explicit: "v0 ships without supplier-side `supply_index` accounting."
> 2. **No princeps-side chain history.** The per-block event replay the section references lives in `openhl/bin/openhl/src/main.rs::chain_history`. Princeps hasn't pulled it across.
> 3. **No operator-sig admission infrastructure.** The closest pattern is `oracle::PublisherKey` ECDSA — per-publisher, not per-operator. Layer 2's operator agreement ([`operator-agreement.md`](../operator-agreement.md)) is legal-only, not cryptographic.
>
> v0 therefore has **no third-party depositor population to socialize against** — the bridge-owned pool absorbs any residual that Layer 2 declines, which is operationally equivalent to operator-cap. The three infrastructure dependencies above are themselves a substantial design space (per-asset positions, princeps-side chain history, operator-key registry) that should not be sneaked in via Layer 3. Implementation is gated on the v1 multi-asset / scaled_supply work (currently outside the v0 lending plan); the surface specified below is the design target for that work to satisfy.
>
> Until that point, Layer 1 + Layer 2 fully cover the v0 threat surface for L-5 — see threat-model row L-5.

If the operator declares Layer 2 cannot cover, lender principal absorbs the residual `unfilled` amount pro-rata across all USDC depositors at the moment of declaration. The mechanics are:

- A `socialize_residual(unfilled: u128) -> SocializationReport` call on `PrincepsNode`, gated behind a `LendingMarketState::accept_socialization(operator_sig)` admission check.
- The lender share-price (`borrow_index` analogue for the supply side, computed from total scaled supply ÷ total supply value) takes a one-time haircut equal to `unfilled / total_supply_value`. Existing positions are repriced atomically at the end of the declared block.
- A `SocializationEvent` is appended to chain history (per-block event replay, openhl Stage 21 pattern).

Socialization is *intentionally* manual at v0–v1 — automating it before tokenomics would make the protocol a worse instrument for depositors (silent haircuts on the supply side are exactly what Aave's Safety Module was designed *not* to do without governance). The operator declaration is the human-in-the-loop pre-token equivalent of governance.

### Scope at v0 — known limit

At v0 the halt only short-circuits the **coordinator-driven** loop. The lending precompiles do not currently read coordinator state, so a borrow precompile call during the halt window will still succeed (and create more debt). Precompile-level enforcement is v1 work — same limitation noted in the oracle circuit-breaker doc-comment (princeps `crates/node/src/lib.rs:88-92`). Documented so auditors see the scope of the v0 mitigation honestly.

What halt does NOT prevent (intentionally):
- **Repays**: always allowed, mirrors oracle-halt — repay reduces system risk.
- **Liquidations**: allowed, draining flagged underwater positions is the whole point of catching them.
- **Withdraws from healthy positions**: allowed; blocking them would be punitive to non-bad-debt-causing users.

## Rationale

- **Reusing `CircuitBreakerParams` shape is the right level of abstraction.** Bad-debt depletion is conceptually a second circuit breaker (different trip condition, identical halt machinery), so it inherits the proven snapshot-persistence, sustained-spike-extends-rather-than-expires, and at-end-of-tick evaluation pattern from the oracle breaker. New code is mostly threading + a new threshold check.
- **Three layers track the trust gradient**. Layer 1 is the only one the protocol controls algorithmically; Layer 2 leverages ADR-008's existing operator-agreement infrastructure; Layer 3 admits that pre-token socialization requires governance-shaped human judgment, and the only governance available pre-token is the operator agreement. This composes cleanly with the rest of the v0–v1 ADR stack.
- **The "Stage 10d ADL territory or protocol-level halt" code-comment** in `absorb_lending_bad_debt` already signaled this design space. Perp ADL (auto-deleveraging on losing counterparties) and lending socialization (haircut on depositors) are different mechanisms — perp ADL has a natural counterparty to absorb the loss; lending does not. So lending uses socialization rather than ADL.
- **Threshold-based trip avoids waiting for full depletion**. By the time `InsuranceFund::balance == 0` the bridge has already silently approved unfunded liquidations. Tripping earlier (when remaining balance can no longer fund a halt-window's worth of typical shortfall) leaves a buffer for the human-decision Layer 2/3 path.
- **The 50%-of-halt-window heuristic** is configurable, not load-bearing. Real number tunes during testnet. The point is to express the threshold as "running coverage time" rather than a USD figure that drifts with chain TVL.

## Tradeoffs

- **A halted lending engine is a UX failure**. Borrowers lose access to a v0 product feature for the halt-window duration. Mitigation: short default window (200 blocks ≈ 200s); operator-cap (Layer 2) can restore service within the window without invoking Layer 3.
- **Threshold false-positives on benign liquidation clusters** are possible — a one-off large liquidation might drain enough InsuranceFund to trip the breaker even though steady state is fine. Mitigation: `recent_avg_shortfall_per_block` is a rolling window that smooths over short bursts; tune in testnet.
- **Operator-cap is not protocol-enforceable**. An undercapitalized operator can't make depositors whole regardless of what the agreement says. Mitigation: same as ADR-008 — prefer operators whose balance sheet is publicly known, document the dependency. v3 sunsets via on-chain stake.
- **Socialization is publicly toxic**. Depositors taking unannounced haircuts is exactly the "DeFi lending blew up" story. Mitigation: require operator declaration + 72-hour public post-mortem (Layer 2 obligation 2). The protocol's algorithmic Layer 1 buys the window in which the human decision happens publicly rather than silently.
- **Precompile-level enforcement is v1 work**. At v0 a determined borrower can still open a position during a halt window via direct precompile call. Mitigation: documented; same scope as the existing oracle circuit breaker (princeps `crates/node/src/lib.rs:88-92`); the v1 work is the same change in both cases (precompiles read coordinator state).
- **Layer 3 specified before its dependencies exist.** The Decision text for Layer 3 references infrastructure (scaled_supply, princeps chain history, operator-sig admission) that is not built. Acceptable at v0 because the supply side is the bridge-owned pool — there is no third-party depositor population to socialize against, so the gap is benign for v0 and the dependencies are themselves outside the v0 lending plan. At v1 multi-asset when scaled_supply lands, Layer 3 implementation must follow before depositors are exposed.

## Forecloses

Nothing permanent. ADR-009 (tokenomics + on-chain stake, forthcoming) supersedes Layer 2 by making operator-cap a stake-backed protocol commitment rather than an off-chain promise, and supersedes Layer 3's manual-declaration gate by routing socialization through on-chain governance. Layer 1 (algorithmic halt) survives both transitions unchanged — the trip condition and mechanic are independent of trust assumptions about who handles the residual.

## Implementation pointers

Layered status as of 2026-06-04:

- **Layer 1 ✅ landed** (`b1d82f0`):
  - `princeps/crates/node/src/lib.rs` — `LendingHaltParams`, `lending_halt_until`, `is_lending_halted`, snapshot serde.
  - `princeps/crates/node/src/lib.rs::PrincepsNode::tick` — post-vault-MTM check + arm logic; `TickReport::lending_halt_tripped_until`.
  - `princeps/bin/princeps/src/main.rs` — skips the `scan_unified → absorb_lending_bad_debt` block while `is_lending_halted`.
  - 12 new tests in `princeps-node` (arm/extend/expiry/burst-rollout/snapshot-roundtrip/serde-default/accumulator).
- **Layer 2 ✅ landed** (`139a93c`):
  - Operator agreement [`princeps/docs/operator-agreement.md`](../operator-agreement.md) — §4 lending halt make-whole, §5 72-hour disclosure. v0 template, signed instances awaited at v1 mainnet onboarding.
- **Layer 3 deferred** to v1 multi-asset / `scaled_supply` work — see the Decision > Layer 3 deferral note above. Gated on: `scaled_supply` field on `lending::Position`, princeps-side chain history (port the openhl Stage 21 pattern), and operator-sig admission infrastructure (likely ECDSA against an operator-key registry, mirroring `oracle::PublisherKey`).
- **Threat-model L-5 row**: updated 2026-06-04 to reflect Layer 1 ✅ + Layer 2 ✅ + Layer 3 deferred.
- **`princeps/docs/plans/v0-lending.md`** — Layer 1/2 status entries TBD; the "Open questions / risks" L-5 mention can be marked resolved-for-v0 once this ADR's deferral note is accepted.

Estimated scope: ~300 LOC + tests, mirrors the `oracle_halt_until` change as a baseline.

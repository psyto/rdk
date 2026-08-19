# princeps

**EVM Prime Broker Sandbox engine.** A deterministic, Reth + Malachite L1 implementation of a unified cross-margin prime broker (lending + perps + future options + structured products on one shared risk engine), designed to make shared-risk system behavior explorable before commitment.

This is the engine behind Fabrknt's [EVM Prime Broker Sandbox](https://fabrknt.com/evm-prime-broker.html). Per `fabrknt/website/CONCEPT.md`, the sandbox exists so engineering teams, risk officers, and product owners can study how shared-risk systems behave under stress — cross-margin liquidations, bad-debt cascades, insurance-fund socialization, oracle stress — by running scenarios rather than reading code.

## Two surfaces

Princeps has two distinct audiences in the same repo. The README sections below mirror that split:

- **Sandbox mode** — for exploration. The audience is someone deciding whether this design is worth adopting or contracting around. Runs in seconds, no infra.
- **Operator mode** — for actually running a Princeps validator. The audience is a counterparty preparing to sign an operator agreement and deploy. Requires keys, infra, and an operational posture.

Most visitors start in sandbox mode and never need operator mode. Operator-mode docs sit alongside in `docs/` and are visible — see [Operator surface](#operator-surface) below.

## What it is (engine subsystems)

`princeps` composes:

- **Reth** as the execution layer (full EVM, library-style).
- **Malachite** as the BFT consensus layer (Tendermint-style finality).
- Pure deterministic state machines: CLOB, funding, liquidation (margin + insurance fund + ADL), oracle (signed observation aggregation + freshness + circuit breaker), vault, clearing (per-account bookkeeping), **lending** (per-market reserves, kinked IRM, Aave-style scaled debt), and **portfolio** (cross-margin compute across lending + perps in one number).
- An integration coordinator (`PrincepsNode::tick`) that runs the per-block routine in one deterministic order: oracle refresh → liquidation scan → ADL absorption → vault mark-to-market → funding settlement → lending bad-debt absorption.

The **prime broker thesis** lives in the `portfolio` crate: a profitable perp expands lending capacity, a losing perp shrinks it — one shared free-equity number, not three siloed risk engines.

Architecture detail: `docs/architecture.md`.

## Sandbox surface (start here)

### Discover

```bash
# List the curated scenarios with their headlines.
princeps scenario list

# Inspect one scenario without running it.
princeps scenario show cross-margin-survival

# Run the scenario: v2-eligible scenarios (every step parses to
# an in-process target — `lending-demo`, `irm-curve-demo`, or any
# of the `lending` sub-commands the walkthrough exercises) dispatch
# in-process and render the 5-section output contract (HEADLINE /
# TIMELINE / DELTA / OUTCOMES / NEXT). The rare v1 scenario spawns
# each step as a sub-process with stdio inherited.
princeps scenario run cross-margin-survival

# Pass --dry-run to print the step list without executing.
princeps scenario run cross-margin-survival --dry-run
```

Five scenarios ship today (all v2 in-process):
- **`cross-margin-survival`** (stress) — canonical prime broker thesis at 10% crash; ✓ all 4 outcomes verify.
- **`cross-margin-edge`** (stress) — mid-stress at 5% crash; ✓ all 5 outcomes verify.
- **`cross-margin-fail`** (stress) — 50% deep crash, unified still HEALTHY; ✓ all 3 outcomes verify.
- **`lending-irm-curve`** (walkthrough) — 11-point IRM curve sweep (0% → 100% utilization in 10% steps) using `princeps_lending::compute_borrow_rate`; surfaces the 80% kink + 11x slope-jump factor; ✓ all 5 outcomes verify.
- **`manual-lending-walkthrough`** (walkthrough) — hands-on `lending init / deposit / borrow / health / scan / list` walkthrough against a shared in-memory `LiveRethEvmBridge<()>`; `--state-file` is parsed and discarded so nothing is written to disk during a v2 run; ✓ all 5 outcomes verify.

Dial flags on `scenario run`: `--eth-crash-price <N>` overrides any `lending-demo` step; `--ltv`, `--liquidation-penalty`, `--oracle-shock` tune the lending market; `--rounds` extends the per-tick walkthrough. Lets a buyer ask "what changes if the crash were 80 instead of 90?" without editing the JSON.

### Drive the underlying CLI directly

```bash
# 1-second full lifecycle: deposit → borrow → ETH crash → cross-margin survives or doesn't.
princeps lending-demo

# Step-by-step lending interactions.
princeps lending init
princeps lending deposit --account alice --amount 1000
princeps lending borrow --account alice --amount 500
princeps lending health --account alice
princeps lending scan

# In-memory single-validator: smallest runnable demo of the full per-block flow.
princeps devnet --rounds 5

# Production-shape boot: Reth + Malachite + bridge + JSON-RPC on 127.0.0.1:8545.
princeps reth-devnet --rounds 20

# Three-validator real-consensus devnet (alice / bob / carol).
./scripts/devnet-3.sh
```

Adjacent binaries:

- `princeps-lending-rpc-server` — HTTP JSON-RPC over an in-process bridge, **plus a browser demo of cross-protocol margin** at `http://localhost:8080`. A trader enters a market-neutral book (ETH lent on Aave, hedged with a short ETH perp on Hyperliquid); the page shows each venue liquidating its own siloed leg while Princeps holds the netted book — every Princeps number computed live by the real `princeps-portfolio` kernel via `GET /portfolio/health`. Includes a replay of the real Aug 5 2024 ETH crash path. The fastest way to see prime broker mechanics from outside Rust. See [`bin/lending-rpc-server/README.md`](bin/lending-rpc-server/README.md).
- `princeps-liquidator-bot` — sample keeper demonstrating the liquidation flow.
- `princeps-faucet` — devnet ETH faucet (operator-mode artifact, used by `docs/operator-manual/faucet-deployment.md`).

## Sandbox elements: current state

Per `fabrknt/website/SANDBOX-PATTERN.md`, every Fabrknt sandbox must ship five elements. Here is Princeps's current state:

| Element | Status | Notes |
|---|---|---|
| (1) Pre-baked scenarios | **present** | `scenarios/` directory with 5 scenarios spanning the shared taxonomy (3 stress, 2 walkthrough). All five take the v2 path. |
| (2) Business-readable output | **v2 done across all scenarios** | All five scenarios dispatch in-process into their respective `run_*_structured` entry points (`run_lending_demo_structured`, `run_irm_curve_demo_structured`, plus the per-step `LendingStep` runner for the walkthrough) and emit the full 5-section contract: HEADLINE (✓/⚠/unverified badge per `expected_outcomes` verification), TIMELINE, DELTA, OUTCOMES, NEXT. 22 outcomes verify ✓ across the five scenarios. |
| (3) Parameter dial | **done** | Four CLI dials on `scenario run` surface the value-relevant knobs: `--eth-crash-price` (lending-demo crash mark), `--ltv`, `--liquidation-penalty`, `--oracle-shock`. Dials take precedence over scenario JSON params over compiled defaults. |
| (4) Scenario replay | **present** | Each scenario file is a deterministic step list — re-running yields the same sub-process invocations. State persistence across steps is the responsibility of the underlying CLI (`lending` writes to `~/.princeps/lending-state.json`). |
| (5) CTA | **done** | `scenario list` / `show` / `run` all render a three-option CTA footer (adopt engine / custom build / hosted access). |

## Operator surface

These are the artifacts for actually running a Princeps validator. They are not part of the sandbox-mode entry experience.

- [`docs/operator-agreement.md`](docs/operator-agreement.md) — v0 operator commitments (equivocation, censorship, oracle manipulation, sustained absence, bad-debt make-whole). Template; signed instances awaited at v1 mainnet onboarding.
- [`docs/operator-manual/`](docs/operator-manual/) — faucet deployment, faucet keys, operator keys, oracle publisher keys.
- [`docs/threat-model.md`](docs/threat-model.md) — full risk inventory, mitigations, audit-ready row format. Cited from `operator-agreement.md` and from code comments throughout `crates/`.
- [`docs/plans/`](docs/plans/) — per-version production plans (`v0-lending.md`, `v0-testnet-deploy.md`).
- [`docs/announcements/`](docs/announcements/) — launch communications. **Note:** `2026-06-01-launch.md` predates the Fabrknt sandbox repositioning. Its "from reference implementation to production platform" language reflects the framing at that publish date, not the current Fabrknt-aligned framing in this README. The historical record is preserved as-is per its own status note.

The threat model and operator agreement are public on purpose: they're evidence that the system is being designed rigorously enough to operate, not just demo. A sandbox-mode visitor who wants to know "is this serious?" can read them.

## Build

```bash
cargo check                          # workspace
cargo test --workspace               # full suite (584+ tests)
cargo run --bin princeps -- --help
cargo run --bin princeps -- lending-demo
```

## Related

- [`fabrknt/website/CONCEPT.md`](../../fabrknt/website/CONCEPT.md) — the Fabrknt brand and 2x2 sandbox structure.
- [`fabrknt/website/SANDBOX-PATTERN.md`](../../fabrknt/website/SANDBOX-PATTERN.md) — cross-engine spec for the five sandbox elements.
- [`../README.md`](../README.md) — the parent `rdk` monorepo and the shared `rdk-*` crates Princeps consumes.
- [`../openhl/README.md`](../openhl/README.md) — sibling engine, EVM Perp Sandbox.
- [`docs/architecture.md`](docs/architecture.md) — subsystem detail, lending flow, portfolio cross-margin.
- [`docs/adr/`](docs/adr/) — load-bearing architectural decisions (consensus choice, sequencer posture, oracle quorum, bad-debt policy, …).

## License

Apache-2.0 / MIT (inherited from `rdk`).

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

# Run the scenario: each step is spawned as a sub-process with
# stdio inherited so output streams live. Wrapped with a headline
# header, per-step separators, and a final verdict + CTA.
princeps scenario run cross-margin-survival

# Pass --dry-run to print the step list without executing.
princeps scenario run cross-margin-survival --dry-run
```

Three scenarios ship today: `cross-margin-survival` (the canonical prime broker thesis at the canonical crash depth), `cross-margin-fail` (negative control at deeper crash), `manual-lending-walkthrough` (hands-on lending CLI walkthrough).

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

- `princeps-lending-rpc-server` — read-only HTTP JSON-RPC over an in-process bridge with 5 pre-seeded accounts. The fastest way to see prime broker mechanics from outside Rust.
- `princeps-liquidator-bot` — sample keeper demonstrating the liquidation flow.
- `princeps-faucet` — devnet ETH faucet (operator-mode artifact, used by `docs/operator-manual/faucet-deployment.md`).

## Sandbox elements: current state

Per `fabrknt/website/SANDBOX-PATTERN.md`, every Fabrknt sandbox must ship five elements. Here is Princeps's current state:

| Element | Status | Notes |
|---|---|---|
| (1) Pre-baked scenarios | **present** | `scenarios/` directory with 3 scenarios (`cross-margin-survival`, `cross-margin-fail`, `manual-lending-walkthrough`). More to follow as oracle-stale and ADL-cascade behaviors get scripted. |
| (2) Business-readable output | **partial** | `scenario run` spawns each step as a princeps sub-process with stdio inherited; the underlying `lending-demo` happens to print a business-readable siloed-vs-unified table, but the manual-lending-walkthrough steps stream raw operator output (PDA addresses, tx hashes, account fields). Scenario wrapper adds only a header + per-step pass/fail verdict, not a business-readable layer. True scenario-level outcome rendering (named verdict, before/after state diff in non-operator language) is v2 work. |
| (3) Parameter dial | partial | `lending-demo --eth-crash-price <N>` already exposes the canonical dial. Per-step parameter overrides on `scenario run` (margin bps, oracle staleness) land in v2. |
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

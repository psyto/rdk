# openhl

**EVM Perp Sandbox engine.** A deterministic, Reth + Malachite L1 implementation of perpetual-futures mechanics, designed to make perp DEX behavior explorable before committing to an implementation.

This is the engine behind Fabrknt's [EVM Perp Sandbox](https://fabrknt.com/evm-perp.html). Per `fabrknt/website/CONCEPT.md`, the sandbox exists so that engineering teams, researchers, and product owners can study how a perp system behaves under stress — liquidation cascades, oracle staleness, funding extremes, ADL — by running scenarios rather than reading code.

## What it is

`openhl` composes:

- **Reth** as the execution layer (full EVM, library-style integration).
- **Malachite** as the BFT consensus layer (Tendermint-style finality).
- Five pure deterministic state machines on top: CLOB, funding, liquidation (margin + insurance fund + ADL), oracle (signed observation aggregation + freshness), vault, and clearing (per-account bookkeeping).
- An integration coordinator (`OpenHlNode::tick`) that runs the per-block routine in one deterministic order: oracle refresh → liquidation scan → ADL absorption → vault mark-to-market → funding settlement.

Architecture detail lives in `docs/architecture.md`. Determinism rules (no `SystemTime::now`, no `HashMap` iteration order, no `rand`) are enforced by `unsafe_code = forbid` plus dependency review.

## What it is NOT

- Not a production exchange. Not deployed. No fee capture, no governance, no token.
- Not a turnkey "white-label exchange" — there is no operator dashboard, no front-end, no KYC.
- Not pedagogy. The depth-and-learning surface for Reth and the broader Rust Ethereum stack lives at RethLab; `openhl` is the product-facing sandbox engine.

## How to explore (today)

### Sandbox surface (recommended starting point)

```bash
# Discover what scenarios exist.
openhl scenario list

# Inspect one scenario without running it.
openhl scenario show cascade

# Get the equivalent reth-devnet invocation to execute the scenario.
# (v0 prints the command; v1 will execute in-process and render a
# headline + before/after diff. See bin/openhl/src/scenario.rs.)
openhl scenario run cascade
```

Three scenarios ship today: `cascade` (5-trader multi-block liquidation cascade), `single-block-cascade` (same cascade compressed into one block), `calm-baseline` (control: two balanced traders, no liquidations).

### Raw devnet surfaces (for deeper inspection)

```bash
# Static view: print config + initial state.
openhl info

# In-memory single-validator: smallest runnable demo of the full per-block flow.
openhl devnet --rounds 5

# Production-shape boot: Reth + Malachite + bridge + JSON-RPC.
openhl reth-devnet --rounds 20

# Replay a scenario across multiple blocks.
openhl reth-devnet --chain-history scenarios/cascade.json --rounds 5

# Boot with a pre-seeded market shape (legacy seed-fixture format).
openhl reth-devnet --seed-fixture seed-default.json --rounds 20
```

### Observation surfaces

- **JSON-RPC `openhl_*` namespace** (port `8545` alongside `eth_*`): `currentMark`, `oracleIndexPrice`, `effectiveMark`, `accounts`, `accountSnapshot`, `marginHealth`, `liquidationParams`, plus WebSocket subscriptions for mark and margin-health changes.
- **`scenarios/*.json`** — sandbox-mode scenario manifests (metadata + chain-history events + suggested params). The format wraps the raw `chain-history` shape; see `bin/openhl/src/scenario.rs`.
- **`chain-history-default.json` / `seed-default.json`** — raw legacy fixtures preserved at the repo root for backward compatibility.

A worked end-to-end shell example (boot, send a raw transaction, poll the receipt) lives in `eth-sendrawtx-demo.sh`.

## Sandbox elements: current state

Per `fabrknt/website/SANDBOX-PATTERN.md`, every Fabrknt sandbox must ship five elements. Here is `openhl`'s current state:

| Element | Status | Notes |
|---|---|---|
| (1) Pre-baked scenarios | **partial → present** | `scenarios/` directory with 3 named scenarios (`cascade`, `single-block-cascade`, `calm-baseline`). 2 more pending (oracle stale, ADL trigger) once the JSON format extends to oracle observations. |
| (2) Business-readable output | partial | `scenario show` and `scenario list` render headline + metadata in ASCII tables. `scenario run` v0 prints the equivalent `reth-devnet` invocation. Embedded execution with headline + before/after delta lands in v1. |
| (3) Parameter dial | partial | `--liquidation-params` and `--validators` exist as `reth-devnet` flags. Per-scenario params (margin bps, fee bps) are baked in but not yet surfaced as CLI overrides on `scenario run`. |
| (4) Scenario replay | **done** | `chain-history` (now via `scenarios/*.json` wrapper) is the replay format. Deterministic, bit-identical across runs. |
| (5) CTA | **done** | `scenario list` / `show` / `run` all render a three-option CTA footer (adopt engine / custom build / hosted access). |

## Roadmap to full v0 sandbox

1. ✅ Promote fixtures to `scenarios/` with named manifests.
2. ✅ Add the `scenario list` / `show` / `run` CLI surface.
3. ✅ CTA footer on every scenario output.
4. ⏳ Embedded `scenario run` execution (replace v0's command-hint with in-CLI in-memory execution + headline + before/after delta).
5. ⏳ Surface 3 additional parameters as CLI flags on `scenario run` (margin bps, oracle staleness, funding cap).
6. ⏳ Extend the scenario JSON format to drive oracle observations directly, then add `oracle-stale` and `adl-trigger` scenarios.

Implementation of remaining items tracked in the cross-engine Task #8.

## Build

```bash
cargo check                          # workspace
cargo test -p openhl-consensus       # default test set (some diagnostics are #[ignore])
cargo test --workspace               # full suite (see docs/testing.md for ignored diagnostics)
cargo run --bin openhl -- --help
```

## Related

- [`fabrknt/website/CONCEPT.md`](../../fabrknt/website/CONCEPT.md) — the Fabrknt brand and 2x2 sandbox structure.
- [`fabrknt/website/SANDBOX-PATTERN.md`](../../fabrknt/website/SANDBOX-PATTERN.md) — cross-engine spec for the five sandbox elements.
- [`../README.md`](../README.md) — the parent `rdk` monorepo and the shared crates `openhl` consumes.
- [`docs/architecture.md`](docs/architecture.md) — subsystem detail, CL/EL contract, determinism rules.
- [`docs/testing.md`](docs/testing.md) — test layout and ignored-diagnostic runbook.

## License

Apache-2.0 / MIT (inherited from `rdk`).

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

# Run the scenario in-process and render a headline + per-block
# timeline + before/after account delta. Takes ~50ms; no Reth boot.
openhl scenario run cascade

# Pass --dry-run for the long-running production-shape boot
# (real Reth + Malachite + JSON-RPC) instead of in-process execution.
openhl scenario run cascade --dry-run
```

Three scenarios ship today: `cascade` (5-trader multi-block liquidation cascade — fires 8 liquidation scan-hits in v1), `single-block-cascade` (same cascade compressed into one block), `calm-baseline` (control: two balanced traders, no liquidations).

Dial flags on `scenario run` (override the values baked into the scenario JSON's `params` block):
- `--rounds <N>` — block count
- `--initial-margin-bps <N>` — initial margin (default 1000)
- `--maintenance-margin-bps <N>` — maintenance margin (default 200)
- `--liquidation-fee-bps <N>` — liquidation fee (default 150)

Lets a buyer ask "what changes if maintenance margin tightens from 2% to 5%?" without editing the scenario JSON.

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
| (1) Pre-baked scenarios | **present** | `scenarios/` directory with 3 named scenarios (`cascade`, `single-block-cascade`, `calm-baseline`). 2 more pending (oracle stale, ADL trigger) once the JSON format extends to oracle observations. |
| (2) Business-readable output | **v2 done for declared scenarios** | `scenario run` executes in-process and renders HEADLINE (with ✓/⚠/unverified badge), TIMELINE, ACCOUNT DELTA, OUTCOMES (per `expected_outcomes` declared in JSON), and NEXT. All three shipped scenarios declare outcomes that verify ✓. **Caveats**: per-block state isn't fully consistent because post-tick `TickReport` is not yet written back to the bridge (same accounts may be re-flagged each tick); oracle-driven cascades need JSON-format extension for oracle observations. Both tracked in `fabrknt/website/SANDBOX-BACKLOG.md`. |
| (3) Parameter dial | partial | Margin params (initial / maintenance / liquidation fee bps) baked into scenario JSON are applied to the `LiquidationParams` at run time. CLI flag overrides on `scenario run` (per-invocation dial) pending. |
| (4) Scenario replay | **done** | `chain-history` (now via `scenarios/*.json` wrapper) is the replay format. Deterministic, bit-identical across runs. |
| (5) CTA | **done** | `scenario list` / `show` / `run` all render a three-option CTA footer (adopt engine / custom build / hosted access). |

## Roadmap to full sandbox

1. ✅ Promote fixtures to `scenarios/` with named manifests.
2. ✅ Add the `scenario list` / `show` / `run` CLI surface.
3. ✅ CTA footer on every scenario output.
4. ✅ Embedded `scenario run` execution (in-process against a unit-provider `LiveRethEvmBridge<()>`).
5. ⏳ v2: write post-tick `TickReport` (funding settlements / liquidation closes / ADL records) back to the bridge so per-block state stays consistent (currently the same accounts get re-flagged each block).
6. ⏳ Surface CLI flag overrides on `scenario run` for in-run parameter dial (margin bps, oracle staleness, funding cap).
7. ⏳ Extend the scenario JSON format to drive oracle observations directly, then add `oracle-stale` and `adl-trigger` scenarios.

Implementation of remaining items tracked in the cross-engine Task #9.

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

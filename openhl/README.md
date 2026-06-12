# openhl — EVM Perp Sandbox

**See how a perp DEX behaves under stress — without spinning up a real one.**

`openhl` is a runnable reference implementation of perpetual-futures mechanics on EVM. You hand it a scenario (a 5-trader liquidation cascade, an oracle going stale, a deep ETH crash), and ~50 ms later it prints back a structured **HEADLINE → TIMELINE → DELTA → OUTCOMES → NEXT** report with every declared outcome verified ✓ or ⚠. No deployment, no validator boot, no UI.

## What you can do in 5 minutes

If you have a few minutes for a cold cargo build of Reth + Malachite + openhl, run it locally:

```bash
git clone https://github.com/psyto/rdk
cd rdk/openhl
cargo run -q -p openhl -- scenario run cascade
```

That runs the canonical liquidation cascade (5 traders, mark drops from 110 → 96, scanner flags 2 underwater longs, write-back loop closes them + their maker counterparties via ADL) end-to-end. 10 declared outcomes verify ✓ before the prompt comes back.

**Expect:** ~5-15 minutes for the first `cargo run` (cold compile of Reth + Malachite + the openhl workspace pulls a lot of crates and fills ~5-15 GB under `target/`); ~50 ms per subsequent `scenario run` once the binary is built. The pinned toolchain (Rust 1.95.0) auto-installs via `rust-toolchain.toml`.

If you want the value first and the build second, [**`docs/sample-output.md`**](docs/sample-output.md) embeds the exact `cascade` output verbatim — no clone or build required.

## Who this is for

- **Perp DEX builders** validating exchange mechanics before writing their full system.
- **Infra researchers** comparing liquidation / ADL / oracle / funding behavior across execution surfaces and risk models.
- **Product owners** who want to answer "what changes if maintenance margin tightens from 2% to 5%?" without reading code — flip the `--maintenance-margin-bps` dial and re-run.
- **Hackathon teams** who want a working conceptual scaffold for an EVM perp engine instead of starting from zero.

If you fit any of those, the [EVM Perp Sandbox landing page](https://fabrknt.com/evm-perp.html) is the product-facing companion to this repo.

## What `openhl` is NOT

- Not a production exchange. Not deployed. No fee capture, no governance, no token.
- Not a turnkey "white-label exchange" — there is no operator dashboard, no front-end, no KYC.
- Not pedagogy. The depth-and-learning surface for Reth and the broader Rust Ethereum stack lives at RethLab; `openhl` is the product-facing sandbox engine.

## What's under the hood

`openhl` composes:

- **Reth** as the execution layer (full EVM, library-style integration).
- **Malachite** as the BFT consensus layer (Tendermint-style finality).
- Five pure deterministic state machines on top: CLOB, funding, liquidation (margin + insurance fund + ADL), oracle (signed observation aggregation + freshness), vault, and clearing (per-account bookkeeping).
- An integration coordinator (`OpenHlNode::tick`) that runs the per-block routine in one deterministic order: oracle refresh → liquidation scan → ADL absorption → vault mark-to-market → funding settlement.

Architecture detail lives in `docs/architecture.md`. Determinism rules (no `SystemTime::now`, no `HashMap` iteration order, no `rand`) are enforced by `unsafe_code = forbid` plus dependency review.

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

Five scenarios ship today:
- **`cascade`** (stress) — 5-trader multi-block liquidation cascade; 7 outcomes ✓.
- **`single-block-cascade`** (stress) — same cascade compressed into one block; 3 outcomes ✓.
- **`threshold-margin`** (stress) — three traders at varying leverage; outcomes are sensitive to the `--maintenance-margin-bps` dial (default fires 1 acct/block, 500bps fires 2/block, 10bps fires 1/block); 7 outcomes ✓.
- **`position-buildup`** (walkthrough) — two market buys at different prices, demonstrates clearing-layer VWAP avg_entry computation; 8 outcomes ✓.
- **`calm-baseline`** (baseline) — two balanced traders, no events; 4 outcomes ✓.

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
| (1) Pre-baked scenarios | **present** | `scenarios/` directory with 7 named scenarios spanning the shared taxonomy: 5 stress (`cascade`, `single-block-cascade`, `threshold-margin`, `oracle-stale`, `adl-trigger`) + 1 walkthrough (`position-buildup`) + 1 baseline (`calm-baseline`). |
| (2) Business-readable output | **v2 done across all scenarios** | `scenario run` executes in-process against a unit-provider `LiveRethEvmBridge<()>` and renders the full 5-section contract: HEADLINE (✓/⚠/unverified badge per `expected_outcomes` verification), TIMELINE, ACCOUNT DELTA, OUTCOMES, NEXT. Per-tick `TickReport` is written back to the bridge (funding settlements → collateral, liquidation closes → position=0, ADL records → counterparty position + PnL) so per-block state stays consistent. The chain-history JSON drives oracle observations directly via per-block `oracle: {set_price: N}` / `oracle: "clear"` ops; the runner reads `effective_mark` so the timeline surfaces `mark_source = "oracle"` when an index is installed. All 7 shipped scenarios declare outcomes that verify ✓ (45 outcomes total). |
| (3) Parameter dial | **done** | Margin params (initial / maintenance / liquidation fee bps) baked into scenario JSON apply at run time. CLI dials on `scenario run` surface the canonical knobs: `--initial-margin-bps`, `--maintenance-margin-bps`, `--liquidation-fee-bps`, `--rounds`. |
| (4) Scenario replay | **done** | `chain-history` (now via `scenarios/*.json` wrapper) is the replay format. Deterministic, bit-identical across runs. |
| (5) CTA | **done** | `scenario list` / `show` / `run` all render a three-option CTA footer (adopt engine / custom build / hosted access). |

## Roadmap to full sandbox

1. ✅ Promote fixtures to `scenarios/` with named manifests.
2. ✅ Add the `scenario list` / `show` / `run` CLI surface.
3. ✅ CTA footer on every scenario output.
4. ✅ Embedded `scenario run` execution (in-process against a unit-provider `LiveRethEvmBridge<()>`).
5. ✅ Write post-tick `TickReport` (funding settlements / liquidation closes / ADL records) back to the bridge so per-block state stays consistent.
6. ✅ Surface CLI flag overrides on `scenario run` for in-run parameter dial (initial / maintenance / liquidation fee bps, rounds).
7. ✅ Extend the scenario JSON format to drive oracle observations directly; `oracle-stale` and `adl-trigger` scenarios shipped.

Remaining cross-engine work tracked in `fabrknt/website/SANDBOX-BACKLOG.md`.

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

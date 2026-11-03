# rdk — perp DEX building blocks in Rust

**Pick the piece you need:** an orderbook, an oracle aggregator, funding, margin and liquidation,
settlement — each its own small crate with no Reth, no consensus and no I/O, so you can use one
without the rest. Two complete L1s built from them, a perp DEX (OpenHL) and a prime broker
(Princeps), show how the pieces fit.

## Pick a crate

| I want to… | Crate | Other rdk crates it pulls in |
|---|---|---|
| match orders (price-time priority, limit and market) | `rdk-clob` | `rdk-units` |
| combine several price feeds into one index, dropping outliers | `rdk-oracle` | `rdk-units` |
| compute funding rates and settle them | `rdk-funding` | `rdk-units` |
| check margin, liquidate, run an insurance fund and ADL | `rdk-liquidation` | `rdk-units` |
| settle fills and funding into balances | `rdk-clearing` | `rdk-clob`, `rdk-funding` |
| pool collateral into shares | `rdk-vault` | — |
| share account, price and size types between them | `rdk-units` | — |

Outside rdk the crates use `serde`, and `rdk-oracle` also `k256` for signed observations.
`rdk-units` holds the newtypes (`AccountId`, `Price`, `Qty`, `Side`, `MarkPrice`, `IndexPrice`,
`PositionSize`, `Notional`). `rdk-clob` and `rdk-funding` re-export them, so `rdk_clob::AccountId`
and `rdk_units::AccountId` are the same type.

## Quickstart

Each builds only its own crate and needs no node:

```bash
git clone https://github.com/psyto/rdk && cd rdk
cargo run -p rdk-clob --example clob_quickstart         # a maker rests, a taker fills part of it
cargo run -p rdk-oracle --example oracle_quickstart       # three feeds, one 5% off, dropped
cargo run -p rdk-liquidation --example liquidation_quickstart  # a long checked as the mark falls
```
```
mark   100000000: margin  1500 bp, Safe
mark    90000000: margin   555 bp, AtRisk
mark    86000000: margin   116 bp, Liquidatable
mark    84000000: margin  -119 bp, Underwater
```

Use one in your project (not on crates.io yet; Cargo finds the crate inside the repo by name):

```toml
[dependencies]
rdk-liquidation = { git = "https://github.com/psyto/rdk" }
rdk-units       = { git = "https://github.com/psyto/rdk" }
```
```rust
use rdk_liquidation::{margin_health, AccountSnapshot, LiquidationParams};
use rdk_units::{AccountId, MarkPrice, Notional, PositionSize};

let account = AccountSnapshot {
    account: AccountId(1),
    position_size: PositionSize(10),        // long 10
    avg_entry: MarkPrice(100_000_000),      // at $100 (minor units)
    collateral: Notional(150_000_000),      // $150
};
let health = margin_health(&account, MarkPrice(86_000_000), &LiquidationParams::hyperliquid_default());
// Liquidatable
```
Pin a commit (`rev = "…"`) rather than following `main`.

## How the pieces fit: two reference L1s

- **`openhl/` — a Hyperliquid-shape perp DEX, as its own L1.** Reth (EVM execution) + Malachite (BFT
  consensus) + on-chain CLOB, funding, and liquidation via custom precompiles. A complete, explorable
  perp DEX you can run locally. → [`openhl/README.md`](openhl/README.md) · powers Fabrknt's
  [EVM Perp Sandbox](https://fabrknt.com/reactor/evm-perp); built step by step in the Build OpenHL
  courses on [Fabrknt Dojo](https://fabrknt.com/dojo/courses)
- **`princeps/` — a prime broker, as its own L1.** The same substrate, unifying lending + perps +
  (future) options under one shared risk engine. → [`princeps/README.md`](princeps/README.md) ·
  powers Fabrknt's [EVM Prime Broker Sandbox](https://fabrknt.com/reactor/evm-prime-broker)

Both are wired end to end — consensus, EVM precompiles, orderbook, funding, liquidation,
settlement — and tested (855 tests across the workspace, 2026-11-03). Built the way Reth ships: reusable
`rdk-*` crates with node binaries on top.

### Why one repo (not three)

OpenHL (perp DEX) and Princeps (prime broker) need the same substrate (Reth EVM + Malachite consensus
+ custom precompile patterns) and the same trading primitives (matching, funding, liquidation,
settlement). Forking one into the other produced massive duplication; the kit factors the shared
layer once so each app focuses on its own integration (precompile set, consensus hooks, node wiring)
and product-specific crates (e.g. `princeps-lending`, `princeps-portfolio`). **This mirrors how Reth
itself ships: one repo, reusable `reth-*` crates, and binaries on top.**

## Structure

```
rdk/
├── crates/                  ← shared DeFi primitives (consumed by openhl + princeps)
│   ├── units                ← shared account / price / size types (every trading crate uses these)
│   ├── types                ← consensus ↔ execution contract types
│   ├── codec
│   ├── clob                 ← central limit orderbook matching engine
│   ├── funding              ← funding rate computation
│   ├── vault                ← share-based collateral pooling primitive
│   ├── liquidation          ← margin math + insurance fund + scanner + ADL
│   ├── clearing             ← settlement
│   └── oracle               ← signed observation aggregation + circuit breaker
├── openhl/                  ← Perp DEX L1
│   ├── crates/{consensus,evm,node}
│   ├── bin/openhl
│   └── docs/
└── princeps/                ← Prime Broker L1
    ├── crates/{consensus,evm,node,lending,portfolio}
    ├── bin/{princeps,liquidator-bot,lending-rpc-server}
    └── docs/
```

## Layer rules

1. **`crates/*` (rdk shared)** — no Reth integration code, no consensus engine code. Pure data structures and business logic. Deterministic, microsecond unit tests. Both openhl and princeps depend on these. Crate prefix: `rdk-*`.
2. **`openhl/crates/*` and `princeps/crates/*` (per-app)** — Reth EVM glue (custom precompile sets), Malachite consensus integration, node wiring. May depend on rdk shared crates but never on the other app. Crate prefixes: `openhl-*`, `princeps-*`.
3. **`openhl/bin/*` and `princeps/bin/*`** — runnable binaries (node, RPC server, bots).

If new shared primitive logic emerges in only one app, it stays per-app until proven useful for the other. Promote to `rdk/crates/` only when both apps need it.

## Stack

- **Reth** v2.2.0 (Ethereum execution client, used as library)
- **Malachite** v0.5.0 (Tendermint-style BFT consensus from Informal Systems)
- **Alloy** v1.5 / v2.0 (Ethereum primitives)
- **Rust** edition 2024, resolver 3, 1.95+, `unsafe_code = forbid`

All pins are release-tag SHAs in `Cargo.toml`. Bump in a dedicated PR.

## Builds

```bash
cargo check                       # workspace
cargo test --workspace            # 855 tests (2026-11-03)
cargo test -p rdk-units -p rdk-clob -p rdk-funding -p rdk-oracle -p rdk-liquidation -p rdk-clearing -p rdk-vault  # shared crates only, no Reth build
cargo run --bin openhl -- --help  # OpenHL node
cargo run --bin princeps -- --help  # Princeps node
```

## Pronunciation

- **rdk** — "R-D-K" (initialism)
- **OpenHL** — "open H-L"
- **Princeps** — PRIN-seps (Latin)

## License

Apache-2.0.

# rdk — build a DeFi L1 on Reth

**Complete, tested reference implementations of DeFi L1s on Reth + Malachite** — a perp DEX
(OpenHL) and a prime broker (Princeps) — plus the shared kit of primitives behind them.

Want to see how a real perpetuals exchange or prime broker works as its *own* Reth-based L1 —
consensus, EVM precompiles, orderbook, funding, liquidation, settlement, wired end-to-end? This is a
working, **590-test** reference you can read and run. Built the way Reth ships: reusable `rdk-*`
crates + node binaries on top.

## The reference implementations

- **`openhl/` — a Hyperliquid-shape perp DEX, as its own L1.** Reth (EVM execution) + Malachite (BFT
  consensus) + on-chain CLOB, funding, and liquidation via custom precompiles. A complete, explorable
  perp DEX you can run locally. → [`openhl/README.md`](openhl/README.md) · powers Fabrknt's
  [EVM Perp Sandbox](https://fabrknt.com/evm-perp.html)
- **`princeps/` — a prime broker, as its own L1.** The same substrate, unifying lending + perps +
  (future) options under one shared risk engine. → [`princeps/README.md`](princeps/README.md) ·
  powers Fabrknt's [EVM Prime Broker Sandbox](https://fabrknt.com/evm-prime-broker.html)

## The kit behind them — `crates/`

Both apps need the same substrate and the same trading primitives, so the shared layer is factored
once: `rdk-*` crates any Reth+Malachite L1 can consume.

| crate | what it does |
|---|---|
| `clob` | central-limit orderbook matching engine |
| `funding` | funding-rate computation |
| `vault` | share-based collateral pooling |
| `liquidation` | margin math + insurance fund + scanner + ADL |
| `clearing` | settlement |
| `oracle` | signed-observation aggregation + circuit breaker |
| `types` · `codec` | shared data structures + wire codec |

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
│   ├── types
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
cargo test --workspace            # 590+ tests
cargo run --bin openhl -- --help  # OpenHL node
cargo run --bin princeps -- --help  # Princeps node
```

## Pronunciation

- **rdk** — "R-D-K" (initialism)
- **OpenHL** — "open H-L"
- **Princeps** — PRIN-seps (Latin)

## License

Apache-2.0.

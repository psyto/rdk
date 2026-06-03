# rdk — Reth DeFi Kit

Ready-made DeFi L1 stack: **Reth/REVM (execution) + Malachite (BFT consensus) + DeFi primitives**.

A monorepo hosting:

- **`crates/`** — the kit itself: reusable DeFi L1 primitives (CLOB, funding, vault, liquidation, clearing, oracle, types, codec) consumable by any Reth+Malachite-based L1.
- **`openhl/`** — reference Perp DEX L1 (Hyperliquid-shape). Worked example for the rethlab DIY Perp track.
- **`princeps/`** — DeFi prime broker L1 (lending → options → structured products → institutional rails). Production product.

## Why a kit

Both OpenHL (perp DEX) and Princeps (prime broker) need the same L1 substrate (Reth EVM + Malachite consensus + custom precompile patterns) and the same trading primitives (orderbook matching, funding computation, collateral liquidation, settlement). Forking openhl into princeps and developing in parallel produced massive duplication. The kit factors the shared layer once so each app focuses on its own integration (precompile set, consensus hooks, node wiring) and product-specific crates (e.g., princeps-lending, princeps-portfolio).

This mirrors how Reth itself ships: a single repo with reusable `reth-*` crates and the `reth` binary on top.

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

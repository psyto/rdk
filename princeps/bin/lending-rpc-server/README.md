# princeps-lending-rpc-server

HTTP JSON-RPC over an in-process `LiveRethEvmBridge`, **plus a single-page
browser demo of cross-protocol margin** that drives the real
`princeps-portfolio` kernel from the page.

```bash
cargo run --bin princeps-lending-rpc-server
open http://localhost:8080          # the demo UI
```

## What the demo shows

A **market-neutral book** — ETH collateral lent on Aave, hedged with a short
ETH perp on Hyperliquid. Each of those venues today prices only its own leg.
Princeps runs the whole book through one risk engine.

Move the ETH mark and watch which venue liquidates a position that, netted, is
barely exposed:

- **ETH falls** → Aave's collateral haircut breaches and it liquidates the
  lending leg — even though the Hyperliquid short gained exactly enough to cover
  it. Princeps nets the two inside one account and holds.
- **ETH rises** → the Hyperliquid short's margin runs out and it liquidates the
  perp — even though the Aave collateral rose. Princeps holds.
- **ETH rises far enough** → even the unified account goes underwater and
  **Princeps liquidates too.** The edge is falsifiable.

The gap Princeps fills is not "cross-margin" — Hyperliquid, dYdX, and Drift all
cross-margin *within* their own venue. It is **cross-*protocol* netting**: no
single venue nets your Aave lending leg against your Hyperliquid perp leg. That
is the prime-broker thesis.

## Honesty model

This demo is built to be an experiment, not an advertisement. Four properties:

| | How |
|---|---|
| **Real engine** | Every Princeps number is `free_equity` returned by `GET /portfolio/health`, which runs the caller's book straight through `princeps_portfolio::compute_free_equity` — the same code path a validator runs. The page does not compute it. |
| **Published rulebooks** | The Aave and Hyperliquid panels apply each venue's own *published* risk parameters (liquidation threshold; initial/maintenance margin) to its own leg. The parameters are shown in the UI, sourced, and editable. No self-declared "the other venue would liquidate" verdicts. |
| **Real price path** | "Replay Aug 5 2024" drives the ETH mark through the real hourly ETH/USDT closes of the Aug 5 2024 yen-carry-unwind crash ($2,924 → $2,228), sourced from Binance klines. |
| **Princeps can lose** | Push the mark far enough and the unified account liquidates too, shown in red. The comparison is falsifiable. |

**Not claimed:** the traded book is entered by the user (real by construction),
not sourced from a specific on-chain address — a real cross-venue hedged-and-
liquidated victim cannot be attributed from on-chain data alone (the Aave and
perp legs live on different chains under unlinkable addresses), so the book is
illustrative. Nothing here is investment advice.

## Endpoints

| Method | Path | Returns |
|---|---|---|
| `GET` | `/` | the demo UI (`ui/index.html`, read from disk per request — edit without rebuilding) |
| `GET` | `/lending/markets` | every registered market |
| `GET` | `/lending/positions` | every open lending position |
| `GET` | `/lending/health?account=..&perp_mark=..&perp_im_bps=..&coll_price=..&debt_price=..` | health for a seeded account |
| `GET` | `/portfolio/health?perp_collateral=..&perp_unrealized_pnl=..&perp_im_req=..&lending_adjusted_collateral_value=..&lending_debt_value=..` | health for an arbitrary, stateless caller-supplied book |
| `GET` | `/lending/scan?perp_mark=..&perp_im_bps=..&coll_price=..&debt_price=..` | `UnifiedScanReport` from the liquidation scanner |

```bash
# arbitrary cross-protocol book straight through the real kernel:
curl 'http://localhost:8080/portfolio/health?perp_collateral=1000&perp_unrealized_pnl=900&perp_im_req=100&lending_adjusted_collateral_value=950&lending_debt_value=1800'
# → {"free_equity":950,"is_healthy":true, ...}   # lending leg -850 underwater, perp leg backs it
```

## Scope

v0 in-process bridge with a dev chainspec — the real kernel, run locally, not a
live network deployment. Real-world deployment serves the same endpoint shapes
from a running `reth-devnet` (`../princeps` node + `../liquidator-bot` keeper +
`../../scripts/devnet-3.sh`); v1 replaces the local bridge with a connection to
a long-lived node. The endpoint shapes do not change.

Because a published static page is sandboxed and cannot reach `localhost`, the
UI is served by this binary and calls the engine same-origin — it is a local
tool, run alongside the server, not a hostable artifact.

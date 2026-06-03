# Migration: psyto/openhl + psyto/princeps → psyto/rdk

Date: 2026-06-03. Both source repos were `/Users/hiroyusai/src/openhl` and `/Users/hiroyusai/src/princeps`. Both had ~identical scaffolding (Reth+Malachite+CLOB+funding+vault+liquidation+clearing+oracle) after Princeps was bootstrapped from OpenHL via rsync on 2026-05-31. Princeps added `lending`, `portfolio`, `liquidator-bot`, `lending-rpc-server` and accumulated divergence in `evm` (+936 LOC for prime-broker precompiles), `consensus`, and `node`.

## Boundary chosen

```
crates/*                  rdk-* (shared, 8 crates)
openhl/crates/*           openhl-* (per-app, 3 crates: consensus, evm, node)
openhl/bin/openhl         openhl binary
princeps/crates/*         princeps-* (per-app, 5 crates: consensus, evm, node, lending, portfolio)
princeps/bin/*            princeps + 2 helper binaries
```

The 8 shared crates were chosen by: (a) low diff between openhl and princeps versions (≤ 200 lines, mostly crate-name prefix renames), (b) no dependency on Reth/Malachite glue.

## Rename rules applied

In source files inside the rdk shared crates (`crates/*`):

- `openhl_<crate>` → `rdk_<crate>` (Rust module identifiers)
- `openhl-<crate>` → `rdk-<crate>` (Cargo names in doc comments)
- `openhl` (bare project references in doc comments) → `rdk`

In source files inside `openhl/` and `princeps/` per-app crates and binaries, only references to the 8 shared crates were renamed:

- `openhl_clob`, `openhl-clob`, etc. → `rdk_clob`, `rdk-clob`, etc.
- `princeps_clob`, `princeps-clob`, etc. → `rdk_clob`, `rdk-clob`, etc.

Per-app identifiers (`openhl_evm`, `princeps_consensus`, `princeps_lending`, the `openhl_*` / `princeps_*` precompile names, etc.) were kept as-is — they remain app-specific.

## Substantive change made during migration

`crates/liquidation/src/scanner.rs::LiquidationScanner::fund_mut(&mut self) -> &mut InsuranceFund` was present in princeps's version but not openhl's. It was added to `rdk-liquidation` so both apps can use it. The doc comment was generalized — original mentioned `PrincepsNode::absorb_lending_bad_debt`, now reads "Integration coordinators use this to absorb out-of-band shortfalls — e.g., lending bad debt routed in from a bridge layer."

All other diffs in the shared crates were rename-only.

## What was NOT touched

- `/Users/hiroyusai/src/openhl` and `/Users/hiroyusai/src/princeps` — left intact. Validate rdk first, then decide on freezing.
- `psyto/openhl` and `psyto/princeps` GitHub repos — unchanged.
- rethlab course `file:line@SHA` citations — still point at the original `psyto/openhl` repo. To be updated once rdk repo is pushed and tagged.

## Build verification

`cargo check --workspace`: all 16 crates + 4 binaries compile.
`cargo test --workspace`: 590 passed / 0 failed / 15 ignored.

## Next steps

1. Push `psyto/rdk` to GitHub.
2. Decide handling of psyto/openhl and psyto/princeps repos (freeze with redirect README, archive, or delete).
3. Audit rethlab references and either re-pin to the historical openhl SHA or update to rdk crate paths.

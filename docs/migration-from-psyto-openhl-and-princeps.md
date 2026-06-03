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

## Resolved 2026-06-04

**Step 1 — done 2026-06-03** when `d1ea674 "Initial commit: rdk monorepo"` landed on `psyto/rdk` `main`.

**Step 3 — resolved by audit; no rewrite needed.** The premise of step 3 — that rethlab citations are `file:line@SHA` strings — was outdated. The drafts directory holding that cite form (`drafts/openhl_*.md`) was removed when `psyto/openhl` commit `3f7b413` made the lesson seeds (`prisma/seed-reth-openhl-*-{en,ja}.ts`) the source of truth. The current cite-checker (`.github/scripts/check-openhl-cites.ts` in rethlab) instead verifies two reference shapes the seeds actually use: (a) per-Stage SHA references like `Stage 10d (d66b44a)`, `SHA \`d66b44a\``; (b) bare source paths like `crates/clob/src/book.rs`. It runs against a local `../openhl` sibling checkout, not the GitHub remote, so archive doesn't affect it. Pre-archive run on 2026-06-04: 1 SHA + 38 paths checked, 0 failures.

Audit of all openhl mentions across `psyto/rethlab` (~187 occurrences in 19 files; full inventory in conversation context, not duplicated here) found none that warrant rewriting to `psyto/rdk`. Reasons rewriting is a non-starter: (i) per-Stage SHAs spanning Stage 1 through Stage 13 (with sub-stages 6b/c, 7a–d, 8a/b/d, 9a–e, 10a–d) exist in `psyto/openhl`'s git history but not in `psyto/rdk`'s single initial commit; (ii) the kit/app split (`rdk_*` shared vs `openhl_*` per-app) is the wrong mental model for students building a monolithic openhl from `cargo init`; (iii) the bulk of citations are course-mechanic narrative ("psyto/openhl is your answer key", cloned read-only to `~/code/openhl-reference/`) which has no path to rewrite. Zero `psyto/princeps` citations and zero accidental `psyto/rdk` slips were found.

**Step 2 — done 2026-06-04.** Banner READMEs were already committed and pushed on 2026-06-03 (`psyto/openhl@9f1b824 "docs: redirect active development to psyto/rdk"` and `psyto/princeps@a9e4314 "docs: redirect active development to psyto/rdk"`). On 2026-06-04 both repos were archived via `gh repo archive` — read-only on GitHub, all commits and SHAs reachable as before, the openhl Stage 1–13 history that rethlab depends on remains intact. Princeps was archived rather than deleted for symmetry and recoverability; its 2026-06-01 launch announcement and v0 lending demos remain accessible at the archived URL.

Local checkouts at `/Users/hiroyusai/src/openhl` and `/Users/hiroyusai/src/princeps` are unchanged and continue to serve as the targets for rethlab's local cite-checker.

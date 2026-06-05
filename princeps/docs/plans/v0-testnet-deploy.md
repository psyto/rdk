# Princeps v0 — Public testnet deploy plan

**Status**: Drafting (created 2026-06-05).
**Target**: Q3 2026 public testnet — the final row of [v0-lending.md](./v0-lending.md) ("Public testnet deploy ⏳ Pending").
**Scope**: Take the protocol — already complete end-to-end on `reth-devnet` — and stand it up as a publicly reachable, externally-validator-able testnet with faucet, monitoring, and a documented runbook. No new protocol work.

## What this plan ships

- A persistent multi-validator princeps network reachable from the public internet
- A documented genesis (chain-id, validator set, seeded markets, oracle publishers, operator registry)
- Public read-only RPC + websocket endpoint(s) for the lending precompiles and standard EVM JSON-RPC
- Faucet for testnet USDC + testnet ETH so third parties can run the v0 demo
- Monitoring + alerting (validator liveness, block production, oracle freshness, lending halt state, insurance-fund balance, chain-history tail)
- Runbook covering: cold start, validator crash recovery, oracle publisher rotation, operator-key rotation, snapshot restore, lending-halt activation/lift, socialization declaration
- README badge flip: `🚧 v0 lending` → `✅ v0 lending (testnet)`

## What this plan does NOT ship

- Mainnet — separate plan once external testnet validation lands (≥3rd-party validators running ≥30 days, no critical incidents)
- Tokenomics or staking — deferred to ADR-009 (v3+ per [open question #6](./v0-lending.md#open-questions--risks))
- Web UI — CLI sufficient per `v0-lending.md` "What v0 does NOT ship"
- Multi-asset collateral — single USDC/ETH market as on devnet
- Operator-agreement signed instances — template in `docs/operator-agreement.md` is ready; collection happens at v1 mainnet onboarding (Layer 2 is template-ready, not signed)

## Architectural decisions

### TD-001 — Reuse `scripts/devnet-3.sh` as the topology baseline

The 3-validator script (alice/bob/carol) is the proven N-agnostic boot path. Testnet inherits the same coordinator-snapshot equivalence guarantee — what changes is hosting, key custody, peer dial lists, and chain-id, not the control plane. Multi-validator status row is already ✅ Complete in `v0-lending.md`; the testnet is the first deployment where the validators don't all live on one laptop.

### TD-002 — Foundation-operated validator set at launch; open registration arrives in stage T6

v0 testnet launches with **3 foundation validators** (operationally equivalent to alice/bob/carol but on three independent hosts in three regions). External validators are invited starting T6 once the runbook has been exercised internally. This mirrors ADR-002 (sequencer-centralized-then-decentralize) at the consensus layer: start under our operational control, prove the runbook, then open up.

### TD-003 — Hosting model: independent commodity cloud instances, no shared control plane

Three validators on three different cloud providers (e.g. AWS us-east, GCP eu-west, Hetzner). No shared orchestration layer — each runs the same `princeps` binary against its own state directory. The point of N validators is fault-isolation; bundling them under one orchestrator (k8s, nomad) reintroduces the single-point-of-failure the consensus layer exists to eliminate.

### TD-004 — Separate validator RPC from public RPC

Validator nodes do **not** serve public traffic. A separate `princeps-lending-rpc-server` instance (already built in Stage 24d) runs as a non-validating read-only follower behind a load balancer. Public-RPC compromise can't influence consensus; validators only peer with each other and the public-RPC follower(s).

### TD-005 — Genesis is a committed file, not a script

The devnet today bootstraps via `seed_v0_lending_markets` + `seed_v0_demo_accounts` called at boot. Testnet requires a **`testnet-genesis.json`** committed to the repo: chain-id, validator pubkeys, oracle publisher keys, operator keys, seeded markets, initial USDC supply, initial demo accounts. A node started against this genesis MUST produce a deterministic genesis state hash that all validators verify. Devnet seed functions get refactored to load from this file rather than hardcoded constants.

### TD-006 — Monitoring stack: Prometheus scrape + Grafana dashboards + PagerDuty (or equiv) on critical alerts

Today the node emits `tracing` events only. Add a `/metrics` Prometheus endpoint to `PrincepsNode` exposing: block height, validator liveness gauge, oracle observation age per publisher, lending-halt state, total markets, total positions, insurance-fund balance, chain-history length, scan-loop duration. Validator-liveness loss and oracle-staleness > 60s page; lending-halt activation alerts (info, not page).

### TD-007 — Faucet is a separate signed-sender service, not a precompile

Testnet USDC + testnet ETH faucet implemented as a small HTTP service that holds a faucet key and submits standard transfers via JSON-RPC. Rate-limit per IP + per recipient. Captcha to keep cost down. Standalone deploy — faucet outage doesn't degrade the chain.

### TD-008 — Snapshot strategy: per-block coordinator snapshot + periodic full-state archive

Coordinator snapshots already persist across restart (Stage 18a). Testnet adds a per-N-block tarball of `{coordinator-snapshot, bridge-snapshot, chain-history.jsonl}` uploaded to object storage, retained 30 days. This is what restores a validator from cold or onboards a 4th validator without re-syncing genesis.

## Stage plan

Following the established Stage N[a–k] pattern, prefix `T` for testnet.

### Stage T1 — Genesis + chain-id

- **T1a** — Allocate chain-id (proposal: `0x...` — pick a number not colliding with public registries; submit to `chainlist.org` post-launch)
- **T1b** — `princeps/genesis/testnet-genesis.json` schema + committed file
- **T1c** — Refactor `seed_v0_lending_markets` / `seed_v0_demo_accounts` to load from a `Genesis` struct (devnet keeps hardcoded path via a `dev-default` feature)
- **T1d** — Genesis-hash determinism test: load → snapshot → load again → snapshot byte-identical (add to existing snapshot-equivalence test suite)

Expected: ~10 tests around genesis loading, deterministic state hash, devnet/testnet feature gating.

### Stage T2 — Observability surface

- **T2a** — Add `metrics` / `metrics-exporter-prometheus` deps to `princeps-node`; gauge + counter set per TD-006
- **T2b** — `/metrics` endpoint on validator binary (separate port from p2p / RPC)
- **T2c** — Grafana dashboard JSON committed to `princeps/ops/grafana/`
- **T2d** — Alert rules YAML committed to `princeps/ops/alerts/` (Prometheus alertmanager format)

Expected: ~5 tests on the metrics surface (registry registration, label cardinality bounds).

### Stage T3 — Operator + oracle key handling for production-grade hosts

- **T3a** — Validator key generation script — `princeps validator gen-keys` (already exists for devnet keys; harden for production: stdin passphrase, keystore-file output)
- **T3b** — Oracle publisher onboarding doc: how to register a 4th–Nth publisher, key rotation procedure, what triggers de-registration
- **T3c** — Operator-key onboarding for ADR-010 Layer 3: foundation registers itself as operator-0 at genesis; documented procedure to register external operators at v1 (template lives in `docs/operator-agreement.md`)

Expected: docs + script work; minimal new tests.

### Stage T4 — Faucet service

- **T4a** — `princeps-faucet` crate under `bin/`: axum HTTP server, captcha integration, sqlite for rate-limit state
- **T4b** — Faucet key custody: dedicated key, top-up procedure documented in runbook
- **T4c** — Deploy as a separate process behind a separate domain (`faucet.testnet.princeps.<tld>`)

Expected: ~15 tests on rate-limiting, double-spend prevention, key isolation.

### Stage T5 — Internal 3-validator dry-run on real infra

- **T5a** — Provision three hosts in three regions; install binary, genesis, keys
- **T5b** — 7-day soak: monitor block production, oracle freshness, coordinator-snapshot equivalence
- **T5c** — Chaos drills (each documented in runbook):
  - Kill validator A, verify B+C continue, restart A, verify catch-up
  - Stall an oracle publisher, verify staleness alert fires within 60s
  - Force a lending-halt by spiking utilization in test market, verify halt + alert
  - Issue a socialization declaration via `princeps socialize`, verify chain-history append + per-position haircut
- **T5d** — Restore-from-snapshot drill: nuke validator C's state dir, restore from object-storage archive, verify rejoin

Expected: no new code; gaps surfaced by drills feed back into earlier stages.

### Stage T6 — Public launch

- **T6a** — DNS for `rpc.testnet.princeps.<tld>`, `faucet.testnet.princeps.<tld>`, `grafana.testnet.princeps.<tld>` (Grafana read-only public dashboard for transparency)
- **T6b** — Public-RPC follower deploy (the read-only `princeps-lending-rpc-server` from Stage 24d behind a load balancer)
- **T6c** — Announcement post in `princeps/docs/announcements/`; README badge flip
- **T6d** — External-validator onboarding doc — how a 4th validator joins (genesis file, peer dial address, registration ceremony if any)

### Stage T7 — External validation window

- **T7a** — Invite ≥3 external validators
- **T7b** — Run `princeps lending-demo` from third-party hosts; collect feedback
- **T7c** — 30-day no-critical-incident window — incident = "consensus halt requiring foundation intervention" OR "validator state divergence detected"
- **T7d** — Post-mortem doc summarizing incidents (if any) and corrective work; feeds into the mainnet plan

Acceptance: window closes successfully → write the mainnet plan.

## Timeline (rough, single-developer pace)

| Stage | Scope | Weeks | Target |
|---|---|---|---|
| T1 | Genesis + chain-id | 1 | 2026-06-19 |
| T2 | Observability | 1 | 2026-06-26 |
| T3 | Key handling | 0.5 | 2026-06-30 |
| T4 | Faucet | 1 | 2026-07-07 |
| T5 | Internal dry-run + chaos drills | 2 | 2026-07-21 |
| T6 | Public launch | 1 | 2026-07-28 |
| T7 | 30-day external-validation window | 4+ | 2026-08-31 |
| **v0 testnet ships** | | | **late Q3 2026** |

~6 weeks build + 4+ weeks external-validation window. T7 is the gate to the mainnet plan.

## Acceptance criteria (v0 testnet "done")

1. Three foundation-operated validators producing blocks in three regions for ≥30 days
2. ≥3 external validators successfully joined and producing blocks
3. Public RPC + faucet reachable from the open internet, documented endpoints
4. Grafana dashboard public-readable; alert rules paging on validator-liveness loss + oracle staleness
5. Chaos drills (T5c) all pass on internal infra
6. ≥3 third-party developers complete `princeps lending-demo` against the public testnet (carries forward `v0-lending.md` acceptance criterion #8)
7. README status `🚧 v0 lending` → `✅ v0 lending (testnet)`
8. Mainnet plan drafted and reviewed before T7 window closes

## Open questions / risks

1. **Chain-id allocation** — which number, and do we register with `chainlist.org` pre-launch (claim) or post-launch (avoid squatting on an unused id)? Lean post-launch.
2. **Hosting providers** — three independent clouds × three regions is the strong form of TD-003. Cost vs fault-isolation tradeoff; bare-metal-at-Hetzner-only is cheaper but trades the multi-provider failure-domain guarantee.
3. **Validator key custody** — where do foundation validator keys live? Cloud KMS (HashiCorp Vault / AWS KMS) vs hardware (YubiHSM) vs disk + filesystem permissions. Lean disk for testnet, KMS or hardware for mainnet.
4. **Pre-mainnet E-4 absolute-value gas recalibration** — open from `v0-lending.md` open-question #4. Strictly speaking a mainnet gate, not a testnet gate, but the testnet hosts ARE production-grade hardware, so the calibration can happen *on testnet* using the criterion harness from `c71c99b`. Pull into T5b if hardware is representative.
5. **Faucet abuse** — captcha + rate-limit will catch casual abuse; determined drainers will route around. Acceptable for testnet because tokens have no value; for mainnet faucet (if any) it isn't. Out-of-scope here, note in T4.
6. **Public-RPC DoS surface** — TD-004 isolates validators from public traffic, but a hammered public RPC can still get expensive (compute on the follower, bandwidth, log volume). Add a CDN / rate-limiter in front; document as part of T6b.
7. **Time-sync requirement** — Malachite consensus is timestamp-sensitive. NTP / chrony required on every validator host; clock-skew > 1s should page. Add to TD-006 alert set.
8. **Operator agreement at testnet** — Layer 2 of ADR-010 calls for signed operator instances. Testnet operators (= the foundation, internally) don't need legal-grade signed instruments, but the template should be acknowledged in writing internally to keep the audit trail clean and validate the procedure works before v1 mainnet. Low-cost; do it in T3c.

## Suggested next action

**Stage T1 — start with `testnet-genesis.json` schema + the devnet-seed refactor**. This is the lowest-uncertainty entry point: it's purely in-repo Rust + JSON work, surfaces zero infra dependencies, and unblocks everything downstream (you can't provision a node without a genesis file to point it at). T2 (observability) can run in parallel if you want a second workstream — it has no dependency on T1 — but T1 is the critical path.

See [v0-lending.md](./v0-lending.md) for the protocol work this builds on. The mainnet plan is intentionally not drafted yet; it depends on what T7 surfaces.

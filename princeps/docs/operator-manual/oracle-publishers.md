# Oracle publisher onboarding (v0 testnet)

**Status**: First-cut, landed at T3b of [v0 testnet deploy plan](./../plans/v0-testnet-deploy.md).
**Scope**: v0 public testnet, single chain, single asset feed (`feed_id = 0` for ETH/USDC). v1+ will revisit the runtime-registration path called out in [§9 Open work](#9-open-work).
**Audience**: An operator preparing to run an oracle publisher, an existing publisher rotating their key, or a foundation engineer onboarding a new publisher to the active set.

This document is the "v0 operator manual" entry referenced by [operator-agreement.md §2](./../operator-agreement.md) for the oracle-publisher role.

---

## 1. What an oracle publisher does

Princeps takes its index price from a small set of independent publishers. Each publisher:

- Owns one **secp256k1 keypair** (one per feed they cover; today there's only `feed_id = 0`).
- Watches the underlying market off-chain (typically a CEX or aggregator).
- Periodically signs a `PriceObservation` with their private key and submits it to the bridge.
- Is identified on-chain by the **SEC1-compressed public key** registered in genesis for that feed.

The bridge aggregates fresh observations across all registered publishers (per-feed median + deviation filter — see [`rdk_oracle::compute::aggregate_index`](../../../crates/oracle/src/compute.rs)) into a single canonical `AggregatedPrice` per block. A publisher whose observations stop arriving or fail the staleness window stops contributing; if the set of fresh feeds drops below `OracleParams::min_feeds_required` the aggregate goes unavailable and downstream consumers (liquidation scan, funding rate) fall back to their no-oracle paths. Sustained unavailability ultimately trips the oracle circuit breaker (threat-model row O-3) and pages on-call.

At v0 the foundation runs three publishers (`foundation-eth-feed-{a,b,c}`) seeded in [`princeps/genesis/testnet-genesis.json`](../../genesis/testnet-genesis.json). External publishers join from T7 onward.

## 2. Wire format

Every publisher must conform to these formats exactly — any drift means the bridge rejects observations with `ObservationError::InvalidSignature`.

### 2.1 Public key

- Curve: **secp256k1** (the same curve as Ethereum addresses).
- Encoding: **SEC1-compressed**, 33 bytes (1-byte parity prefix + 32-byte X coordinate).
- On-chain representation: 33 bytes serialized as a `PublisherKey([u8; 33])`. See [`rdk_oracle::types::PublisherKey`](../../../crates/oracle/src/types.rs).
- Wire format in genesis JSON: lowercase hex, no `0x` prefix, 66 hex characters.

### 2.2 Signature

- Algorithm: **ECDSA over secp256k1**, raw `(r, s)` pair — IEEE P1363 format.
- Encoding: 64 bytes, big-endian `r || s`.
- The implementation accepts what [`k256::ecdsa::Signature::from_slice`](https://docs.rs/k256) accepts. DER-encoded signatures are NOT supported — use the raw format.
- See [`rdk_oracle::types::Signature`](../../../crates/oracle/src/types.rs).

### 2.3 Signed payload

Publishers sign exactly the 20 bytes returned by [`PriceObservation::signed_bytes`](../../../crates/oracle/src/types.rs):

```text
  [ 0..  4]  feed_id   (u32, big-endian)
  [ 4.. 12]  price     (u64, big-endian, smallest-unit per IndexPrice)
  [12.. 20]  timestamp (u64, big-endian, unix seconds)
```

Sign the **raw bytes**. `k256::ecdsa` hashes them with SHA-256 internally; you do NOT pre-hash. The verifier does the same on the bridge side, so any deviation produces a signature that won't verify.

Big-endian + fixed widths so every validator computes the same digest from the same observation.

### 2.4 Timestamp semantics

`timestamp` is the publisher-reported unix-second time of observation, NOT when rdk ingested it. The bridge rejects observations whose timestamp is:

- in the future relative to current block time (`ObservationError::FromFuture`);
- older than `OracleParams::staleness_window_secs` before current block time (`ObservationError::Stale`);
- earlier than a previously-stored observation from the same feed — replay guard (`ObservationError::Stale`).

The default staleness window is `60` seconds; the testnet runs the same. A publisher needs accurate NTP/chrony and should aim to submit observations within a small fraction of the window.

## 3. Generating a publisher key

The key never needs to leave the publisher's host; only the 33-byte public key is shared.

### 3.1 Conceptually

Any secp256k1 keypair works. The standard approach using the same crate the bridge verifies with (`k256` v0.13):

```rust
use k256::ecdsa::{SigningKey, VerifyingKey};
use rand_core::OsRng;

let signing_key = SigningKey::random(&mut OsRng);
let verifying_key: VerifyingKey = signing_key.verifying_key().clone();

// 33-byte SEC1-compressed public key — this is what lands in genesis.
let pubkey_compressed: [u8; 33] = verifying_key.to_encoded_point(true).as_bytes()
    .try_into().expect("compressed point is 33 bytes");

// Hex for the JSON form expected by genesis.
let pubkey_hex = hex::encode(pubkey_compressed);
println!("publisher pubkey: {pubkey_hex}");
```

### 3.2 Hardening expectations

Same expectations as a validator key (see [operator-agreement.md §2](./../operator-agreement.md) "Hosting posture"):

- Mint on an **air-gapped host**; never copy the private key off it in plaintext.
- Store the private key under **0o600** perms (file owner only).
- At v0 a passphrase-encrypted keystore file is recommended; at v1 mainnet **HSM use is required**. The `princeps validator gen-keystore` tooling shipped in T3a-2 covers the encrypted-at-rest shape; a dedicated `princeps oracle-publisher` subcommand to wrap this is on the v0 todo list but **not yet implemented** — for now publishers manage their own files using `k256` directly.
- Keep an **off-host encrypted backup** of the private key for recovery.

### 3.3 What you share with the foundation

Only the 33-byte SEC1-compressed public key, as lowercase hex. Communicate it through a channel the foundation can authenticate (e.g. a signed email, the public key paste-in step at validator-set bootstrap, or a PR onto the testnet-genesis branch). The pubkey itself is not a secret, but **binding it to the right operator's real-world identity is** — that binding is what makes the signed-by check meaningful.

## 4. Onboarding a new publisher (4th–Nth)

### 4.1 The v0 model: genesis-only registration

**At v0 the publisher registry is part of genesis.** The bridge re-registers every publisher from genesis on each boot. There is no runtime add/remove RPC. This means onboarding a 4th publisher to an already-running testnet requires a coordinated restart.

The flow:

1. **Prospective publisher** generates their secp256k1 keypair locally (§3.1).
2. **Prospective publisher** shares the 33-byte hex pubkey + the legal-entity identity disclosure from [operator-agreement.md §1](./../operator-agreement.md) with the foundation.
3. **Foundation** validates the pubkey:
   - Parses as 33 bytes (66 hex chars, no `0x` prefix).
   - Decodes as a SEC1-compressed point (catches typos that aren't a valid curve point).
   - Is not a duplicate of an existing entry under the same `feed_id`.
4. **Foundation** opens a PR amending [`princeps/genesis/testnet-genesis.json`](../../genesis/testnet-genesis.json) with a new entry in `oracle_publishers`:
   ```json
   {
     "feed_id": 0,
     "label": "<operator-short-name>-eth-feed",
     "pubkey_hex": "02..."
   }
   ```
   `label` is operator-facing prose; only `feed_id` and `pubkey_hex` are load-bearing on consensus. The label SHOULD start with the operator's name so the registry stays readable as it grows.
5. **PR review** confirms the pubkey came through the authenticated channel from step 2.
6. **Coordinated restart**: foundation announces the change in the testnet operator channel, validators restart against the amended genesis. Genesis-hash determinism (pinned by [T1d](./../plans/v0-testnet-deploy.md)) guarantees every validator sees the same new registry.
7. **New publisher** brings their publisher service up. The next block whose `OracleParams::min_feeds_required` floor is met with their contribution flips the `feeds_used` count in `AggregatedPrice` from N-1 to N.

**Restart is required** because the publisher registry currently lives in `OracleState`, which the bridge constructs from genesis at boot. The registry is **NOT** persisted in `CoordinatorSnapshot` — same pattern as `OperatorRegistry`; both are re-registered each boot from the canonical genesis source. Adding a publisher entry without a restart will not take effect.

### 4.2 What the bridge does at boot

The relevant code path is in [`princeps_evm`](../../../princeps/crates/evm) where the bridge calls `OracleState::register_publisher(feed, key)` for each entry. The behavior:

- If `feed_id` is new for this oracle, the entry is added.
- If `feed_id` already has a registered key, the new key **replaces** the old one. This is the rotation primitive (see §5).
- The bridge does NOT enforce a per-feed publisher count — N validators can all register independent keys for the same feed and they aggregate together.

The `register_publisher` call is idempotent across boots; restarting with an unchanged genesis re-registers the same keys and produces the same registry.

## 5. Key rotation

Publishers MUST rotate keys at a minimum cadence and SHOULD rotate sooner on any compromise indication.

### 5.1 Cadence

- **Minimum**: every 12 months, matching [operator-agreement.md §2](./../operator-agreement.md)'s validator-key rotation cadence.
- **Recommended**: every 6 months, or any time a key may have been exposed (host rebuild, snapshot leak, suspected breach).
- **Mandatory**: within 30 days of foundation notice if the foundation flags a class of keys as compromised (e.g. CVE in an upstream library).

### 5.2 Procedure

The on-chain primitive is the single-call replace baked into [`OracleState::register_publisher`](../../../crates/oracle/src/state.rs) — re-registering an existing `feed_id` with a new pubkey atomically swaps the key:

1. **Publisher** mints a new secp256k1 keypair, treating the old key as compromised the moment generation begins.
2. **Publisher** keeps the **old** publisher process running with the **old** key — observations continue to verify and contribute to the aggregate.
3. **Publisher** shares the new pubkey with the foundation through the same authenticated channel as onboarding (§4.1 step 2).
4. **Foundation** opens a PR amending the publisher's existing entry in `testnet-genesis.json` — `pubkey_hex` changes; `feed_id` and `label` stay the same.
5. **Coordinated restart**: foundation announces, validators restart. The registry now holds the new key for this publisher.
6. **Publisher** swaps their publisher process over to sign with the new key. Old key is destroyed.

There is **no overlap window** — the on-chain swap is atomic on the validator restart boundary. Step 2's old-key continuity exists so the aggregate doesn't lose a feed during PR review.

### 5.3 Audit trail

The rotation event is captured by:

- **Git history** on the testnet-genesis branch (the PR + merge commit is the source of truth).
- **Validator boot logs** — every validator prints the registered publisher set on boot.
- **Operator-agreement appendix** — operators are expected to record the rotation in their own ops log per the audit-handoff bundle.

There is no in-chain "rotation event" log at v0. ADR-010 Layer 3 introduced a chain-history append log for socialization declarations; a similar mechanism for publisher rotation is on the v1+ wishlist but **not in scope for the v0 testnet**.

## 6. De-registration triggers

A publisher key is removed (`OracleState::revoke_publisher(feed)`) — at v0, by removing the entry from `testnet-genesis.json` and triggering a coordinated restart — under any of these conditions:

### 6.1 Operator-initiated

- **Voluntary withdrawal** with ≥ 14 days notice ([operator-agreement.md §2](./../operator-agreement.md) "Operational discipline").
- **Reported key compromise** — the operator notifies the foundation; the entry is removed at the next coordinated restart, before the operator finishes minting a replacement.

### 6.2 Foundation-initiated (per [operator-agreement.md §3](./../operator-agreement.md))

- **Equivocation**: the same `(feed_id, timestamp)` signed for two different prices by the same publisher. Removal is immediate; the foundation file an incident report per the operator agreement.
- **Censorship**: the publisher provably skips an asset-price movement (e.g. mid-market drop > 1% sustained for > 5 minutes) without a corresponding observation.
- **Sustained absence**: no observations from this publisher within the staleness window for ≥ 24 hours unannounced.
- **Misrepresentation**: a price observation more than `OracleParams::deviation_bps_cap` away from the aggregated median for ≥ 3 consecutive blocks (the deviation filter already drops these; sustained presence indicates a malfunctioning publisher).
- **Identity-disclosure change without notice**: per [operator-agreement.md §1](./../operator-agreement.md).

### 6.3 What `revoke_publisher` actually does

Behavior is pinned by the test [`revoke_publisher_removes_key_but_keeps_stored_observations`](../../../crates/oracle/src/state.rs):

- The publisher's entry is removed from the registry. Subsequent `ingest_signed` calls for that publisher's `feed_id` (without a replacement key) fail with `ObservationError::UnknownFeed`.
- Already-stored observations in the per-feed record are **NOT** purged. They continue contributing to aggregates until they age out via the staleness window (≤ 60 seconds). This is intentional — yanking observations mid-block would be a determinism hazard.

For an immediate aggregate-state effect at de-registration, the foundation may also clear the per-feed record via the bridge's admin path — but at v0 this is rarely necessary: the staleness window does the work within a minute.

## 7. Operational expectations

These are the day-to-day commitments a publisher takes on. Numbers come from the v0 [`OracleParams::hyperliquid_default`](../../../crates/oracle/src/types.rs) profile; check the actual deployed config for the operative values.

- **Submission cadence**: at least once per `staleness_window_secs / 2` seconds — so 30 seconds on the v0 default. Faster is fine; slower means your feed misses windows when the network is busy or your host blips.
- **Clock**: NTP / chrony required. Clock skew > 1 second produces observations that the bridge may reject as future-dated. (See open question #7 of the [testnet deploy plan](./../plans/v0-testnet-deploy.md).)
- **Liveness SLO**: best-effort 99% monthly uptime on the publisher host (mirrors [operator-agreement.md §2](./../operator-agreement.md)). Sustained absence > 24 h triggers §6.2.
- **Network**: the publisher must be reachable from a validator's RPC endpoint or whatever submission path is documented for the deployment. v0 testnet uses [TBD — to be finalized by T6].
- **Monitoring**: every publisher SHOULD expose a metric for `last-submitted-observation-age-seconds`. The validator-side counterpart is the `princeps_oracle_circuit_breaker_trips_total` counter (T2b — see the [`Prometheus rules`](../../ops/alerts/princeps.rules.yaml)); see also the [TD-006 note](./../plans/v0-testnet-deploy.md) on the missing per-publisher age gauge.
- **Software discipline**: same as a validator — run the released `princeps` binary at the current or immediately prior tagged version unless coordinated with the foundation.

## 8. Quick reference: file paths

| What | Where |
|---|---|
| Oracle types (PublisherKey, Signature, signed_bytes) | [`crates/oracle/src/types.rs`](../../../crates/oracle/src/types.rs) |
| Signature verify path | [`crates/oracle/src/verify.rs`](../../../crates/oracle/src/verify.rs) |
| Register / revoke / lookup API | [`crates/oracle/src/state.rs`](../../../crates/oracle/src/state.rs) |
| Genesis publisher block | [`princeps/genesis/testnet-genesis.json`](../../genesis/testnet-genesis.json) → `oracle_publishers` |
| Genesis loader struct | [`princeps/bin/princeps/src/genesis.rs`](../../bin/princeps/src/genesis.rs) → `GenesisPublisher` |
| Operator agreement (the contract this manual is referenced from) | [`princeps/docs/operator-agreement.md`](./../operator-agreement.md) |
| Threat-model rows on oracle | [`princeps/docs/threat-model.md`](./../threat-model.md) → O-1, O-2, O-3 |
| Testnet deploy plan | [`princeps/docs/plans/v0-testnet-deploy.md`](./../plans/v0-testnet-deploy.md) → TD-006 §oracle observation age |

## 9. Open work

Not in v0 scope; tracked here so the gap is explicit.

- **Per-publisher observation-age gauge**. TD-006 calls for it; the deferred-list comment in [`princeps_node::metrics`](../../crates/node/src/metrics.rs) flags it as well. Until then, the oracle paging rule in [`princeps.rules.yaml`](../../ops/alerts/princeps.rules.yaml) piggybacks on the circuit-breaker counter — operators can't see WHICH publisher fell behind without checking the publisher's own logs.
- **Runtime registration RPC**. Today every onboarding requires a coordinated restart. A `register_publisher` precompile (akin to the ADR-010 Layer 3 `socialize` entrypoint) would let the foundation onboard a new publisher without restarting validators. Has to live behind operator-key admission ([T3c](./../plans/v0-testnet-deploy.md)) — arbitrary callers cannot mutate the publisher registry.
- **In-chain rotation event log**. The chain-history mechanism added for ADR-010 Layer 3 socialization is the natural home for publisher rotation events too. v1+.
- **`princeps oracle-publisher` subcommand**. A counterpart to `princeps validator gen-keystore` — generate a secp256k1 keypair, write an encrypted keystore, print the SEC1-compressed pubkey. Removes the "publishers manage their own files" caveat in §3.2.

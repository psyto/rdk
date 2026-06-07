# Operator-key onboarding for ADR-010 Layer 3 (v0 testnet)

**Status**: First-cut, landed at T3c of [v0 testnet deploy plan](./../plans/v0-testnet-deploy.md).
**Scope**: v0 public testnet. Documents the operator-key registry behind [ADR-010](./../adr/010-bad-debt-depletion-policy.md) Layer 3 — the mechanism that lets an authorized operator declare "socialize this much residual bad debt across depositors" on a lending market, with the chain enforcing operator-signature admission.
**Audience**: A foundation engineer wiring up the operator-0 entry at testnet cut, an external operator preparing for v1 mainnet onboarding, or an auditor tracing the authority chain on a socialization event.

Companion to [oracle-publishers.md](./oracle-publishers.md). Both docs are entries in the "v0 operator manual" referenced by [operator-agreement.md §2](./../operator-agreement.md).

---

## 1. What an operator does

An **operator** is the authority allowed to invoke ADR-010 Layer 3 — the depositor-haircut branch of the bad-debt depletion policy. When the bridge-side insurance fund (Layer 2) is depleted and a lending market still has unfilled residual debt after liquidation, the operator signs a [`SocializationDeclaration`](../../crates/node/src/operator.rs) naming the market and the residual amount; the chain verifies the signature against the registry, applies a pro-rata `supply_index` haircut to depositors of that market, and appends a `Socialization` event to the chain-history log for audit.

The operator role is **rare-but-authoritative**:

- **Rare** — under healthy market conditions Layer 3 never fires. Most blocks have zero operator activity. Compare to oracle publishers, who submit on every tick.
- **Authoritative** — a single valid declaration can move depositor balances. There is no consensus override at admission; the registered operator's signature IS the authority. This is why the trust model is tighter than for publishers, and why the operator-agreement is a precondition for any non-foundation operator.

At v0 the foundation registers itself as `operator-0`. External operators come in at v1 mainnet, with each candidate first signing [operator-agreement.md](./../operator-agreement.md) as the legal counterpart to the on-chain key admission.

### 1.1 Operators vs validators

These are **distinct roles** with separate key material:

|  | Validator | Operator |
|---|---|---|
| Authority over | Block production, consensus | ADR-010 Layer 3 socialization |
| Key type | Ed25519 (Malachite consensus) | secp256k1 (matches oracle/EVM) |
| Activity frequency | Per-block | Per-incident (rare) |
| Registered in | Malachite validator-set (genesis + restart) | `OperatorRegistry` (genesis + restart) |
| On-chain registry | `validators` block in `testnet-genesis.json` | `operators` block in `testnet-genesis.json` |

A real-world entity may hold both roles, but the **keys are separate**. A compromised validator key does not give an attacker Layer 3 authority and vice versa. The hosting-posture expectations are similar but the key material lives in different files.

## 2. Wire format

Identical to the oracle-publisher wire format on key and signature shape (deliberate — `princeps_node::operator` "mirrors the `rdk_oracle::PublisherKey` pattern exactly"). The signed payload differs because the message is different.

### 2.1 Operator key

- Curve: **secp256k1**.
- Encoding: **SEC1-compressed**, 33 bytes (1-byte parity prefix + 32-byte X coordinate).
- On-chain representation: [`OperatorKey([u8; 33])`](../../crates/node/src/operator.rs).
- Wire format in genesis JSON: lowercase hex, no `0x` prefix, 66 hex characters.

### 2.2 Operator signature

- Algorithm: **ECDSA over secp256k1**, raw `(r, s)` pair — IEEE P1363 format.
- Encoding: 64 bytes, big-endian `r || s`.
- On-chain representation: [`OperatorSignature([u8; 64])`](../../crates/node/src/operator.rs).
- DER-encoded signatures are NOT accepted; use the raw format. The implementation calls [`k256::ecdsa::Signature::from_slice`](https://docs.rs/k256) on the 64 bytes.
- `OperatorSignature::ZERO` is the unsigned placeholder; it is explicitly rejected by [`verify_socialization_declaration`](../../crates/node/src/operator.rs) — no risk of an accidentally-unsigned declaration being mistaken for a valid one.

### 2.3 Signed payload

Operators sign exactly the 32 bytes returned by [`SocializationDeclaration::signed_bytes`](../../crates/node/src/operator.rs):

```text
  [ 0..  4]  operator     (u32, big-endian)
  [ 4..  8]  market_id    (u32, big-endian)
  [ 8.. 24]  unfilled     (u128, big-endian)
  [24.. 32]  block_height (u64, big-endian)
```

Sign the **raw bytes** — `k256::ecdsa` hashes with SHA-256 internally; do NOT pre-hash. The verifier mirrors the publisher path: same `k256::ecdsa::VerifyingKey::verify(msg, sig)` call shape.

### 2.4 Replay protection

The `block_height` field is the replay guard. A captured signature for `(operator, market, unfilled, height=100)` cannot be replayed at `height=101` because the signed payload bytes change, requiring a fresh signature the attacker cannot produce. This means an operator MUST include the height they intend the declaration to apply at — typically the next block they expect a validator to admit it into.

There is no separate nonce. The `(operator, market_id, block_height)` triple is the de-facto admission key; a second declaration for the same triple at the same height would re-apply the haircut (the precompile / CLI guards against this at the bridge level, not via the operator signature itself).

## 3. Generating an operator key

Same procedure and hardening posture as a publisher key, with one extra consideration noted below.

### 3.1 Conceptually

```rust
use k256::ecdsa::{SigningKey, VerifyingKey};
use rand_core::OsRng;

let signing_key = SigningKey::random(&mut OsRng);
let verifying_key: VerifyingKey = signing_key.verifying_key().clone();

// 33-byte SEC1-compressed public key — what lands in genesis.
let pubkey_compressed: [u8; 33] = verifying_key.to_encoded_point(true).as_bytes()
    .try_into().expect("compressed point is 33 bytes");

let pubkey_hex = hex::encode(pubkey_compressed);
println!("operator pubkey: {pubkey_hex}");
```

For testing only, the codebase ships a deterministic-seed signing helper at [`princeps_node::operator::demo_signing`](../../crates/node/src/operator.rs) — `demo_signing_key(seed)` + `demo_operator_key(seed)`. Production paths MUST NOT use these; the module doc-comment says so explicitly. Their purpose is to make the `princeps socialize` CLI demo and the operator-module unit tests produce verifiable signatures in-process.

### 3.2 Hardening expectations

Stricter than for an oracle publisher because Layer 3 authority is incident-grade. From [operator-agreement.md §2 "Hosting posture"](./../operator-agreement.md):

- **Air-gapped key generation**. The operator key is signing authority over depositor balances; it never touches a host that can be reached from the public internet during normal operation.
- **Cold storage during normal operation**. Unlike a validator or publisher key — which has to sign on every block / tick — an operator key may go months without producing a signature. Keep it offline. A hardware key (YubiHSM, AWS CloudHSM, Ledger with the relevant app, etc.) is recommended at v0 and **required at v1 mainnet** per the operator-agreement.
- **0o600 perms** on any plaintext copy of the private key.
- **Encrypted backup off-host** — recovery must not depend on a single physical device surviving.

### 3.3 What you share with the foundation

Only the 33-byte SEC1-compressed public key, as lowercase hex. Same authentication channel as a publisher pubkey — signed email, GitHub PR signed-off-by from the operator's authenticated GitHub identity, or in-person handoff at validator-set bootstrap.

Critically: the **mapping from `OperatorId` to real-world legal entity** must be recorded out-of-band at the same time. ADR-010 Layer 3 admits any declaration signed by the registered key for an `OperatorId`; if the foundation can't bind the ID to a specific legal counterparty, the audit trail for a socialization event has no anchor.

## 4. Onboarding flow

### 4.1 At v0: foundation registers itself as `operator-0`

The relevant genesis entry already exists with a placeholder pubkey:

```json
{
  "operator_id": 0,
  "label": "foundation-op-0",
  "pubkey_hex": "020000..."
}
```

(see [`testnet-genesis.json`](../../genesis/testnet-genesis.json) → `operators`). The placeholder must be replaced with a real foundation-controlled pubkey **before** the testnet cut where Layer 3 admission becomes meaningful (T5b — internal chaos drills include a forced socialization).

Steps for the foundation engineer at cut time:

1. **Mint** a new secp256k1 keypair on an air-gapped host (§3.1). Treat this key the same way the operator-agreement treats a validator key — air-gapped generation, HSM custody by v1.
2. **Record** the operator-0 ↔ foundation-legal-entity binding in the audit-handoff bundle (per [operator-agreement.md](./../operator-agreement.md)'s Q1 2027 audit handoff reference). At v0 this is internal; at v1 the operator-agreement's signed instance is the binding.
3. **PR** amending `testnet-genesis.json` → replace `operator-0`'s `pubkey_hex` with the real value. Same review process as the publisher PR (§4 of [oracle-publishers.md](./oracle-publishers.md)).
4. **Coordinated restart** of the testnet validator set against the amended genesis. Genesis-hash determinism guarantees every validator sees the same operator registry. Per the boot path, `OperatorRegistry::register(OperatorId(0), foundation_key)` runs on every validator — pinned by the chain-history hookup that already happens in the bin per [`princeps/bin/princeps/src/main.rs`](../../bin/princeps/src/main.rs) → "ADR-010 Layer 3 — install the operator-key registry".
5. **Verify** the registration on each validator by checking the boot log line that prints the registered operators (one per `OperatorRegistry::iter()` entry).

`operator-0` is then admissible as the signer of `SocializationDeclaration` at the `princeps socialize` CLI or the on-chain `princeps_lending_socialize` precompile at `0x0c27`.

### 4.2 What the bridge does at boot

Same pattern as the publisher registry. The bridge:

- Constructs an empty `OperatorRegistry`.
- For each entry in genesis `operators`, calls `OperatorRegistry::register(OperatorId(operator_id), OperatorKey(pubkey_bytes))`.
- Installs the registry behind the `princeps_lending_socialize` precompile via `install_operator_registry`.

The registry is **NOT** included in `CoordinatorSnapshot` — re-registration each boot from the canonical genesis source is the deliberate design (rationale: keeps the snapshot deterministic across deployments with different operator sets — same justification used for publishers).

### 4.3 At v1 mainnet: registering an external operator

External operators are not on the v0 testnet path. The plan-mandated v1 procedure:

1. **Operator-agreement execution**. The prospective operator signs a fully-executed instance of [operator-agreement.md](./../operator-agreement.md) with the foundation. This is the legal binding for the on-chain `OperatorId → real entity` mapping; no operator goes on-chain without it. Identity disclosure (operator-agreement §1) is shared with every existing operator before the new entry is added.
2. **Key generation + sharing** following §3.1 and §3.3. v1 mainnet requires HSM custody for the private key (operator-agreement §2 hosting posture).
3. **Foundation validates** the 33-byte SEC1 point shape and confirms the pubkey came through the authenticated channel from step 2.
4. **PR** amending the mainnet equivalent of `testnet-genesis.json` adding a new entry:
   ```json
   {
     "operator_id": <next-unused>,
     "label": "<operator-name>-op-<id>",
     "pubkey_hex": "02..."
   }
   ```
   Choosing the next-unused `operator_id` (incrementing) keeps the BTreeMap iteration order stable and the validator boot logs readable.
5. **Coordinated upgrade window**. At v1 mainnet this may not be a hard validator-restart but a planned upgrade window depending on what the mainnet deployment story looks like — TBD by the mainnet plan, which T7 of the testnet plan gates.
6. **First-event tabletop**. Before going live with admission authority, the new operator runs a tabletop exercise: sign an off-chain declaration against the v0 testnet, walk the verification path with the foundation, and document the procedure in their ops runbook. The first real declaration is not the first time they've produced one.

## 5. Operator-agreement template acknowledgement (v0 testnet)

Per the v0 testnet plan's [open question #8](./../plans/v0-testnet-deploy.md):

> Testnet operators (= the foundation, internally) don't need legal-grade signed instruments, but the template should be acknowledged in writing internally to keep the audit trail clean and validate the procedure works before v1 mainnet.

For v0 cut:

- **Internal acknowledgement** of [operator-agreement.md](./../operator-agreement.md) by the foundation principal acting as `operator-0`. This is an internal artifact, not a legal instrument — a signed copy stored alongside the audit-handoff bundle suffices.
- **Procedure dry-run**: walk the §4.3 v1 mainnet steps end-to-end with the foundation acting as the prospective external operator. Surface any gaps in the doc (back-edit this file or open follow-up work) before T7 closes.

The signed instance lands in the foundation's internal docs store; this manual does not cover the storage location.

## 6. Key rotation

Same on-chain primitive as publishers — `OperatorRegistry::register` is idempotent on the ID, so re-registering an `OperatorId` with a new key is the atomic rotation operation. Behavior is pinned by `register_replaces_prior_key_for_same_id` in [`princeps_node::operator` tests](../../crates/node/src/operator.rs).

### 6.1 Cadence

- **Minimum**: every 12 months, matching the validator-key and publisher-key cadence ([operator-agreement.md §2](./../operator-agreement.md)).
- **Recommended**: every 6 months OR after any incident where Layer 3 has fired (the key was used; the auditor trail benefits from a fresh key per real event).
- **Mandatory**: immediately on any compromise indication. Unlike a validator key — where rotation can wait for a coordinated window — operator-key rotation should land at the next planned restart at minimum, because the authority being rotated is socialization, not block production.

### 6.2 Procedure

1. **Operator** generates a new key off-chain (§3.1), continuing to hold the old key as the active signer.
2. **Operator** shares the new pubkey with the foundation through the authenticated channel.
3. **Foundation** opens a PR amending the operator's existing entry in the genesis file — `pubkey_hex` changes, `operator_id` and `label` stay the same. Same review process as onboarding.
4. **Coordinated restart**. Validators boot against the amended genesis, the registry now holds the new key for this `OperatorId`.
5. **Operator** destroys the old private key.

**No declaration is in flight during this procedure.** Unlike block production or oracle observation, an operator declaration only exists when bad debt has occurred — rotation can happen during quiescent state without disrupting anything.

## 7. De-registration triggers

The current registry exposes `register` and `get`/`iter`/`len` but **not** an explicit `revoke`. Removing an operator means removing their entry from the genesis file and triggering a coordinated restart; the next boot constructs an `OperatorRegistry` without that entry, so the un-registered operator's declarations fail verification with `OperatorAuthError::UnknownOperator`.

Triggers, in increasing severity:

### 7.1 Operator-initiated

- **Voluntary withdrawal** with ≥ 14 days notice ([operator-agreement.md §2](./../operator-agreement.md)). The operator has not used the key in months; removing them is administrative.
- **Reported key compromise**. The operator notifies the foundation; the entry is removed at the next coordinated restart, BEFORE the operator finishes minting a replacement, so no window exists where the old key could still admit a declaration.

### 7.2 Foundation-initiated (per [operator-agreement.md §3](./../operator-agreement.md))

- **Unauthorized declaration**: any `SocializationDeclaration` submitted that does not correspond to a legitimate bad-debt event under [ADR-010](./../adr/010-bad-debt-depletion-policy.md). This is the equivalent of validator equivocation — the on-chain trail is sufficient evidence and removal is immediate.
- **Failure to declare when required**: bad debt depletes the insurance fund (Layer 2) but the operator does not submit a Layer 3 declaration within the foundation's escalation window (TBD by the operational runbook — T5b drafts this). The operator may be replaced rather than removed if a successor is available.
- **Identity-disclosure change without notice** ([operator-agreement.md §1](./../operator-agreement.md)).
- **Withdrawal from the operator-agreement** before a successor is provisioned. Treated as voluntary withdrawal §7.1 with immediate effect.

### 7.3 What removal actually does

- The operator's entry is gone from the registry. Subsequent `verify_socialization_declaration` calls naming that `OperatorId` fail with `OperatorAuthError::UnknownOperator`.
- Past declarations applied while the operator was active are **NOT** reversed. The chain-history `Socialization` events stay, the `supply_index` haircuts stay. Removal is forward-looking.
- The `OperatorId` should not be re-used for a different operator. Pick the next unused integer instead; the BTreeMap doesn't care, and a fresh ID per principal keeps the audit trail unambiguous.

## 8. Operational expectations

Different shape from publishers (event-driven, not periodic). What an active operator commits to:

- **Monitoring**: subscribe to the `princeps_insurance_fund_balance` and `princeps_liquidation_unfilled_deficit` gauges (from T2b — see [the alert rules](../../ops/alerts/princeps.rules.yaml)). A persistently negative insurance fund or sustained unfilled liquidation deficit is the precursor to a Layer 3 event; the operator should know about it as soon as the foundation does.
- **Escalation availability**: respond within the foundation-defined SLA on a paged Layer 3 incident. The actual SLA number is TBD in the v0 operational runbook (T5b); current placeholder is "within 4 hours of the foundation page".
- **Decision authority**: ADR-010 Layer 3 is a **manual-declaration** branch — the operator decides how much residual to socialize, against which market, at what block height. This is a deliberate design choice; there is no autonomous Layer 3. The operator is expected to follow the runbook (sit with the foundation on the incident call, confirm the bad-debt numbers, sign + submit), not act unilaterally.
- **Key custody discipline**: §3.2 hardening expectations are continuous obligations, not one-time setup.
- **No competing infrastructure**: an operator must not also run a publisher / validator for the same chain unless the operator-agreement explicitly contemplates it. v0 testnet's foundation-operates-everything posture is the exception, not the model.

## 9. Quick reference: file paths

| What | Where |
|---|---|
| Operator types (`OperatorKey`, `OperatorSignature`, `OperatorRegistry`) | [`princeps/crates/node/src/operator.rs`](../../crates/node/src/operator.rs) |
| `SocializationDeclaration` + `signed_bytes` layout | [`princeps/crates/node/src/operator.rs`](../../crates/node/src/operator.rs) |
| `verify_socialization_declaration` | [`princeps/crates/node/src/operator.rs`](../../crates/node/src/operator.rs) |
| `demo_signing` helpers (test/CLI ONLY — never production) | [`princeps/crates/node/src/operator.rs`](../../crates/node/src/operator.rs) `mod demo_signing` |
| Genesis operator block | [`princeps/genesis/testnet-genesis.json`](../../genesis/testnet-genesis.json) → `operators` |
| Genesis loader struct | [`princeps/bin/princeps/src/genesis.rs`](../../bin/princeps/src/genesis.rs) → `GenesisOperator` |
| Bin boot wiring (`install_operator_registry`) | [`princeps/bin/princeps/src/main.rs`](../../bin/princeps/src/main.rs) → "ADR-010 Layer 3" block |
| CLI v0 entrypoint | [`princeps/bin/princeps/src/main.rs`](../../bin/princeps/src/main.rs) → `Command::Socialize` and [`socialize.rs`](../../bin/princeps/src/socialize.rs) |
| EVM v0 entrypoint | `princeps_lending_socialize` precompile at `0x0c27`, see [`princeps/crates/evm/src/precompiles`](../../crates/evm/src/precompiles) |
| Lending primitive (the actual haircut) | [`princeps/crates/lending/src/socialization.rs`](../../crates/lending/src/socialization.rs) |
| ADR-010 | [`princeps/docs/adr/010-bad-debt-depletion-policy.md`](../../docs/adr/010-bad-debt-depletion-policy.md) |
| Operator agreement template | [`princeps/docs/operator-agreement.md`](./../operator-agreement.md) |
| Companion oracle-publisher manual | [`oracle-publishers.md`](./oracle-publishers.md) |

## 10. Open work

Not in v0 scope; tracked here so the gap is explicit.

- **Explicit `revoke` API**. Today removal is done by amending genesis and restarting. A `revoke_operator(id)` method on `OperatorRegistry` would simplify the runtime path if/when registration becomes runtime-mutable (parallel to the oracle's open work). Low-cost addition once the rest of the runtime-mutability story exists.
- **In-chain rotation event log**. Currently rotation is tracked only in git history. ADR-010 Layer 3 already added a chain-history mechanism for socialization events; extending it to operator-key rotation is natural and would make the audit trail self-contained on-chain. v1+.
- **`princeps operator-key` subcommand**. Counterpart to `princeps validator gen-keystore` (T3a-2). Mint a secp256k1 keypair, write an encrypted keystore, print the SEC1-compressed pubkey. Removes the "operators manage their own files" caveat in §3.2.
- **Per-operator declaration metrics**. Today the only signal is the existence of a chain-history `Socialization` event after the fact. A counter per `OperatorId` would feed dashboards and surface "operator-X has not declared in 18 months — is the key still in service?" review prompts.
- **Multi-operator threshold admission**. ADR-010 explicitly does NOT require multi-sig for Layer 3 at v0–v1 (single operator signature is sufficient). For higher-stakes deployments a 2-of-3 or 3-of-5 admission scheme would be a future hardening, but it changes the signed-payload shape and the verification path — out of scope until ADR-010 is revisited.

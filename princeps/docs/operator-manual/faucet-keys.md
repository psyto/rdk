# Faucet wallet custody (v0 testnet)

**Status**: First-cut, landed at T4b of [v0 testnet deploy plan](./../plans/v0-testnet-deploy.md).
**Scope**: v0 public testnet. Documents the secp256k1 wallet that backs `princeps-faucet` — generation, storage, funding, monitoring, rotation, and incident response.
**Audience**: A foundation engineer provisioning the faucet for testnet cut, or an operator running an external mirror.

Companion to [oracle-publishers.md](./oracle-publishers.md) and [operator-keys.md](./operator-keys.md). All three are entries in the v0 operator manual.

---

## 1. What the faucet wallet is

The faucet wallet is a secp256k1 EOA (externally-owned account) that:

- Holds testnet ETH.
- Signs EIP-1559 transfers to recipient addresses on every successful `POST /drip`.
- Lives in a passphrase-encrypted Web3 V3 keystore on disk (the format `princeps validator gen-keystore` does NOT produce — that one's Ed25519 for consensus; the faucet uses standard secp256k1 because it signs real EIP-1559 txs that a Reth-shape node will execute).

**Risk profile differs from every other key in the system.** A leaked validator key compromises consensus; a leaked oracle key lets an attacker forge prices; a leaked operator key lets them socialize bad debt. A leaked faucet key just lets the attacker drain the faucet's daily payout cap. At testnet that's tokens with no economic value — the operational cost is the wallet refill + the brief faucet outage while the key rotates.

That makes the faucet wallet the **lowest-stakes** of the four key types. It does NOT need an HSM at v0 or v1; a passphrase-encrypted file under 0o600 is sufficient. The hardening focus is operational: monitoring the balance, rate-limiting the drain, and rotating fast on any compromise indication.

### 1.1 Faucet wallet vs everything else

|  | Validator | Oracle publisher | Operator | **Faucet** |
|---|---|---|---|---|
| Curve | Ed25519 | secp256k1 | secp256k1 | **secp256k1** |
| Authority | Consensus | Price truth | Socialization | **Token drain** |
| Activity | Per-block | Per-tick (~30s) | Per-incident (rare) | **Per-drip request** |
| At-rest format | T3a-2 encrypted JSON (Ed25519) | Operator-managed | Operator-managed | **Web3 V3 (scrypt+AES-128-CTR)** |
| Custody requirement (v1) | HSM required | HSM required | HSM required | **Disk + passphrase sufficient** |
| Rotation cadence | 12 months | 12 months | 6–12 months | **As often as operationally convenient** |
| Compromise blast radius | Chain halt | Bad prices for one feed | One wrong haircut | **Daily payout cap** |

## 2. Wire format

The faucet uses the **Web3 Secret Storage V3** keystore — the de-facto Ethereum format readable by every tool in the ecosystem (geth `account new`, `cast wallet new`, foundry, ethers, web3.js, …). Not the T3a-2 validator-keystore format. **Don't try to reuse the validator gen-keystore CLI for this**:

- T3a-2 stores 32 bytes of an Ed25519 private key (consensus signing).
- Faucet needs 32 bytes of a secp256k1 scalar (transaction signing).
- Same envelope shape would technically work, but using the universal V3 format means operators don't have to learn a princeps-specific format for a key that's morally an Ethereum wallet.

### 2.1 V3 structure

```json
{
  "address": "abcdef...",                 // 20-byte EVM address, no 0x
  "id": "uuid-v4",
  "version": 3,
  "crypto": {
    "cipher": "aes-128-ctr",
    "cipherparams": { "iv": "..." },
    "ciphertext": "...",
    "kdf": "scrypt",
    "kdfparams": { "dklen": 32, "n": 262144, "p": 1, "r": 8, "salt": "..." },
    "mac": "..."
  }
}
```

The faucet binary decrypts this via [`alloy_signer_local::PrivateKeySigner::decrypt_keystore`](../../bin/princeps-faucet/src/chain.rs) — no custom KDF or cipher code in the princeps tree; we lean on alloy's vetted implementation.

### 2.2 What gets stored where

| Item | Location | Perms |
|---|---|---|
| Keystore JSON | `/etc/princeps-faucet/wallet-keystore.json` (deployment convention) | 0o600, owned by the faucet process user |
| Passphrase | `systemd-creds`-encrypted file, mounted via `LoadCredential=` | inherited from `LoadCredential` |
| Wallet address (public) | `chain.wallet_keystore_path` resolves it at boot; printed at startup | non-secret |

Never store the passphrase in the same file or directory tree as the keystore. The whole point of the passphrase is that a filesystem snapshot containing the keystore must stay useless without it.

## 3. Generating the keystore

Three equivalent paths. The address bytes printed at the end are the public faucet address; everything else stays on the operator's air-gapped machine.

### 3.1 With `cast` (foundry — recommended)

```bash
# Air-gapped host, no network deps required for the mint itself.
cast wallet new --json /etc/princeps-faucet/wallet-keystore.json
# Prompts for passphrase twice (never echoed); writes the V3 JSON.
chmod 0600 /etc/princeps-faucet/wallet-keystore.json
chown princeps-faucet: /etc/princeps-faucet/wallet-keystore.json
```

`cast wallet new` outputs the address on stdout. Capture it; you'll need it to fund the wallet (§6).

### 3.2 With geth

```bash
geth account new --datadir /etc/princeps-faucet
mv /etc/princeps-faucet/keystore/UTC--*--<address> \
   /etc/princeps-faucet/wallet-keystore.json
rmdir /etc/princeps-faucet/keystore
chmod 0600 /etc/princeps-faucet/wallet-keystore.json
```

### 3.3 With a programmatic mint

Any library that produces V3 keystores works:

```js
// ethers (illustrative)
import { Wallet, encryptKeystoreJson } from "ethers";
const wallet = Wallet.createRandom();
const json = await wallet.encrypt(process.env.PASSPHRASE);
fs.writeFileSync("/etc/princeps-faucet/wallet-keystore.json", json, { mode: 0o600 });
console.log(wallet.address);
```

`princeps-faucet` does NOT ship its own `gen-wallet` subcommand at v0 — the ecosystem tools are mature and operators are expected to use one. A first-party subcommand is tracked in §10 open work.

### 3.4 Hardening expectations

Mirror the publisher / operator key hardening, scaled down for the lower-stakes role:

- **Air-gapped generation**: the host the keystore is minted on should not be reachable from the internet at mint time. Move the keystore + record the address by hand or USB; never paste the passphrase into a chat window.
- **0o600 perms** on the keystore file. Owned by the user the faucet runs as (not root — the faucet binary doesn't need root).
- **Off-host backup** of the keystore, encrypted at rest with a different passphrase than the boot one. Recovery story: re-derive the same secp256k1 key from the backup → re-encrypt under a new boot passphrase → resume.
- **No HSM at v0 or v1.** The blast radius doesn't justify the complexity. v2+ might add HSM support if drain limits go up substantially.

## 4. Passphrase delivery

Two production-grade paths and one for local dev. **TTY input is intentionally unsupported** by the faucet binary — `princeps-faucet` is a daemon, not an interactive tool.

### 4.1 systemd `LoadCredential=` (recommended for production)

```ini
# /etc/systemd/system/princeps-faucet.service
[Service]
ExecStart=/usr/bin/princeps-faucet serve \
  --config /etc/princeps-faucet/config.json \
  --wallet-keystore-passphrase-file %d/wallet-passphrase

LoadCredential=wallet-passphrase:/etc/princeps-faucet/wallet-passphrase.enc
# (encrypted with `systemd-creds encrypt`; decrypted into %d at start time
#  and removed when the service stops)
```

`%d` is the per-unit credentials directory, tmpfs-mounted and isolated from other services. The passphrase exists in plaintext for the lifetime of the service and never lands on disk.

### 4.2 `--wallet-keystore-passphrase-stdin` (for CI / scripted boots)

```bash
echo -n "$FAUCET_PASSPHRASE" | princeps-faucet serve \
  --config /etc/princeps-faucet/config.json \
  --wallet-keystore-passphrase-stdin
```

Mind the shell history. `set +o history` first, or use `read -s` to populate the env var.

### 4.3 Plain file (last resort)

```bash
princeps-faucet serve \
  --config /etc/princeps-faucet/config.json \
  --wallet-keystore-passphrase-file /etc/princeps-faucet/passphrase
```

Requires the same 0o600 perms as the keystore itself, owned by the faucet user. Acceptable for local dev where systemd-creds isn't set up; **not recommended** for production because it sits on disk continuously.

The faucet validates that the passphrase is non-empty and rejects mutual `--file` + `--stdin` invocation; missing-both-flags exits with a clear hint.

## 5. Funding the wallet

The faucet wallet starts empty after `cast wallet new`. Before serving its first drip, top it up.

### 5.1 First fund (testnet cut day)

1. **Get the wallet address** — captured from §3.
2. **Source the funds**:
   - For v0 testnet: from a foundation-controlled genesis allocation. Add the faucet address to the `genesis` block of the chain spec with a starting balance, OR transfer from a foundation deployment wallet that was funded at genesis.
   - For a non-foundation external mirror: from another faucet (testnet ETH is exchangeable across mirrors by definition).
3. **Verify on-chain**: `cast balance <faucet-address> --rpc-url http://...`.

The faucet itself does NOT need to be running for the initial fund; it just needs the address to be reachable when `serve` boots.

### 5.2 Sizing the initial balance

```
initial_balance ≥ drip.eth_amount_wei × global.max_drips × refill_period_days × buffer
```

With v0 testnet defaults (0.1 ETH / drip, 100 drips / hour global cap, refill every 7 days, 2× buffer):

```
0.1 × 100 × 24 × 7 × 2 = 3360 testnet ETH
```

Round up generously — testnet ETH is free to mint at genesis, and an underfunded faucet that bricks mid-week is a worse failure mode than over-provisioning.

### 5.3 Refill cadence

Subscribe to the `princeps_faucet_wallet_balance_wei` gauge once it lands (T4a-5 hasn't wired the metric yet — tracked in §10). Until then, weekly `cast balance` checks via cron are sufficient. Refill when balance drops to ~30% of the §5.2 budget.

Refill procedure mirrors the initial-fund — transfer from the foundation deployment wallet via standard `cast send <faucet-addr> --value <amount>`.

## 6. Monitoring

### 6.1 What to watch

Today:

- `up{job=~"princeps-faucet"}` — the Prometheus scrape from [`PrincepsScrapeDown`](../../ops/alerts/princeps.rules.yaml) shape (need to copy the rule for the faucet job; not wired yet — §10).
- Faucet boot log lines — `wallet keystore loaded`, `rate-limit state:`, and the `verify_chain_id()` success/failure message. Anomalies show up in `journalctl -u princeps-faucet`.

Once T4a-5's wallet-balance gauge lands:

- `princeps_faucet_wallet_balance_wei` < refill threshold → ticket
- `princeps_faucet_wallet_balance_wei == 0` → page (faucet wallet drained — service is down for paying drips)

### 6.2 What NOT to surface

The faucet's `/status` endpoint deliberately omits:

- Wallet balance (would leak drain progress to script kiddies probing how much budget remains)
- Rate-limit table row counts (same reason)

The only thing `/status` exposes is configured limits + chain ID + version + uptime. The actual operational metrics belong on the internal Prometheus / Grafana dashboard.

## 7. Rotation

Cheap and quick. The faucet wallet is the easiest of the four key types to rotate because there's no on-chain registry to update — the chain doesn't know which wallet is "the faucet"; it just sees normal EOA transfers.

### 7.1 When to rotate

- **Suspected compromise**: rotate immediately (§9).
- **Routine hygiene**: every 6 months, or whenever the foundation engineer who minted the original key leaves.
- **Operator handoff** (if external mirrors emerge in T7): the new operator mints their own wallet; the old one isn't transferred.
- **Refill from a different funding source**: optional — fresh wallet per funding cycle is overkill but valid.

### 7.2 Procedure

1. **Mint a new wallet** following §3 with a fresh passphrase.
2. **Fund the new wallet** following §5 from the foundation deployment wallet.
3. **Drain the old wallet** by sending all but ~21000 × max_fee_per_gas wei back to the deployment wallet (or to the new faucet wallet directly — saves a hop).
4. **Stop the faucet service** (`systemctl stop princeps-faucet`).
5. **Swap the keystore + passphrase**: update `wallet_keystore_path` in the faucet config to point at the new file; re-encode the new passphrase with `systemd-creds encrypt`.
6. **Start the faucet service**. Verify the boot log shows the new wallet address.
7. **Destroy the old keystore** (`shred -u <old-keystore>`) once you've confirmed the new wallet processes a test drip cleanly.

There's no overlap window — drips just stop for the ~minute the service is down. At testnet that's fine.

## 8. De-commissioning

Same as rotation but step 7 is the goal, not the cleanup. Used when the faucet is being permanently retired (e.g. when v0 testnet is sunset and the v1 mainnet faucet supersedes it).

The retired keystore + passphrase should be archived to the audit-handoff bundle ([operator-agreement.md §audit role](./../operator-agreement.md)) per the same retention policy as other historical key material — it may need to be produced if a question arises about what addresses the faucet ever signed from.

## 9. Incident response: suspected compromise

The faucet wallet is the lowest-stakes key, but a known-compromised key is still a key. The response is fast rotation, not investigation.

### 9.1 Indicators

- Wallet balance dropped faster than the rate-limit math allows
- `eth_getTransactionCount` jumped without a corresponding number of `POST /drip` 200s
- Unexpected transactions from the faucet address visible on a block explorer
- Passphrase delivery channel (systemd-creds file, password manager entry, etc.) showed signs of access

### 9.2 Response

1. **Stop the faucet service** immediately (`systemctl stop princeps-faucet`).
2. **Drain the wallet** to a fresh foundation-controlled address (do NOT send to your usual deployment wallet — assume the attacker is watching it). Use a different passphrase / different operator host than the suspected compromised one to sign the drain tx.
3. **Mint a fresh wallet** following §7.
4. **Bring the faucet back** with the new wallet.
5. **Post-mortem**: how did the passphrase or keystore leak? Update the procedure if necessary. File the incident in the audit-handoff bundle.

Step 2 is the only time-sensitive one — the attacker is racing you to drain the remaining balance. The faucet outage during this is acceptable.

## 10. Quick reference: file paths

| What | Where |
|---|---|
| Wallet keystore loader (alloy V3 decrypt) | [`princeps/bin/princeps-faucet/src/chain.rs`](../../bin/princeps-faucet/src/chain.rs) → `load_wallet` |
| Boot-time chain_id verification | [`princeps/bin/princeps-faucet/src/chain.rs`](../../bin/princeps-faucet/src/chain.rs) → `AlloyEthSender::verify_chain_id` |
| Passphrase-source CLI | [`princeps/bin/princeps-faucet/src/main.rs`](../../bin/princeps-faucet/src/main.rs) → `read_wallet_passphrase` |
| Faucet config schema | [`princeps/bin/princeps-faucet/src/config.rs`](../../bin/princeps-faucet/src/config.rs) → `FaucetConfig` / `ChainConfig` |
| Reference deployment config | [`princeps/ops/faucet/config.example.json`](../../ops/faucet/config.example.json) |
| Faucet rate-limit semantics | [`princeps/bin/princeps-faucet/src/rate_limit.rs`](../../bin/princeps-faucet/src/rate_limit.rs) |
| Companion: validator key custody (T3a stack) | [`operator-keys.md`](./operator-keys.md) §1.1 has the role comparison |
| v0 testnet deploy plan | [`v0-testnet-deploy.md`](./../plans/v0-testnet-deploy.md) → TD-007 |
| Operator agreement template | [`operator-agreement.md`](./../operator-agreement.md) |

## 11. Open work

- **`princeps_faucet_wallet_balance_wei` gauge**. T4a-5 doesn't expose the wallet balance as a metric yet — operators need it to drive the §6 low-balance alert. Land it alongside the per-drip counter the T2 stack established for other metrics.
- **Per-drip latency histogram + outcome counter**. `princeps_faucet_drip_duration_seconds` + `princeps_faucet_drips_total{outcome=allowed|rate_limited|captcha_failed|chain_unavailable|...}`. Feeds dashboards + capacity planning.
- **`PrincepsFaucetScrapeDown` / `PrincepsFaucetWalletEmpty` alert rules**. Mirror the [`princeps.rules.yaml`](../../ops/alerts/princeps.rules.yaml) shape with a `job=~"princeps-faucet.*"` filter.
- **`princeps-faucet gen-wallet` subcommand**. Counterpart to `princeps validator gen-keystore` (T3a-2) but using the Web3 V3 format so the output is portable to every Ethereum tool. Removes the §3 "operators use external tools" caveat.
- **`princeps-faucet drain` subcommand**. Subcommand-of-last-resort that sends the entire wallet balance to a specified recipient (used during rotation §7.3 and incident response §9.2). Today operators do this with `cast send` — fine but error-prone (mistype the value field, lose funds). A first-party `drain` avoids that.
- **USDC support**. Out of scope at T4a-5 — bridge-side accounting indexed by `AccountId(u64)`, not an ERC-20 at an EVM address. See [v0-testnet-deploy.md TD-007 footer](./../plans/v0-testnet-deploy.md) for the deferral context and the v1 mainnet open question (deploy USDC as ERC-20 vs codify address↔AccountId convention).

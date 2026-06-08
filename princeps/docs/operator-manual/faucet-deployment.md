# Faucet deployment runbook (v0 testnet)

**Status**: First-cut, landed at T4c of [v0 testnet deploy plan](./../plans/v0-testnet-deploy.md). Closes Stage T4.
**Scope**: v0 public testnet. Stands up a `princeps-faucet` instance behind a reverse proxy on a dedicated host, reachable at `faucet.testnet.princeps.<tld>`.
**Audience**: A foundation engineer provisioning the testnet faucet host, or an operator running an external mirror.

Companion to [faucet-keys.md](./faucet-keys.md) (wallet provisioning, funding, rotation, incident response). This doc is the **operational** half; faucet-keys is the **custody** half. Read faucet-keys first if you haven't yet — this doc assumes the wallet keystore exists.

---

## 1. Architecture

Three independent processes on (typically) one host:

```
   Public Internet
        │ HTTPS
        ▼
  ┌──────────────────────┐
  │  Reverse proxy       │  nginx | caddy | similar
  │  • TLS termination   │  Let's Encrypt or equivalent
  │  • X-F-F insertion   │  (strips inbound, adds own)
  │  • basic rate-limit  │  defense-in-depth before the faucet's own
  └──────────┬───────────┘
             │ HTTP, 127.0.0.1:8080
             ▼
  ┌──────────────────────┐                        ┌────────────────────┐
  │  princeps-faucet     │  serve --config X     │  Prometheus        │
  │  • POST /drip        │                        │  • scrapes :9091   │
  │  • GET /status       │  ──── 127.0.0.1:9091 ──▶│  • drives alerts   │
  │  • GET /health       │     /metrics           │                    │
  └──────────┬───────────┘                        └────────────────────┘
             │ HTTPS, JSON-RPC
             ▼
  ┌──────────────────────┐
  │  princeps-lending-   │  Read-only follower per TD-004 — public
  │  rpc-server (or      │  RPC traffic NEVER touches the validator's
  │  validator RPC)      │  consensus surface.
  └──────────────────────┘
```

Three architectural rules from the [v0-testnet-deploy.md](./../plans/v0-testnet-deploy.md):

- **TD-007: faucet is a separate process.** Faucet outage cannot degrade the chain. Don't bundle the faucet with a validator.
- **TD-004: public-RPC isolation.** The faucet's outbound JSON-RPC must hit a read-only follower, not a validator's RPC endpoint directly.
- **TD-006: observability surface on a separate port.** The `/metrics` listener on 9091 (or whatever `metrics_bind` resolves to) is for internal Prometheus only — never expose via the reverse proxy.

## 2. systemd unit

The reference unit. Adjust paths to your filesystem layout.

```ini
# /etc/systemd/system/princeps-faucet.service
[Unit]
Description=Princeps v0 testnet faucet
After=network-online.target
Wants=network-online.target

[Service]
Type=exec
User=princeps-faucet
Group=princeps-faucet

# --- credentials ---
# Encrypt the passphrase with `systemd-creds encrypt - /etc/princeps-faucet/wallet-passphrase.enc <<< 'YOUR-PASSPHRASE'`
# (root-only encryption — only this unit can decrypt). The plaintext lands in
# %d, a per-service tmpfs, and is purged when the unit stops.
LoadCredentialEncrypted=wallet-passphrase:/etc/princeps-faucet/wallet-passphrase.enc

# --- exec ---
ExecStart=/usr/local/bin/princeps-faucet serve \
  --config /etc/princeps-faucet/config.json \
  --wallet-keystore-passphrase-file %d/wallet-passphrase

# --- restart policy ---
Restart=on-failure
RestartSec=10s
StartLimitInterval=300s
StartLimitBurst=5

# --- sandboxing (defense-in-depth) ---
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectControlGroups=yes
NoNewPrivileges=yes
SystemCallArchitectures=native
LockPersonality=yes
MemoryDenyWriteExecute=yes
RestrictRealtime=yes
RestrictSUIDSGID=yes
ReadWritePaths=/var/lib/princeps-faucet
# Network: must reach the RPC URL + the captcha provider's API + nothing else.
# Restrict via IPAddressAllow on a hardened host; default-allow is fine for
# v0 testnet behind a corporate egress firewall.

# --- logging ---
StandardOutput=journal
StandardError=journal
SyslogIdentifier=princeps-faucet

[Install]
WantedBy=multi-user.target
```

User + group + state dir setup, run once before enabling:

```bash
useradd --system --no-create-home --shell /usr/sbin/nologin princeps-faucet
install -d -o princeps-faucet -g princeps-faucet -m 0700 /var/lib/princeps-faucet
install -d -o root           -g princeps-faucet -m 0750 /etc/princeps-faucet
install -o princeps-faucet -g princeps-faucet -m 0600 wallet-keystore.json /etc/princeps-faucet/
install -o root           -g princeps-faucet -m 0640 config.json           /etc/princeps-faucet/
```

The `state_db_path` in `config.json` points at `/var/lib/princeps-faucet/state.db`. The faucet auto-creates parents on first boot.

## 3. Reverse proxy

The reverse proxy is **load-bearing on security**, not just convenience. From the [drip handler's inline doc](../../bin/princeps-faucet/src/server.rs):

> X-Forwarded-For is trusted unconditionally. The deployment runbook (T4c) MUST require operators to terminate inbound at a reverse proxy that STRIPS any inbound X-F-F and inserts its own. Without that, per-IP limits can be dodged by header forgery.

Two equivalent templates. Pick one.

### 3.1 nginx

```nginx
# /etc/nginx/sites-available/faucet.testnet.princeps.example
server {
    listen 443 ssl http2;
    listen [::]:443 ssl http2;
    server_name faucet.testnet.princeps.example;

    # TLS via Let's Encrypt (certbot --nginx) or your own cert.
    ssl_certificate     /etc/letsencrypt/live/faucet.testnet.princeps.example/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/faucet.testnet.princeps.example/privkey.pem;
    ssl_protocols       TLSv1.2 TLSv1.3;

    # Defense-in-depth rate limit BEFORE the faucet's own. Cheap to apply
    # at the proxy — drops obviously-abusive clients without consuming
    # SQLite write capacity downstream. Sized 10× the faucet's per-IP
    # limit so legitimate traffic never hits this first.
    limit_req_zone $binary_remote_addr zone=faucetip:10m rate=10r/m;

    location = /drip {
        limit_req zone=faucetip burst=5 nodelay;
        proxy_pass http://127.0.0.1:8080;
        proxy_http_version 1.1;

        # CRITICAL: drop any X-F-F the client sent and set our own.
        # Without these two lines, per-IP rate-limit is bypassable
        # by header forgery.
        proxy_set_header X-Forwarded-For $remote_addr;
        proxy_set_header X-Real-IP       $remote_addr;
        # If you previously had `proxy_set_header X-Forwarded-For
        # $proxy_add_x_forwarded_for`, REPLACE it — that variant
        # appends to the client-supplied header, which is the
        # forgeable case.

        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_read_timeout 30s;
    }

    location = /health {
        # Load-balancer probe target. No rate limit.
        proxy_pass http://127.0.0.1:8080;
        proxy_set_header X-Forwarded-For $remote_addr;
        proxy_set_header Host $host;
    }

    location = /status {
        # Operational read-only endpoint. Light rate limit;
        # operators sometimes hit this from scripts.
        limit_req zone=faucetip burst=2 nodelay;
        proxy_pass http://127.0.0.1:8080;
        proxy_set_header X-Forwarded-For $remote_addr;
        proxy_set_header Host $host;
    }

    # Everything else: 404, no proxying. Closes the door on
    # /metrics, /admin, /.env, and the long tail of probe paths.
    location / {
        return 404;
    }
}

# Plain HTTP → permanent redirect to HTTPS.
server {
    listen 80;
    listen [::]:80;
    server_name faucet.testnet.princeps.example;
    return 301 https://$host$request_uri;
}
```

### 3.2 caddy

Simpler, with automatic TLS via Let's Encrypt:

```caddy
# /etc/caddy/Caddyfile
faucet.testnet.princeps.example {
    # Caddy by default DOES NOT trust client-supplied X-F-F, which is
    # exactly the property we need. The `trusted_proxies` directive
    # below establishes the *only* upstream we trust to set headers
    # — set this to your load balancer's address if you have one, or
    # leave it untouched for direct exposure.
    handle /drip {
        rate_limit {
            zone faucetip {
                key {remote_host}
                window 1m
                events 10
            }
        }
        reverse_proxy 127.0.0.1:8080 {
            header_up X-Forwarded-For {remote_host}
            header_up X-Real-IP       {remote_host}
        }
    }

    handle /health {
        reverse_proxy 127.0.0.1:8080
    }

    handle /status {
        rate_limit {
            zone faucetip
        }
        reverse_proxy 127.0.0.1:8080 {
            header_up X-Forwarded-For {remote_host}
        }
    }

    # Closed by default — only the three routes above are reachable.
    handle {
        respond 404
    }
}
```

(The `rate_limit` directive requires the `caddy-ratelimit` module; install with `xcaddy build --with github.com/mholt/caddy-ratelimit`.)

### 3.3 What the proxy does NOT do

- **Captcha verification**: the faucet calls hCaptcha itself (T4a-4). Don't double-verify at the proxy.
- **`/metrics` exposure**: the Prometheus scrape endpoint (`metrics_bind` in the faucet config) is for internal Prometheus only. **Never reverse-proxy it through the public domain.** Leave it bound to 127.0.0.1 or a private interface.
- **Wallet-balance gating**: the proxy doesn't know the wallet balance. The faucet itself will (once the §10-tracked balance gauge lands) emit `chain_unavailable` if it can't broadcast — the proxy passes that through unchanged.

## 4. DNS + TLS

### 4.1 DNS

Single A record (and AAAA if you support IPv6) for `faucet.testnet.princeps.<tld>` pointing at the faucet host's public IP. Document the record in the operator runbook's DNS zone file as part of the testnet's `T6a` work item.

### 4.2 TLS

Let's Encrypt is the path of least resistance.

For nginx, use certbot:

```bash
certbot --nginx -d faucet.testnet.princeps.example \
  --email ops@princeps.example --agree-tos --redirect
```

For caddy, TLS is automatic — no certbot needed. Just keep port 80 reachable for the ACME challenge.

In either case, monitor cert expiry:

```yaml
# Prometheus blackbox_exporter probe; rule in alerts/princeps.rules.yaml
- alert: PrincepsFaucetCertExpiringSoon
  expr: probe_ssl_earliest_cert_expiry{instance="faucet.testnet.princeps.example"} - time() < 7 * 24 * 3600
  for: 6h
  labels:
    severity: warning
    area: faucet
```

(Not in the committed alert rules yet — tracked in [faucet-keys.md §10](./faucet-keys.md#10-open-work).)

## 5. First boot checklist

Order matters — each step depends on the previous one succeeding.

1. **DNS** record propagated for `faucet.testnet.princeps.<tld>`. Verify with `dig`.
2. **Faucet user + state dir created** (§2 setup commands).
3. **Wallet keystore + passphrase provisioned** ([faucet-keys.md §3 + §4](./faucet-keys.md)). Run `cast wallet address <keystore>` to capture the public address.
4. **Wallet funded** with the §5.2 budget ([faucet-keys.md §5.2](./faucet-keys.md#52-sizing-the-initial-balance)).
5. **Faucet config installed** at `/etc/princeps-faucet/config.json` per `config.example.json`. Update:
   - `listen_addr` → `127.0.0.1:8080` (behind proxy)
   - `metrics_bind` → `127.0.0.1:9091` (internal Prometheus only)
   - `chain_id` → real testnet chain ID
   - `rate_limit` → final knobs
   - `captcha.provider` → **`hcaptcha`** (NOT `disabled`)
   - `chain.rpc_url` → the read-only follower's RPC URL
   - `chain.wallet_keystore_path` → `/etc/princeps-faucet/wallet-keystore.json`
6. **Reverse proxy configured** (§3) + tested with `nginx -t` / `caddy validate`.
7. **TLS cert issued** (§4.2).
8. **Faucet enabled + started**:
   ```bash
   systemctl daemon-reload
   systemctl enable --now princeps-faucet
   journalctl -u princeps-faucet -f
   ```
   Watch for the four boot lines: metrics endpoint, rate-limit state, captcha provider (NOT the disabled-warn), wallet loaded with address.
9. **Reverse proxy enabled** (`systemctl reload nginx` / `systemctl reload caddy`).
10. **End-to-end test from a third-party host**:
    ```bash
    curl -X POST https://faucet.testnet.princeps.example/drip \
      -H 'content-type: application/json' \
      -d '{"address":"0x<your-recipient>","captcha_token":"<solved-token>"}'
    ```
    Expect 200 + a real tx hash. Verify on a block explorer or via `cast tx <hash>`.
11. **Prometheus scrape** added to the testnet's Prometheus config:
    ```yaml
    - job_name: princeps-faucet
      scrape_interval: 30s
      static_configs:
        - targets: ['127.0.0.1:9091']  # scrape from the faucet host itself
    ```
12. **Alert rules added** (mirror `princeps.rules.yaml`; faucet-specific rules are §10 open work).

## 6. Backup + restore

Three artifacts to back up. Different update cadences → different backup strategies.

| Artifact | Where | Backup strategy |
|---|---|---|
| `/etc/princeps-faucet/wallet-keystore.json` | static (changes on rotation) | Offsite encrypted backup at mint time + after each rotation. [faucet-keys.md §3.4](./faucet-keys.md#34-hardening-expectations) is the source of truth. |
| `/etc/princeps-faucet/config.json` | semi-static | Track in your infra repo (it has no secrets — passphrase is in systemd-creds, not the config). |
| `/var/lib/princeps-faucet/state.db` | mutates per drip | Periodic snapshot. Loss of state.db means rate-limit budgets reset (slightly worse user experience — an attacker who knew about the loss could re-drip immediately) but no funds are at risk. Daily snapshots are sufficient at v0; if needed, increase. |

The wallet **passphrase** itself is backed up separately, in a passphrase manager / vault product, NOT alongside the keystore. The systemd-creds encrypted file is a copy of the production deployment artifact, not a backup of the secret.

### 6.1 Restore from full host loss

```bash
# 1. Spin up a fresh host. Install princeps-faucet binary + systemd unit.
# 2. Restore the keystore from the offsite backup.
# 3. Restore the config from the infra repo.
# 4. Re-encrypt the passphrase with systemd-creds on the NEW host:
systemd-creds encrypt - /etc/princeps-faucet/wallet-passphrase.enc <<< 'YOUR-PASSPHRASE'
# 5. Restore state.db from the most recent snapshot (optional — losing
#    it just resets rate-limit buckets).
# 6. Re-issue the TLS cert (§4.2) for the new host.
# 7. Update DNS to point at the new host's IP.
# 8. systemctl start princeps-faucet + reload the reverse proxy.
```

`systemd-creds` ties the encryption to the host TPM by default. Don't try to copy the encrypted file across hosts; re-encrypt on the destination.

## 7. Monitoring + alerting

### 7.1 What the faucet emits today

Prometheus on port `metrics_bind` (default `:9091`) exposes:

- Process-level metrics from the default `metrics-exporter-prometheus` collector (memory, CPU, file descriptors).
- Faucet-specific metrics: **none yet at T4a-5**. T4a tracks `princeps_faucet_drips_total{outcome=...}`, `princeps_faucet_drip_duration_seconds`, and `princeps_faucet_wallet_balance_wei` as the named gaps in [faucet-keys.md §10](./faucet-keys.md#11-open-work).

### 7.2 What you can alert on today

Until the faucet-specific metrics land:

- `up{job="princeps-faucet"} == 0` for 2m → page (faucet scrape down). Mirrors the existing `PrincepsScrapeDown` rule pattern in [`princeps.rules.yaml`](../../ops/alerts/princeps.rules.yaml) — just swap the job label.
- Process restart frequency via `process_start_time_seconds` deltas → ticket (signals a crash loop).
- The HTTPS probe from §4.2 → page on cert expiry within 7 days.

### 7.3 What to alert on once the metrics land

- `princeps_faucet_wallet_balance_wei == 0` → page (faucet drained, drips return 503).
- `princeps_faucet_wallet_balance_wei < <refill threshold>` for 1h → ticket.
- `rate(princeps_faucet_drips_total{outcome="chain_unavailable"}[5m]) > 0` for 5m → page (downstream RPC broken).
- `rate(princeps_faucet_drips_total{outcome="captcha_unreachable"}[5m]) > 0` for 5m → page (hCaptcha unreachable; both 5xx and Network variants).
- `histogram_quantile(0.95, princeps_faucet_drip_duration_seconds_bucket) > 3s` for 10m → ticket (capacity warning).

### 7.4 Grafana

Mirror the `princeps-overview` dashboard ([`princeps/ops/grafana/princeps-overview.json`](../../ops/grafana/princeps-overview.json)) with a `princeps-faucet` dashboard. Three rows once the metrics exist:

1. **Service health** — uptime, drip rate, error rate by outcome
2. **Wallet** — balance over time, drips-per-refill-cycle
3. **Latency** — drip-duration p50/p95/p99, broken down by outcome bucket

## 8. Common ops

### 8.1 View logs

```bash
journalctl -u princeps-faucet            # last boot
journalctl -u princeps-faucet -f         # follow
journalctl -u princeps-faucet --since="1 hour ago" -p err
```

The faucet emits at `info` by default. To debug, override with `RUST_LOG=princeps_faucet=debug,reqwest=debug` in the unit:

```ini
[Service]
Environment="RUST_LOG=princeps_faucet=debug,reqwest=debug"
```

### 8.2 Stop / start / restart

```bash
systemctl stop princeps-faucet           # graceful — no graceful shutdown wiring yet, just a SIGTERM
systemctl start princeps-faucet
systemctl restart princeps-faucet        # equivalent to stop + start
systemctl reload nginx                   # config change in the reverse proxy
```

### 8.3 Drain + rotate the wallet

Cross-reference [faucet-keys.md §7](./faucet-keys.md#7-rotation). The procedure assumes the systemd unit is the one this runbook documents.

### 8.4 Incident response

Cross-reference [faucet-keys.md §9](./faucet-keys.md#9-incident-response-suspected-compromise) for compromise indicators + the drain-fast procedure. From this runbook's POV, the operational steps are:

1. `systemctl stop princeps-faucet` immediately
2. Drain the wallet from a different host (don't reuse the compromised host)
3. Follow [faucet-keys.md §7](./faucet-keys.md#7-rotation) with a fresh keystore
4. `systemctl start princeps-faucet`
5. Post-mortem into the audit-handoff bundle

## 9. Quick reference

| What | Where |
|---|---|
| Faucet binary | `/usr/local/bin/princeps-faucet` |
| Config | `/etc/princeps-faucet/config.json` |
| Keystore | `/etc/princeps-faucet/wallet-keystore.json` (0o600) |
| Passphrase (encrypted) | `/etc/princeps-faucet/wallet-passphrase.enc` (systemd-creds) |
| Rate-limit state | `/var/lib/princeps-faucet/state.db` |
| systemd unit | `/etc/systemd/system/princeps-faucet.service` |
| nginx config | `/etc/nginx/sites-available/faucet.testnet.princeps.example` |
| caddy config | `/etc/caddy/Caddyfile` |
| Logs | `journalctl -u princeps-faucet` |
| Metrics scrape | `127.0.0.1:9091/metrics` (internal) |
| Public endpoint | `https://faucet.testnet.princeps.<tld>` |
| Faucet src — entry point | [`princeps/bin/princeps-faucet/src/main.rs`](../../bin/princeps-faucet/src/main.rs) |
| Faucet src — chain + wallet | [`princeps/bin/princeps-faucet/src/chain.rs`](../../bin/princeps-faucet/src/chain.rs) |
| Faucet src — drip handler | [`princeps/bin/princeps-faucet/src/server.rs`](../../bin/princeps-faucet/src/server.rs) |
| Reference config | [`princeps/ops/faucet/config.example.json`](../../ops/faucet/config.example.json) |
| Companion: wallet custody | [`faucet-keys.md`](./faucet-keys.md) |
| Companion: oracle publishers | [`oracle-publishers.md`](./oracle-publishers.md) |
| Companion: operator keys | [`operator-keys.md`](./operator-keys.md) |
| Plan TD-007 (architectural decisions) | [`v0-testnet-deploy.md`](./../plans/v0-testnet-deploy.md) |
| Existing alert rules | [`princeps/ops/alerts/princeps.rules.yaml`](../../ops/alerts/princeps.rules.yaml) |

## 10. Open work

Not blocking the v0 testnet faucet from going live, but worth tracking.

- **Faucet-specific Prometheus rules**. Today the runbook references the alerts shape but the rules YAML doesn't carry the `princeps-faucet` job filter. Land alongside the wallet-balance gauge ([faucet-keys.md §10](./faucet-keys.md#11-open-work)).
- **Faucet Grafana dashboard**. §7.4 specifies the shape; needs the metrics to exist first.
- **TLS cert expiry monitoring**. The blackbox_exporter probe in §4.2 isn't wired into the standing alert config.
- **Reverse-proxy templating**. nginx + caddy are documented but not generated — a small `ops/faucet/nginx.conf.template` + `Caddyfile.template` would make T4c reproducible from config.
- **Multi-region failover**. v0 ships single-region. T7's external-validator window may surface latency complaints from far-away regions that justify a second mirror.
- **Bot-net abuse modeling**. The §3 proxy rate-limit + the faucet's own rate-limit handle casual abuse. A determined botnet rotating IPs across a /16 will exhaust the per-recipient + global caps. Acceptable at v0 testnet where tokens have no value; documented here so the v1 mainnet plan can revisit (probably with a stricter captcha + a smaller per-drip amount).
- **Faucet outage SLA**. v0 doesn't promise one; T7 external-validator feedback may surface a number.

## 11. Stage T4 summary

Stage T4 of [v0-testnet-deploy.md](./../plans/v0-testnet-deploy.md) is now closed:

- T4a-1 — crate skeleton (axum + /health + /status)
- T4a-2 — SQLite-backed rate limiter (per-IP + per-recipient + global)
- T4a-3 — POST /drip with stubbed transfer + rate-limit wiring
- T4a-4 — captcha verification gate (hCaptcha + Disabled providers)
- T4a-5 — real EIP-1559 broadcast via alloy + JSON-RPC (ETH-only; USDC deferred)
- T4b   — faucet wallet custody, funding, rotation, incident response (in operator-manual)
- **T4c** — this document — deployment runbook

The faucet is ready for testnet provisioning. Next: Stage T5 — internal 3-validator dry-run on real infra, with chaos drills.

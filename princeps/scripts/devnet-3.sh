#!/usr/bin/env bash
# scripts/devnet-3.sh
#
# Boot a 3-validator princeps reth-devnet locally and verify all three
# converge on identical state. Reproducible equivalent of the manual
# alice/bob/carol walkthrough in docs/testing.md, so the multi-validator
# safety net (Stage 18a follower replication + Stage 13l peer dial list)
# stays exercised and regressions show up immediately.
#
# Usage:
#   ./scripts/devnet-3.sh                  # default: 3 rounds
#   PRINCEPS_ROUNDS=10 ./scripts/devnet-3.sh
#
# Validator keys persist under $PRINCEPS_DATA_ROOT/{a,b,c}; only bridge
# and coordinator state are wiped each run so every invocation is a
# fresh chain on the same set of validator identities. Tear down on
# exit kills all three processes.
#
# Exit code: 0 if all three nodes produced byte-identical coordinator
# snapshots, non-zero otherwise.

set -euo pipefail

ROUNDS="${PRINCEPS_ROUNDS:-3}"
ROOT="${PRINCEPS_DATA_ROOT:-/tmp/princeps-devnet-3}"
BIN="${PRINCEPS_BIN:-target/release/princeps}"

NODES=(a b c)
MONIKERS=(alice bob carol)
P2P_PORTS=(27656 27657 27658)
RPC_BINDS=(127.0.0.1:18545 127.0.0.1:18546 127.0.0.1:18547)

if [[ ! -x "$BIN" ]]; then
  echo "Building release binary at $BIN…"
  cargo build --release --bin princeps
fi

mkdir -p "$ROOT"

# Step 1 — ensure each node has a validator key + pubkey sidecar.
# A fresh --rounds 0 boot generates them; existing keys are reused.
# Sequential to avoid Reth/libp2p port collisions during keygen.
for i in 0 1 2; do
  DD="$ROOT/${NODES[$i]}"
  KEY="$DD/validator-key.json"
  PK="$DD/validator-pubkey.hex"
  mkdir -p "$DD"
  if [[ ! -f "$KEY" || ! -f "$PK" ]]; then
    echo "Generating validator key for ${MONIKERS[$i]} (one-time)…"
    "$BIN" reth-devnet \
      --moniker "${MONIKERS[$i]}" \
      --data-dir "$DD" \
      --listen-addr "/ip4/127.0.0.1/tcp/${P2P_PORTS[$i]}" \
      --rpc-bind "${RPC_BINDS[$i]}" \
      --rounds 0 \
      > "$ROOT/${NODES[$i]}-keygen.log" 2>&1
  fi
done

# Step 2 — write the shared validators.json from the three pubkey
# sidecars. Equal voting power; >2/3 quorum needs all three.
VALIDATORS="$ROOT/validators.json"
{
  echo '{'
  echo '  "validators": ['
  for i in 0 1 2; do
    PUBKEY=$(tr -d '[:space:]' < "$ROOT/${NODES[$i]}/validator-pubkey.hex")
    COMMA=","
    [[ $i -eq 2 ]] && COMMA=""
    printf '    { "pubkey_hex": "%s", "voting_power": 1, "peer_multiaddr": "/ip4/127.0.0.1/tcp/%d" }%s\n' \
      "$PUBKEY" "${P2P_PORTS[$i]}" "$COMMA"
  done
  echo '  ]'
  echo '}'
} > "$VALIDATORS"

echo "Wrote shared validators.json:"
cat "$VALIDATORS"
echo

# Step 3 — wipe prior bridge / coordinator / reth state so each run
# starts from a fresh chain. Validator keys are preserved.
for i in 0 1 2; do
  DD="$ROOT/${NODES[$i]}"
  rm -rf "$DD/bridge" "$DD/coordinator" "$DD/reth"
done

# Step 4 — boot the three nodes in parallel against the shared set.
PIDS=()
cleanup() {
  trap - EXIT
  echo "Tearing down validators…"
  for pid in "${PIDS[@]:-}"; do
    kill "$pid" 2>/dev/null || true
  done
  wait 2>/dev/null || true
}
trap cleanup EXIT INT TERM

for i in 0 1 2; do
  DD="$ROOT/${NODES[$i]}"
  LOG="$ROOT/${NODES[$i]}.log"
  echo "Booting ${MONIKERS[$i]}: p2p tcp/${P2P_PORTS[$i]}, rpc ${RPC_BINDS[$i]}, log $LOG"
  "$BIN" reth-devnet \
    --moniker "${MONIKERS[$i]}" \
    --data-dir "$DD" \
    --validators "$VALIDATORS" \
    --listen-addr "/ip4/0.0.0.0/tcp/${P2P_PORTS[$i]}" \
    --rpc-bind "${RPC_BINDS[$i]}" \
    --rounds "$ROUNDS" \
    > "$LOG" 2>&1 &
  PIDS+=($!)
done

echo
echo "Driving $ROUNDS round(s) across 3 validators…"
EXIT_OK=true
for pid in "${PIDS[@]}"; do
  if ! wait "$pid"; then
    EXIT_OK=false
  fi
done

if ! $EXIT_OK; then
  echo
  echo "FAIL: one or more validators exited non-zero. Tail of each log:"
  for i in 0 1 2; do
    echo "--- ${MONIKERS[$i]} (last 20 lines) ---"
    tail -20 "$ROOT/${NODES[$i]}.log" || true
  done
  exit 1
fi

# Step 5 — verify byte-identical coordinator snapshots. Bridge
# snapshots also match in principle but include block timestamps that
# can differ trivially across processes; coordinator state is the
# authoritative cross-check.
echo
echo "Verifying convergence…"
PASS=true
for i in 1 2; do
  REF="$ROOT/${NODES[0]}/coordinator/state.json"
  CMP="$ROOT/${NODES[$i]}/coordinator/state.json"
  if [[ ! -f "$REF" || ! -f "$CMP" ]]; then
    echo "FAIL: missing coordinator snapshot — $REF or $CMP"
    PASS=false
    continue
  fi
  if ! diff -q "$REF" "$CMP" > /dev/null; then
    echo "FAIL: ${MONIKERS[0]} vs ${MONIKERS[$i]} coordinator snapshots differ"
    diff "$REF" "$CMP" | head -30 || true
    PASS=false
  else
    echo "OK: ${MONIKERS[0]} ≡ ${MONIKERS[$i]}"
  fi
done

if $PASS; then
  echo
  echo "PASS: all three coordinator snapshots are byte-identical after $ROUNDS round(s)."
  exit 0
else
  exit 1
fi

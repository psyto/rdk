# Bring your own scenario

Reactor's sample scenarios run our design at fixed values. This page is for running **yours**: your
market's margins and fee, the stress you worry about, and what your design must guarantee, on the
same engine, on your machine.

A scenario is one JSON file. You don't touch the engine's code.

## What you can change today

| | What | Where in the file |
|---|---|---|
| Your design | initial margin, maintenance margin, liquidation fee (basis points) | `params` |
| The stress | any sequence of deposits, orders (limit or market) and oracle moves, block by block | `history.blocks` |
| What must hold | checks on liquidations, ADL, fills, the final mark, and any account's collateral, position or entry | `expected_outcomes` |

Not yet configurable from a file: funding (cap and interval), oracle staleness, the insurance fund's
starting size, ADL ordering. To change those today, fork the engine. Only the EVM perp engine
(OpenHL) exists; the other Reactor sandboxes are planned.

## Run the template

```sh
git clone https://github.com/psyto/rdk && cd rdk
cargo build --release -p openhl          # first build: 5-15 minutes, 5-15 GB under target/
./target/release/openhl scenario run one-update-drop --dir openhl/byo
```

The template, [`openhl/byo/one-update-drop.json`](../byo/one-update-drop.json), is the size of a real
incident: on 2026-07-27 one oracle update cut xyz:SKHX on Hyperliquid by 15.6%. The oracle starts
at 1130, three longs with different margins buy from a maker there, and at block 3 the oracle falls
to 954. The file asks a
design question: at a 3% maintenance margin, is the thinly margined long closed while the well
margined one survives?

What it printed for us (the report also has a headline, the description and next steps):

```text
TIMELINE (per-block):
  height  mark    src              trades  fills  deposits  liqs  adl  fund
  ------  ------  ---------------  ------  -----  --------  ----  ---  ----
       1    1130  oracle                1      0         4     0    —     —
       2    1130  oracle                3      3         0     0    —     —
       3     954  oracle                0      0         0     1  yes     —
       4     954  oracle                0      0         0     0    —     —
       5     954  oracle                0      0         0     0    —     —

ACCOUNT DELTA (final − initial):
  account  collateral  position  avg_entry
  -------  ----------  --------  ---------
       10        3000         5       1130
       20        1300         5       1130
       30           0         0       1130
      100       52289         0       1130
  (initial account count: 0, final account count: 4)

OUTCOMES:
  ✓ The three market buys each fill against the maker.
  ✓ The mark ends at or below the dropped oracle price.
  ✓ At least one long is liquidated by the drop.
  ✓ Not every long is liquidated: the 3,000-deposit account keeps its position.

  4 of 4 outcome(s) verified.
```

At 3% only account 30, the thinnest margin, is closed; accounts 10 and 20 keep their positions.

Every check prints ✓ or ✗. A ✗ is a finding, not a broken file: your design doesn't guarantee what
you wrote down.

Try your own margin without editing the file:

```sh
./target/release/openhl scenario run one-update-drop --dir openhl/byo --maintenance-margin-bps 1000
```

```text
TIMELINE (per-block):
  height  mark    src              trades  fills  deposits  liqs  adl  fund
  ------  ------  ---------------  ------  -----  --------  ----  ---  ----
       1    1130  oracle                1      0         4     0    —     —
       2    1130  oracle                3      3         0     0    —     —
       3     954  oracle                0      0         0     2  yes     —
       4     954  oracle                0      0         0     0    —     —
       5     954  oracle                0      0         0     0    —     —

ACCOUNT DELTA (final − initial):
  account  collateral  position  avg_entry
  -------  ----------  --------  ---------
       10        3000         5       1130
       20         349         0       1130
       30           0         0       1130
      100       52360         0       1130
  (initial account count: 0, final account count: 4)

OUTCOMES:
  ✓ The three market buys each fill against the maker.
  ✓ The mark ends at or below the dropped oracle price.
  ✓ At least one long is liquidated by the drop.
  ✓ Not every long is liquidated: the 3,000-deposit account keeps its position.

  4 of 4 outcome(s) verified.
```

At 10% account 20 is closed too (two liquidations), and the checks still hold, because they only ask that the 3,000-deposit account survive. Tighten a check and the same run would print ✗.

## Write your own

Copy the template into a folder of your own, rename `name` to match the file stem, and change three
things.

**`params`: your design.**

```json
"params": {
  "rounds": 5,
  "initial_margin_bps": 1000,
  "maintenance_margin_bps": 300,
  "liquidation_fee_bps": 150
}
```

Each can also be overridden on the command line (`--initial-margin-bps`, `--maintenance-margin-bps`,
`--liquidation-fee-bps`, `--rounds`), so one file can answer "what if we tighten to 5%?".

**`history.blocks`: the stress.** Each block may hold:

- `deposits`: `{"account": 10, "amount": 3000}`. Negative amounts withdraw.
- `trades`: `{"id": 9001, "account": 100, "side": "Sell", "qty": 20, "kind": "Limit", "price": 1130}`.
  `side` is `Buy` or `Sell`, `kind` is `Limit` (needs `price`) or `Market`. Order ids must be unique.
- `oracle`: `{"set_price": 954}` to move the index price, or `"clear"` to make it stale, so the mark
  falls back to the order book's midpoint. Set a price in block 1: with no oracle and an empty book the
  mark starts at a placeholder of 100, which would liquidate positions opened far above it.

Trades and deposits apply first, then the oracle move, then the liquidation scan for that block.

**`expected_outcomes`: what your design must guarantee.** Each entry has a `name`, a `description`
and one `check`:

| Check | Means |
|---|---|
| `{"liquidations_min": N}` / `{"liquidations_max": N}` | at least / at most N liquidations across all blocks |
| `{"adl_records_min": N}` | at least N ADL force-closes |
| `{"fills_min": N}` / `{"fills_max": N}` | at least / at most N fills |
| `{"final_account_count": N}` | exactly N accounts at the end |
| `{"final_mark_min": N}` / `{"final_mark_max": N}` | the final mark is at least / at most N |
| `{"account_collateral": {"account": 10, "expected": 2950}}` | an account's final collateral |
| `{"account_position": {"account": 10, "expected": 5}}` | an account's final position size |
| `{"account_avg_entry": {"account": 10, "expected": 1130}}` | an account's final average entry |

The sample scenarios in [`openhl/scenarios/`](../scenarios/) show each of these in use: a cascade, a
stale oracle, an ADL trigger, a threshold that moves with the margin.

## Want us to run it?

Send the market you're designing and the stress you're worried about through
[fabrknt.com's contact page](https://fabrknt.com/waitlist?product=reactor-byo). We write the
scenario, run it, and go through the result with you. If you need a setting that isn't in the file
yet, say which one: that tells us what to open up next.

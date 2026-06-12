# Sample output

This page exists so an evaluator can see exactly what `openhl scenario run` produces — without cloning the repo, building the binary, or running anything.

The output below is the canonical [`cascade`](../scenarios/cascade.json) scenario, captured verbatim from a clean checkout. The scenario is **5 traders, a multi-block liquidation cascade, ADL on the maker side, 10 outcomes verified**. The text wraps long but is 80-col friendly otherwise.

If you want to reproduce it locally:

```bash
git clone https://github.com/psyto/rdk
cd rdk/openhl
cargo run -q -p openhl -- scenario run cascade
```

Expect ~5-15 minutes for the first `cargo run` (cold compile of Reth + Malachite + the openhl workspace pulls a lot of crates and fills ~5-15 GB under `target/`); ~50 ms per subsequent `scenario run` once the binary is built. The pinned toolchain (Rust 1.95.0) auto-installs via `rust-toolchain.toml`.

---

## `cargo run -q -p openhl -- scenario run cascade`

```
─── scenario: cascade ────────────────────────────────────
HEADLINE ✓: Mark drops from 110 to 96 (mark-book settles); the scanner flags 2
underwater longs at block 2, the write-back loop closes them + their maker
counterparties via ADL, cascade resolves in one tick.

DESCRIPTION:
  Five traders open long perp positions over two blocks at entry price 110.
  At block 3 a small mark-book is placed (Buy 95 / Sell 97) — this drops the
  CLOB midpoint to 96, which is what the liquidation scanner reads as the
  mark. With longs entered at 110 and mark at 96, the maintenance margin
  breaks: at block 2 the scanner flags both underwater longs (Bob + Carol);
  the post-tick write-back loop zeroes their positions and absorbs the
  shortfall via the insurance fund + ADL (which forces the maker shorts to
  close at the haircut price). Subsequent ticks see no underwater accounts
  left to flag, so the cascade resolves in one tick.

TIMELINE (per-block):
  height  mark    src              trades  fills  deposits  liqs  adl  fund
  ------  ------  ---------------  ------  -----  --------  ----  ---  ----
       1     100  stub-empty-book       2      0         5     0    —     —
       2     100  stub-empty-book       4      4         0     2  yes     —
       3      96  clob                  2      0         0     0    —     —
       4      96  clob                  0      0         0     0    —     —
       5      96  clob                  0      0         0     0    —     —

ACCOUNT DELTA (final − initial):
  account  collateral  position  avg_entry
  -------  ----------  --------  ---------
       10         200        10        110
       20           0         0        110
       30           0         0        110
       40         300         0        110
       50         200       -20        110
  (initial account count: 0, final account count: 5)

OUTCOMES:
  ✓ Five trading accounts exist after the chain history applies
  ✓ Exactly four fills (the four market buys crossing the resting sells)
  ✓ No surprise fills beyond the four buys
  ✓ Mark settles to 96 once the block-3 mark-book lands
    (Buy 95 / Sell 97 → midpoint 96)
  ✓ The scanner flags the two underwater longs (Bob + Carol) at block 2;
    write-back closes them so subsequent ticks see nothing to flag
  ✓ No more than 2 liquidations fire across the run — write-back prevents
    re-flagging
  ✓ Alice (account 10) survives with her +10 long unchanged
    (high collateral relative to her position)
  ✓ Bob (account 20) ends with position zeroed by the liquidation close
  ✓ Carol (account 30) ends with position zeroed by the liquidation close
  ✓ Dave (account 40) — the maker who sold into the underwater longs —
    is force-closed via ADL when the longs go underwater

  10 of 10 outcome(s) verified.

NEXT:
  • Adopt this engine  : https://github.com/psyto/rdk
  • Custom build       : https://fabrknt.com/waitlist.html?product=evm-perp&intent=build
  • Hosted access      : https://fabrknt.com/waitlist.html?product=evm-perp&intent=hosted
```

## How to read this

**HEADLINE** — one-line claim, with a ✓ badge (every declared outcome passed), ⚠ badge (at least one didn't), or "unverified" (no outcomes declared). Verifies the scenario's narrative against the run's actual state before you read further.

**TIMELINE** — per-block ledger of what fired this tick. `liqs`, `adl`, `fund` count what the engine's per-block routine did. `mark` and `src` show whether the mark came from the CLOB midpoint, an installed oracle, or a stub. Read top-to-bottom to see how the cascade emerged across 5 blocks.

**ACCOUNT DELTA** — final per-account state. Surface the structural outcome (who's still in, who got zeroed, who took the ADL hit on the maker side) so you don't need to re-derive it from the timeline.

**OUTCOMES** — declarative checks against the run's actual state, declared in the scenario's JSON. Each ✓ means the scenario's claim matched reality; each ✗ would mean it didn't.

**NEXT** — three-option CTA. Whichever action fits.

---

## Other scenarios

`cascade` is the canonical demo. The 7 other scenarios in [`scenarios/`](../scenarios/) cover variations a buyer typically asks for:

| Scenario | What it shows |
|---|---|
| `single-block-cascade` | Same cascade as above compressed into one block |
| `threshold-margin` | Three traders at varying leverage; sensitive to `--maintenance-margin-bps` |
| `position-buildup` | Two market buys at different prices, demonstrates VWAP avg_entry |
| `calm-baseline` | Two balanced traders, no events — sanity check for "what does a quiet block look like" |
| `oracle-stale` | Oracle protects positions at price 110; goes stale at block 4; cascade fires when fallback to CLOB midpoint kicks in |
| `adl-trigger` | Single position + maker; oracle drops 73%; insurance fund + ADL path fires on a single underwater close |
| `bring-up` | One deposit, no trades — the EVM half of the `perp-bring-up` cross-engine compare key (pairs with `openhl-solana/bring-up`) |

Total across all 8 scenarios: **49 declared outcomes**, all verifying ✓ as of this writing.

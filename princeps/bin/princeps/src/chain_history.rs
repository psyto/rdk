// bin-crate `unreachable_pub` triggers on every public item in this
// module since `princeps` has no library surface — same pattern as
// openhl's chain_history.rs. Silence at the module level.
#![allow(unreachable_pub)]

//! Per-block event log: runtime-append + boot-time replay.
//!
//! ADR-010's [Layer 3 deferral note](../../docs/adr/010-bad-debt-depletion-policy.md)
//! names "princeps-side chain history (event replay)" as one of three
//! dependencies blocking Layer 3 implementation. This module is that
//! dependency: a per-block-indexed event log that supports BOTH
//! load-and-replay (the openhl Stage 21 shape: read JSON at boot,
//! apply events at each block) AND runtime append (Layer 3's
//! socialization declaration writes a [`ChainEvent::Socialization`]
//! at the block it fires).
//!
//! ### Why both modes
//!
//! The openhl pattern was load-only — replay a saved sequence to
//! exercise scenario tests without hand-crafting per-block state.
//! Layer 3 needs the opposite: write an event during execution that
//! audit/restart can read back. A unified store covers both —
//! file-loaded events and runtime-appended events live in the same
//! BTreeMap, serialize to the same JSON shape, and the [`apply_for_height`]
//! reader doesn't care which side produced them.
//!
//! ### Determinism contract
//!
//! For replay (file-loaded): every validator MUST load the same
//! file. Mirrors openhl's contract.
//!
//! For runtime-append: producers MUST run on every validator at the
//! same height with the same arguments. Layer 3 satisfies this
//! because operator declaration is itself a deterministic event
//! (`accept_socialization` admission lands on every validator's
//! bridge before any of them appends the [`Socialization`] event).
//!
//! [`Socialization`]: ChainEvent::Socialization

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// One event in chain history. Add new variants at the end of the
/// enum; older snapshots/files keep decoding because the serde tag
/// is the variant name string.
///
/// Uses `adjacently tagged` (`{"type": "Socialization", "data": {...}}`)
/// rather than internally-tagged because internally-tagged buffers
/// through `serde_value::Value`, which doesn't handle `u128` —
/// `unfilled` is u128 to match the ADR-010 spec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum ChainEvent {
    /// ADR-010 Layer 3 socialization event — recorded when the operator
    /// declares Layer 3 and the depositor-share haircut is applied to
    /// `market.supply_index`. Append-only audit record; the actual
    /// supply_index mutation happens elsewhere in the lending code path.
    ///
    /// `declared_by` is informational at v0 (the operator-sig admission
    /// infrastructure is the third Layer 3 blocker, still pending). When
    /// that lands, the field becomes the verified operator key / signature
    /// reference.
    Socialization {
        market_id: u32,
        unfilled: u128,
        declared_by: String,
    },
}

/// One block's worth of events. Wire shape — appears in JSON files
/// and snapshots.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryBlock {
    pub height: u64,
    /// `#[serde(default)]` so a block listing only a height (no events
    /// yet) decodes as an empty Vec. Forward-compatible with future
    /// fields that might land alongside `events`.
    #[serde(default)]
    pub events: Vec<ChainEvent>,
}

/// On-disk + in-memory wire format. Used for file loads, snapshots,
/// and the in/out of [`ChainHistoryStore::snapshot`] /
/// [`ChainHistoryStore::from_history`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChainHistory {
    pub blocks: Vec<HistoryBlock>,
}

impl ChainHistory {
    /// Number of distinct heights listed.
    #[must_use]
    pub fn total_blocks(&self) -> usize {
        self.blocks.len()
    }

    /// Heights listed, in iteration order.
    #[must_use]
    pub fn heights(&self) -> Vec<u64> {
        self.blocks.iter().map(|b| b.height).collect()
    }
}

/// Thread-safe runtime store. Wrap in `Arc<ChainHistoryStore>` so
/// per-block callbacks can clone-and-move it across threads.
#[derive(Debug, Default)]
pub struct ChainHistoryStore {
    /// Block height → events. Mutex-guarded for runtime append.
    by_height: Mutex<BTreeMap<u64, Vec<ChainEvent>>>,
    /// Heights already read out via [`apply_for_height`]. Defensive
    /// idempotency guard for boot-time replay — consensus normally
    /// fires each height once, but on a restart-resume edge case it
    /// might re-fire a height that's already been applied. Append
    /// from runtime is unaffected (writes go directly to `by_height`).
    applied: Mutex<BTreeSet<u64>>,
}

impl ChainHistoryStore {
    /// Empty store. Used at boot when no `--chain-history` file is
    /// supplied. Runtime appends still work.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Construct from a loaded [`ChainHistory`]. Used at boot when
    /// loading from a file OR when restoring from a snapshot.
    /// Rejects duplicate heights — the JSON shouldn't list block 7
    /// twice, mirroring openhl's invariant.
    pub fn from_history(history: ChainHistory) -> eyre::Result<Self> {
        let mut by_height: BTreeMap<u64, Vec<ChainEvent>> = BTreeMap::new();
        for b in history.blocks {
            if by_height.contains_key(&b.height) {
                return Err(eyre::eyre!(
                    "chain history: duplicate block height entry",
                ));
            }
            by_height.insert(b.height, b.events);
        }
        Ok(Self {
            by_height: Mutex::new(by_height),
            applied: Mutex::new(BTreeSet::new()),
        })
    }

    /// Append one event at `height` (runtime write path).
    ///
    /// Idempotent on the applier side — even if `height` has already
    /// been drained via [`apply_for_height`], the appended event
    /// lands in `by_height` and is persisted via the next snapshot
    /// (so audit / restart-replay see it). Future calls to
    /// `apply_for_height(same_height)` still return `None` — the
    /// applied-set is one-way; we only re-run a height if the
    /// applied-set is cleared (e.g., via [`forget_applied`]).
    pub fn append_event(&self, height: u64, event: ChainEvent) {
        let mut by_height = self
            .by_height
            .lock()
            .expect("chain history by_height mutex poisoned");
        by_height.entry(height).or_default().push(event);
    }

    /// Read out (and mark applied) the events at `height`. Returns
    /// `Some(events)` the first time `height` is asked for, `None`
    /// thereafter or if no events were ever listed at `height`.
    ///
    /// The boot-time replay loop calls this once per block height as
    /// consensus delivers blocks; the rest of the per-block tick
    /// then runs against any state mutations these events implied
    /// (Layer 3 is the first user — when it lands the dispatcher
    /// here will route `Socialization` to a `market.supply_index`
    /// haircut).
    pub fn apply_for_height(&self, height: u64) -> Option<Vec<ChainEvent>> {
        {
            let mut applied = self
                .applied
                .lock()
                .expect("chain history applied mutex poisoned");
            if applied.contains(&height) {
                return None;
            }
            applied.insert(height);
        }
        let by_height = self
            .by_height
            .lock()
            .expect("chain history by_height mutex poisoned");
        by_height.get(&height).cloned()
    }

    /// Peek at events for `height` without marking applied. For
    /// audit / RPC paths that want to read without consuming.
    #[must_use]
    pub fn peek_at_height(&self, height: u64) -> Option<Vec<ChainEvent>> {
        let by_height = self
            .by_height
            .lock()
            .expect("chain history by_height mutex poisoned");
        by_height.get(&height).cloned()
    }

    /// Clear the applied-set. Lets a subsequent
    /// [`apply_for_height`] re-fire heights that were previously
    /// drained. Used by restart-resume paths that want to re-deliver
    /// recent events without re-loading from disk.
    pub fn forget_applied(&self) {
        self.applied
            .lock()
            .expect("chain history applied mutex poisoned")
            .clear();
    }

    /// Total distinct heights with at least one event.
    #[must_use]
    pub fn total_blocks(&self) -> usize {
        self.by_height
            .lock()
            .expect("chain history by_height mutex poisoned")
            .len()
    }

    /// Heights with at least one event, ascending.
    #[must_use]
    pub fn heights(&self) -> Vec<u64> {
        self.by_height
            .lock()
            .expect("chain history by_height mutex poisoned")
            .keys()
            .copied()
            .collect()
    }

    /// Snapshot the current state (for persistence + restart-resume).
    /// Does NOT include the applied-set — restart starts with all
    /// heights eligible for re-replay; producers decide whether to
    /// also call [`forget_applied`] depending on whether they want
    /// the bridge to re-apply post-snapshot events.
    #[must_use]
    pub fn snapshot(&self) -> ChainHistory {
        let by_height = self
            .by_height
            .lock()
            .expect("chain history by_height mutex poisoned");
        let blocks: Vec<HistoryBlock> = by_height
            .iter()
            .map(|(&height, events)| HistoryBlock {
                height,
                events: events.clone(),
            })
            .collect();
        ChainHistory { blocks }
    }
}

/// Parse a chain-history file from a path. For boot-time
/// load-and-replay; mirrors openhl's `load_from_path`.
pub fn load_from_path(path: &Path) -> eyre::Result<ChainHistory> {
    let bytes = std::fs::read(path)
        .map_err(|e| eyre::eyre!("chain history {}: {e}", path.display()))?;
    let history: ChainHistory = serde_json::from_slice(&bytes)
        .map_err(|e| eyre::eyre!("chain history {} parse: {e}", path.display()))?;
    Ok(history)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── ChainEvent serde ──────────────────────────────────────────

    #[test]
    fn chain_event_socialization_serde_round_trip() {
        let evt = ChainEvent::Socialization {
            market_id: 0,
            unfilled: 1_000_000,
            declared_by: "operator-A".to_string(),
        };
        let json = serde_json::to_string(&evt).expect("serialize");
        let decoded: ChainEvent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(evt, decoded);
        // Sanity: variant name is in the wire tag.
        assert!(json.contains("Socialization"));
    }

    // ─── ChainHistory wire shape ───────────────────────────────────

    #[test]
    fn chain_history_round_trips_with_socialization_event() {
        let json = r#"{
            "blocks": [
                {
                    "height": 7,
                    "events": [
                        {"type": "Socialization",
                         "data": {"market_id": 0, "unfilled": 5000,
                                  "declared_by": "operator-A"}}
                    ]
                },
                {"height": 9}
            ]
        }"#;
        let h: ChainHistory = serde_json::from_str(json).expect("parse");
        assert_eq!(h.blocks.len(), 2);
        assert_eq!(h.blocks[0].height, 7);
        assert_eq!(h.blocks[0].events.len(), 1);
        // The second block has no `events` field — serde(default) decodes empty.
        assert_eq!(h.blocks[1].height, 9);
        assert!(h.blocks[1].events.is_empty());
    }

    // ─── ChainHistoryStore: construction + invariants ──────────────

    #[test]
    fn store_from_history_rejects_duplicate_heights() {
        let history = ChainHistory {
            blocks: vec![
                HistoryBlock { height: 5, events: vec![] },
                HistoryBlock { height: 5, events: vec![] },
            ],
        };
        let err = ChainHistoryStore::from_history(history).expect_err("dup must fail");
        assert!(
            err.to_string().contains("duplicate block height"),
            "unexpected error: {err}",
        );
    }

    #[test]
    fn empty_store_is_empty() {
        let store = ChainHistoryStore::empty();
        assert_eq!(store.total_blocks(), 0);
        assert!(store.heights().is_empty());
        assert!(store.apply_for_height(1).is_none());
    }

    // ─── Apply path: idempotency, mismatch, no-entry ───────────────

    #[test]
    fn apply_for_height_returns_events_then_is_idempotent() {
        let store = ChainHistoryStore::from_history(ChainHistory {
            blocks: vec![HistoryBlock {
                height: 3,
                events: vec![ChainEvent::Socialization {
                    market_id: 0,
                    unfilled: 500,
                    declared_by: "operator-A".to_string(),
                }],
            }],
        })
        .expect("construct");

        // Heights 1, 2 → no entry → None.
        assert!(store.apply_for_height(1).is_none());
        assert!(store.apply_for_height(2).is_none());

        // Height 3 → entry → applies.
        let events = store.apply_for_height(3).expect("entry at height 3");
        assert_eq!(events.len(), 1);

        // Second time at height 3 → no-op (idempotent).
        assert!(store.apply_for_height(3).is_none());

        // Height past the history → no-op.
        assert!(store.apply_for_height(4).is_none());
    }

    // ─── Append path: runtime write + peek + later apply ───────────

    #[test]
    fn append_event_then_peek_returns_event_without_marking_applied() {
        let store = ChainHistoryStore::empty();
        let evt = ChainEvent::Socialization {
            market_id: 1,
            unfilled: 42,
            declared_by: "operator-B".to_string(),
        };
        store.append_event(10, evt.clone());

        // peek doesn't mark applied — apply still returns the event.
        let peeked = store.peek_at_height(10).expect("peek finds it");
        assert_eq!(peeked.len(), 1);
        let applied = store.apply_for_height(10).expect("apply finds it");
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0], evt);
        // ...but the second apply is None.
        assert!(store.apply_for_height(10).is_none());
    }

    #[test]
    fn append_event_after_apply_is_visible_via_peek_and_snapshot() {
        let store = ChainHistoryStore::empty();
        store.append_event(
            5,
            ChainEvent::Socialization {
                market_id: 0,
                unfilled: 100,
                declared_by: "op".to_string(),
            },
        );
        // Drain height 5.
        let _ = store.apply_for_height(5).expect("first apply");
        // Append another event at the same height.
        store.append_event(
            5,
            ChainEvent::Socialization {
                market_id: 0,
                unfilled: 200,
                declared_by: "op".to_string(),
            },
        );
        // The applier remembers it ran — no re-apply.
        assert!(store.apply_for_height(5).is_none());
        // But peek + snapshot see both events.
        let peeked = store.peek_at_height(5).expect("peek");
        assert_eq!(peeked.len(), 2);
        let snap = store.snapshot();
        assert_eq!(snap.blocks.len(), 1);
        assert_eq!(snap.blocks[0].events.len(), 2);
    }

    #[test]
    fn forget_applied_lets_height_be_replayed() {
        let store = ChainHistoryStore::empty();
        store.append_event(
            8,
            ChainEvent::Socialization {
                market_id: 0,
                unfilled: 1,
                declared_by: "op".to_string(),
            },
        );
        assert!(store.apply_for_height(8).is_some());
        assert!(store.apply_for_height(8).is_none()); // applied
        store.forget_applied();
        // After forgetting, the same height fires again.
        let again = store.apply_for_height(8).expect("re-applies post-forget");
        assert_eq!(again.len(), 1);
    }

    // ─── Snapshot / restore round-trip ─────────────────────────────

    #[test]
    fn snapshot_then_restore_preserves_state() {
        let store = ChainHistoryStore::empty();
        store.append_event(
            5,
            ChainEvent::Socialization {
                market_id: 0,
                unfilled: 111,
                declared_by: "op-1".to_string(),
            },
        );
        store.append_event(
            7,
            ChainEvent::Socialization {
                market_id: 1,
                unfilled: 222,
                declared_by: "op-2".to_string(),
            },
        );
        store.append_event(
            7,
            ChainEvent::Socialization {
                market_id: 1,
                unfilled: 333,
                declared_by: "op-3".to_string(),
            },
        );

        let snap = store.snapshot();
        assert_eq!(snap.total_blocks(), 2);
        // JSON round-trip is the on-disk shape.
        let bytes = serde_json::to_vec(&snap).expect("ser");
        let decoded: ChainHistory = serde_json::from_slice(&bytes).expect("de");
        let restored = ChainHistoryStore::from_history(decoded).expect("restore");
        assert_eq!(restored.total_blocks(), 2);
        let evs_at_5 = restored.peek_at_height(5).expect("h=5 present");
        assert_eq!(evs_at_5.len(), 1);
        let evs_at_7 = restored.peek_at_height(7).expect("h=7 present");
        assert_eq!(evs_at_7.len(), 2);
    }

    #[test]
    fn snapshot_drops_applied_set_so_restored_store_replays() {
        // Snapshot of an apply does NOT carry the applied-set; restored
        // store re-fires events. Documented contract.
        let store = ChainHistoryStore::empty();
        store.append_event(
            3,
            ChainEvent::Socialization {
                market_id: 0,
                unfilled: 1,
                declared_by: "op".to_string(),
            },
        );
        // Apply once on the original.
        let _ = store.apply_for_height(3).expect("first apply");
        assert!(store.apply_for_height(3).is_none());
        // Snapshot then restore.
        let snap = store.snapshot();
        let restored = ChainHistoryStore::from_history(snap).expect("restore");
        // Restored store fires the event again.
        let applied = restored.apply_for_height(3).expect("restored re-fires");
        assert_eq!(applied.len(), 1);
    }
}

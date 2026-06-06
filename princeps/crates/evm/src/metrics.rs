//! Stage T2b-b of `docs/plans/v0-testnet-deploy.md` — bridge-side
//! gauges for the testnet observability surface (TD-006).
//!
//! Adds three gauges that reflect the size of the bridge-owned state
//! the per-block tick reads from and writes to:
//!
//! - `princeps_lending_markets` — number of registered lending markets
//! - `princeps_lending_positions` — number of `(account, market)` positions
//! - `princeps_accounts` — number of perp accounts on the bridge
//!
//! Recorded from `bin/princeps`'s per-block hook (after `node.tick`),
//! so the gauges land one observation per committed block. Empty
//! markets/positions read as 0 — useful baseline before the genesis
//! seed runs and after a fresh bridge boots.
//!
//! Companion to [`princeps_node::metrics`] (T2a): princeps-node owns
//! TickReport-derivable metrics, princeps-evm owns bridge-state-
//! derivable metrics. The two register independently at
//! [`init_observability`][bin-princeps-observability] boot time.
//!
//! [bin-princeps-observability]: ../../bin/princeps/src/observability.rs
//!
//! ## Why a `record_bridge_counts` inner function
//!
//! [`record_bridge_state`] takes a `&LiveRethEvmBridge<P>` — needs a
//! constructed bridge, which in tests means dragging in
//! `dev_chain_spec` and the surrounding reth scaffolding. Splitting
//! out [`record_bridge_counts`] lets the metric pipeline (name
//! lookups, type cast, emit) be unit-tested independently with just
//! three `usize` inputs. The bridge-bound wrapper is one line per
//! gauge and gets exercised by the binary in T5 chaos drills.

use crate::LiveRethEvmBridge;

/// Gauge: count of registered lending markets in the bridge's
/// `markets_snapshot()`. v0 ships with 1 (USDC/ETH); v1+ adds more.
pub const PRINCEPS_LENDING_MARKETS: &str = "princeps_lending_markets";

/// Gauge: count of `(account, market)` lending positions in the
/// bridge's `positions_snapshot()`. Sum across all markets.
pub const PRINCEPS_LENDING_POSITIONS: &str = "princeps_lending_positions";

/// Gauge: count of perp accounts in the bridge's
/// `accounts_snapshot()`. Includes flat accounts (zero position) —
/// matches `seed_v0_demo_accounts`'s post-seed count.
pub const PRINCEPS_ACCOUNTS: &str = "princeps_accounts";

/// Histogram: wall-clock seconds spent inside
/// `LiveRethEvmBridge::scan_unified` per per-block invocation.
/// Sourced from `Instant::now()` brackets in the binary's per-block
/// hook. p99 of this histogram is the leading indicator for the
/// scan keeping up with block time — if p99 approaches block time
/// the bridge is one fault away from missing a block. Alert at
/// p99 > 250ms on a 1s-block testnet.
pub const PRINCEPS_SCAN_DURATION_SECONDS: &str = "princeps_scan_duration_seconds";

/// One-shot metadata registration. Called from
/// `bin/princeps::observability::init_observability` alongside
/// [`princeps_node::metrics::describe_metrics`] at boot.
pub fn describe_metrics() {
    metrics::describe_gauge!(
        PRINCEPS_LENDING_MARKETS,
        "Number of registered lending markets on the bridge"
    );
    metrics::describe_gauge!(
        PRINCEPS_LENDING_POSITIONS,
        "Number of (account, market) lending positions on the bridge"
    );
    metrics::describe_gauge!(
        PRINCEPS_ACCOUNTS,
        "Number of perp accounts on the bridge"
    );
    metrics::describe_histogram!(
        PRINCEPS_SCAN_DURATION_SECONDS,
        metrics::Unit::Seconds,
        "Wall-clock duration of LiveRethEvmBridge::scan_unified per per-block invocation"
    );
}

/// Snapshot the bridge's state-counts and emit them as gauges.
/// Called from `bin/princeps`'s per-block hook after `node.tick`,
/// so each gauge advances one observation per committed block.
///
/// Best-effort: no-op if no recorder is installed.
pub fn record_bridge_state<P>(bridge: &LiveRethEvmBridge<P>) {
    record_bridge_counts(
        bridge.markets_snapshot().len(),
        bridge.positions_snapshot().len(),
        bridge.accounts_snapshot().len(),
    );
}

/// Pure-counts variant. Same emission semantics as
/// [`record_bridge_state`] but takes the counts directly so unit
/// tests can validate the metric pipeline without standing up a
/// bridge.
#[allow(clippy::cast_precision_loss)] // monitoring is best-effort
pub(crate) fn record_bridge_counts(markets: usize, positions: usize, accounts: usize) {
    metrics::gauge!(PRINCEPS_LENDING_MARKETS).set(markets as f64);
    metrics::gauge!(PRINCEPS_LENDING_POSITIONS).set(positions as f64);
    metrics::gauge!(PRINCEPS_ACCOUNTS).set(accounts as f64);
}

/// Record one observation of `LiveRethEvmBridge::scan_unified`
/// wall-clock duration. Called from the binary's per-block hook
/// with `Instant::now()` brackets around the scan call.
///
/// Best-effort: no-op if no recorder is installed.
pub fn record_scan_duration(elapsed: std::time::Duration) {
    metrics::histogram!(PRINCEPS_SCAN_DURATION_SECONDS).record(elapsed.as_secs_f64());
}

#[cfg(test)]
mod tests {
    use super::*;
    use metrics_util::debugging::{DebugValue, DebuggingRecorder, Snapshotter};

    fn snapshot_map(s: &Snapshotter) -> std::collections::HashMap<String, DebugValue> {
        s.snapshot()
            .into_vec()
            .into_iter()
            .map(|(ckey, _unit, _desc, value)| (ckey.key().name().to_string(), value))
            .collect()
    }

    /// Pin: each gauge appears in the snapshot with the value
    /// passed in. Catches name typos and any gauge we forget to
    /// emit. Numeric fidelity matters here — alert rules that
    /// compare `princeps_lending_markets >= N` need to see N.
    #[test]
    fn record_bridge_counts_emits_each_gauge() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _g = ::metrics::set_default_local_recorder(&recorder);

        record_bridge_counts(2, 5, 3);

        let map = snapshot_map(&snapshotter);
        assert_eq!(
            map.get(PRINCEPS_LENDING_MARKETS),
            Some(&DebugValue::Gauge(2.0.into()))
        );
        assert_eq!(
            map.get(PRINCEPS_LENDING_POSITIONS),
            Some(&DebugValue::Gauge(5.0.into()))
        );
        assert_eq!(
            map.get(PRINCEPS_ACCOUNTS),
            Some(&DebugValue::Gauge(3.0.into()))
        );
    }

    /// Empty bridge → all-zero gauges. The natural baseline pre-
    /// seed and after a fresh boot.
    #[test]
    fn record_bridge_counts_zero_emits_zeros() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _g = ::metrics::set_default_local_recorder(&recorder);

        record_bridge_counts(0, 0, 0);

        let map = snapshot_map(&snapshotter);
        assert_eq!(
            map.get(PRINCEPS_LENDING_MARKETS),
            Some(&DebugValue::Gauge(0.0.into()))
        );
        assert_eq!(
            map.get(PRINCEPS_LENDING_POSITIONS),
            Some(&DebugValue::Gauge(0.0.into()))
        );
        assert_eq!(
            map.get(PRINCEPS_ACCOUNTS),
            Some(&DebugValue::Gauge(0.0.into()))
        );
    }

    /// Idempotency smoke test on describe.
    #[test]
    fn describe_metrics_is_callable() {
        let recorder = DebuggingRecorder::new();
        let _g = ::metrics::set_default_local_recorder(&recorder);
        describe_metrics();
        describe_metrics();
    }

    /// T2b-d: each call to `record_scan_duration` appends one
    /// observation to the histogram in seconds (Duration → f64
    /// via `as_secs_f64`). The keystone property: two successive
    /// calls must show up as TWO observations, not get aggregated
    /// or last-wins-overwritten. Catches any future regression
    /// where `record_scan_duration` accidentally uses `gauge!` or
    /// `counter!` instead of `histogram!`.
    #[test]
    fn record_scan_duration_appends_observations() {
        use std::time::Duration;
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _g = ::metrics::set_default_local_recorder(&recorder);

        record_scan_duration(Duration::from_millis(5));
        record_scan_duration(Duration::from_millis(12));

        let map = snapshot_map(&snapshotter);
        match map.get(PRINCEPS_SCAN_DURATION_SECONDS) {
            Some(DebugValue::Histogram(values)) => {
                let secs: Vec<f64> = values.iter().map(|v| v.0).collect();
                assert_eq!(secs.len(), 2, "expected two observations, got {secs:?}");
                // Order may be implementation-defined; sort before comparing.
                let mut sorted = secs.clone();
                sorted.sort_by(f64::total_cmp);
                assert!(
                    (sorted[0] - 0.005).abs() < 1e-9,
                    "first observation should be 5ms, got {sorted:?}"
                );
                assert!(
                    (sorted[1] - 0.012).abs() < 1e-9,
                    "second observation should be 12ms, got {sorted:?}"
                );
            }
            other => panic!(
                "{PRINCEPS_SCAN_DURATION_SECONDS} should be a Histogram, got {other:?}"
            ),
        }
    }
}

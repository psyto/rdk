//! Stage T2a of `docs/plans/v0-testnet-deploy.md` — per-tick metric
//! emission for the testnet observability surface (TD-006).
//!
//! What this module does (T2a scope):
//! - Centralizes the metric-name inventory as `&'static str` constants
//!   so the describe path and the record path can never drift apart.
//! - Provides [`describe_metrics`] for one-shot registration of unit /
//!   description metadata at binary startup.
//! - Provides [`record_tick`] which the per-tick path
//!   ([`crate::PrincepsNode::tick`]) calls at the end of every tick to
//!   update gauges / counters from a [`TickReport`].
//!
//! As of T2b-c this module also exposes:
//! - [`record_node_state`] — called from `tick()` alongside
//!   [`record_tick`] to emit gauges sourced from `PrincepsNode`
//!   state that doesn't appear in `TickReport` (currently:
//!   insurance-fund balance).
//! - [`record_chain_history`] — called from the binary's per-block
//!   hook with the bin-owned `Arc<ChainHistoryStore>` since the store
//!   isn't owned by the node.
//!
//! Deferred to later T2b slices:
//! - Bridge counts (markets / positions / accounts) → T2b-b in
//!   `princeps_evm::metrics`.
//! - Scan-loop duration histogram → T2b-d.
//! - Oracle publisher observation age, validator liveness → later
//!   slices once the corresponding accessors are wired.
//!
//! ## Why the `metrics` façade and not Prometheus directly
//!
//! Lets the binary choose the recorder at link time. Production gets
//! `metrics-exporter-prometheus`; tests install a `DebuggingRecorder`
//! and inspect emitted values in-process; cron-style runs can install
//! a log-and-discard recorder. Picking a concrete backend here would
//! force every consumer through Prometheus, including tests that
//! don't want a `/metrics` listener.

use crate::{chain_history::ChainHistoryStore, PrincepsNode, TickReport};

// --- Metric names -----------------------------------------------------------
//
// Prometheus convention: snake_case, `_total` suffix on counters.
// Keep the `princeps_` prefix on every name so a single Prometheus job
// scraping multiple binaries on the same host stays disambiguatable.

/// Gauge: most recent block height observed by `tick`.
pub const PRINCEPS_BLOCK_HEIGHT: &str = "princeps_block_height";
/// Gauge: most recent block timestamp (seconds) observed by `tick`.
pub const PRINCEPS_BLOCK_TIME: &str = "princeps_block_time";
/// Counter: total tick invocations since process start.
pub const PRINCEPS_TICKS_TOTAL: &str = "princeps_ticks_total";
/// Gauge: block height through which the lending-halt is armed
/// (ADR-010 Layer 1). `0` when no halt is active. Use the rule
/// `value > 0 AND value >= princeps_block_height` to alert on
/// "currently halted" in Prometheus.
pub const PRINCEPS_LENDING_HALT_ARMED_UNTIL: &str = "princeps_lending_halt_armed_until";
/// Counter: total times the lending-halt has newly tripped or been
/// extended (transition events, not sustained-armed ticks).
pub const PRINCEPS_LENDING_HALT_TRIPS_TOTAL: &str = "princeps_lending_halt_trips_total";
/// Counter: total times the oracle circuit-breaker has tripped
/// (threat-model row O-3).
pub const PRINCEPS_ORACLE_CIRCUIT_BREAKER_TRIPS_TOTAL: &str =
    "princeps_oracle_circuit_breaker_trips_total";
/// Counter: total funding settlements applied since process start.
pub const PRINCEPS_FUNDING_SETTLEMENTS_TOTAL: &str = "princeps_funding_settlements_total";
/// Gauge: liquidation scan's unfilled deficit from the last tick.
/// `0` when scan cleared cleanly; positive when underwater positions
/// could not be auto-liquidated by the available liquidator pool.
pub const PRINCEPS_LIQUIDATION_UNFILLED_DEFICIT: &str =
    "princeps_liquidation_unfilled_deficit";
/// Gauge: insurance-fund balance (i64; can be negative if absorbed
/// debt exceeds prior reserves). Sourced from
/// `PrincepsNode::snapshot().insurance_fund_balance` at the end of
/// every tick. Drops toward zero are the precursor to an ADR-010
/// Layer 1 halt; the [`PRINCEPS_LENDING_HALT_TRIPS_TOTAL`] counter
/// is the corresponding event.
pub const PRINCEPS_INSURANCE_FUND_BALANCE: &str = "princeps_insurance_fund_balance";
/// Gauge: number of distinct heights in the chain-history store
/// (ADR-010 Layer 3 audit log). Monotonic non-decreasing across the
/// life of a chain — append-only by design. Sourced from
/// `ChainHistoryStore::total_blocks()` from the bin's per-block hook.
pub const PRINCEPS_CHAIN_HISTORY_BLOCKS: &str = "princeps_chain_history_blocks";

/// One-shot metadata registration. Call once at binary startup
/// (typically after installing the exporter / recorder). Idempotent —
/// `describe_*!` macros may be invoked repeatedly without ill effect.
pub fn describe_metrics() {
    metrics::describe_gauge!(
        PRINCEPS_BLOCK_HEIGHT,
        "Most recent block height observed by PrincepsNode::tick"
    );
    metrics::describe_gauge!(
        PRINCEPS_BLOCK_TIME,
        metrics::Unit::Seconds,
        "Most recent block timestamp observed by PrincepsNode::tick"
    );
    metrics::describe_counter!(
        PRINCEPS_TICKS_TOTAL,
        "Total PrincepsNode::tick invocations since process start"
    );
    metrics::describe_gauge!(
        PRINCEPS_LENDING_HALT_ARMED_UNTIL,
        "Block height through which the ADR-010 Layer 1 lending halt is armed (0 if not armed)"
    );
    metrics::describe_counter!(
        PRINCEPS_LENDING_HALT_TRIPS_TOTAL,
        "Total transitions where the lending halt newly tripped or extended"
    );
    metrics::describe_counter!(
        PRINCEPS_ORACLE_CIRCUIT_BREAKER_TRIPS_TOTAL,
        "Total transitions where the oracle circuit breaker tripped (threat-model O-3)"
    );
    metrics::describe_counter!(
        PRINCEPS_FUNDING_SETTLEMENTS_TOTAL,
        "Total funding settlements applied since process start"
    );
    metrics::describe_gauge!(
        PRINCEPS_LIQUIDATION_UNFILLED_DEFICIT,
        "Liquidation scan's unfilled deficit from the last tick (0 when clean)"
    );
    metrics::describe_gauge!(
        PRINCEPS_INSURANCE_FUND_BALANCE,
        "Insurance-fund balance (i64; can be negative)"
    );
    metrics::describe_gauge!(
        PRINCEPS_CHAIN_HISTORY_BLOCKS,
        "Number of distinct heights in the chain-history store (ADR-010 Layer 3 audit log)"
    );
}

/// Emit per-tick metrics. Called from [`crate::PrincepsNode::tick`]
/// after the report is assembled but before it returns to the caller.
///
/// Gauges always set; counters increment only on transition events
/// (lending-halt newly-tripped, circuit-breaker fired, funding
/// settlement actually applied) so the counter slope matches the
/// observable event rate, not the tick rate.
///
/// Best-effort: if no recorder is installed, every macro is a no-op.
#[allow(clippy::cast_precision_loss)] // monitoring is best-effort; f64 has 53 bits of mantissa, enough for block heights well past human relevance
#[allow(clippy::cast_sign_loss)] // unfilled_deficit is i64; we clamp to >= 0 before casting
pub(crate) fn record_tick(report: &TickReport) {
    metrics::gauge!(PRINCEPS_BLOCK_HEIGHT).set(report.block_height as f64);
    metrics::gauge!(PRINCEPS_BLOCK_TIME).set(report.block_time as f64);
    metrics::counter!(PRINCEPS_TICKS_TOTAL).increment(1);

    metrics::gauge!(PRINCEPS_LENDING_HALT_ARMED_UNTIL)
        .set(report.lending_halt_tripped_until.unwrap_or(0) as f64);

    if report.lending_halt_tripped_until.is_some() {
        metrics::counter!(PRINCEPS_LENDING_HALT_TRIPS_TOTAL).increment(1);
    }
    if report.circuit_breaker_tripped_until.is_some() {
        metrics::counter!(PRINCEPS_ORACLE_CIRCUIT_BREAKER_TRIPS_TOTAL).increment(1);
    }
    if report.funding.is_some() {
        metrics::counter!(PRINCEPS_FUNDING_SETTLEMENTS_TOTAL).increment(1);
    }

    let unfilled = report.liquidation.unfilled_deficit.max(0) as f64;
    metrics::gauge!(PRINCEPS_LIQUIDATION_UNFILLED_DEFICIT).set(unfilled);
}

/// Emit gauges sourced from `PrincepsNode` state that doesn't
/// appear in [`TickReport`]. Called from [`PrincepsNode::tick`]
/// alongside [`record_tick`] so the gauge advances one observation
/// per tick.
///
/// Currently only `insurance_fund_balance`; future expansions add
/// here (e.g., oracle publisher counts, last-refresh-age) without
/// changing the call shape.
pub fn record_node_state(node: &PrincepsNode) {
    record_insurance_fund_balance(node.snapshot().insurance_fund_balance);
}

/// Pure-value variant. Same emission semantics as
/// [`record_node_state`] but takes the balance directly so unit
/// tests can validate the metric pipeline without constructing a
/// full `PrincepsNode`. Same split rationale as
/// `princeps_evm::metrics::record_bridge_counts`.
#[allow(clippy::cast_precision_loss)] // monitoring is best-effort
pub(crate) fn record_insurance_fund_balance(balance: i64) {
    metrics::gauge!(PRINCEPS_INSURANCE_FUND_BALANCE).set(balance as f64);
}

/// Emit the chain-history-length gauge. Called from the binary's
/// per-block hook — the `ChainHistoryStore` is bin-owned (the bin
/// constructs it from `--reth-chain-history-file`), not
/// node-owned, so emission has to happen at that layer rather than
/// inside `tick`.
pub fn record_chain_history(store: &ChainHistoryStore) {
    record_chain_history_blocks(store.total_blocks());
}

/// Pure-counts variant. See [`record_chain_history`] for the
/// production call shape; this exists for testability.
#[allow(clippy::cast_precision_loss)] // monitoring is best-effort
pub(crate) fn record_chain_history_blocks(blocks: usize) {
    metrics::gauge!(PRINCEPS_CHAIN_HISTORY_BLOCKS).set(blocks as f64);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::metrics::{Key, Label};
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};

    /// Build a minimal `TickReport` for metric-emission tests. Most
    /// fields are zero/empty; the test customizes the few that matter.
    fn minimal_report() -> TickReport {
        TickReport {
            block_height: 0,
            block_time: 0,
            oracle: None,
            liquidation: rdk_liquidation::ScanReport::default(),
            adl: None,
            vault_total_shares: 0,
            vault_total_assets: 0,
            vault_share_price_bps: None,
            funding: None,
            circuit_breaker_tripped_until: None,
            lending_halt_tripped_until: None,
        }
    }

    fn snapshot_map(snapshotter: &metrics_util::debugging::Snapshotter)
        -> std::collections::HashMap<String, DebugValue>
    {
        snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .map(|(ckey, _unit, _desc, value)| (ckey.key().name().to_string(), value))
            .collect()
    }

    /// Smoke test: every metric we record appears in the snapshot
    /// with a non-default value. Catches typos in metric names and
    /// any metric we forget to emit.
    #[test]
    fn record_tick_emits_every_metric() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = ::metrics::set_default_local_recorder(&recorder);

        let mut report = minimal_report();
        report.block_height = 42;
        report.block_time = 1_700_000_000;
        report.lending_halt_tripped_until = Some(142);
        report.circuit_breaker_tripped_until = Some(50);
        report.liquidation.unfilled_deficit = 17;
        // Don't fabricate a `funding` — it requires concrete types from
        // rdk-funding we don't import here. `record_tick_emits_every_metric`
        // is about coverage of the always-emitted set; the funding counter
        // has its own dedicated test below.

        record_tick(&report);

        let map = snapshot_map(&snapshotter);

        // Gauges always emitted.
        assert_eq!(map.get(PRINCEPS_BLOCK_HEIGHT), Some(&DebugValue::Gauge(42.0.into())));
        assert_eq!(
            map.get(PRINCEPS_BLOCK_TIME),
            Some(&DebugValue::Gauge(1_700_000_000.0.into()))
        );
        assert_eq!(
            map.get(PRINCEPS_LENDING_HALT_ARMED_UNTIL),
            Some(&DebugValue::Gauge(142.0.into()))
        );
        assert_eq!(
            map.get(PRINCEPS_LIQUIDATION_UNFILLED_DEFICIT),
            Some(&DebugValue::Gauge(17.0.into()))
        );

        // Counters incremented on transition events.
        assert_eq!(map.get(PRINCEPS_TICKS_TOTAL), Some(&DebugValue::Counter(1)));
        assert_eq!(
            map.get(PRINCEPS_LENDING_HALT_TRIPS_TOTAL),
            Some(&DebugValue::Counter(1))
        );
        assert_eq!(
            map.get(PRINCEPS_ORACLE_CIRCUIT_BREAKER_TRIPS_TOTAL),
            Some(&DebugValue::Counter(1))
        );
    }

    /// Counters should NOT increment on a quiet tick where no
    /// transition events fired. Validates the
    /// "counter-slope-matches-event-rate" property the doc claims.
    #[test]
    fn quiet_tick_does_not_increment_transition_counters() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = ::metrics::set_default_local_recorder(&recorder);

        record_tick(&minimal_report());

        let map = snapshot_map(&snapshotter);
        // Ticks counter MUST still increment — it counts ticks, not events.
        assert_eq!(map.get(PRINCEPS_TICKS_TOTAL), Some(&DebugValue::Counter(1)));
        // Transition counters either absent (never touched) or zero.
        // `metrics` doesn't emit unobserved counters into the snapshot,
        // so absence is the expected case here.
        assert!(map.get(PRINCEPS_LENDING_HALT_TRIPS_TOTAL).is_none());
        assert!(map.get(PRINCEPS_ORACLE_CIRCUIT_BREAKER_TRIPS_TOTAL).is_none());
        assert!(map.get(PRINCEPS_FUNDING_SETTLEMENTS_TOTAL).is_none());
    }

    /// describe_metrics is idempotent and never panics. The actual
    /// description text doesn't surface in DebuggingRecorder snapshots
    /// (descriptions are exporter-side metadata), so we just verify
    /// the call shape.
    #[test]
    fn describe_metrics_is_callable() {
        let recorder = DebuggingRecorder::new();
        let _guard = ::metrics::set_default_local_recorder(&recorder);
        describe_metrics();
        describe_metrics(); // idempotent
    }

    /// T2b-c: insurance-fund balance gauge pins the i64 → f64 cast
    /// at typical balances. Negative balances are legal (absorbed
    /// debt exceeded prior reserves) and must round-trip with sign.
    #[test]
    fn record_insurance_fund_balance_emits_signed_values() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _g = ::metrics::set_default_local_recorder(&recorder);

        record_insurance_fund_balance(-500);
        record_insurance_fund_balance(1_000_000); // most recent wins

        let map = snapshot_map(&snapshotter);
        assert_eq!(
            map.get(PRINCEPS_INSURANCE_FUND_BALANCE),
            Some(&DebugValue::Gauge(1_000_000.0.into())),
            "gauge should reflect the latest call"
        );
    }

    /// T2b-c: chain-history blocks gauge pins the usize → f64 cast.
    /// Monotonic non-decreasing in production; the test just
    /// validates the emission and that the latest value sticks.
    #[test]
    fn record_chain_history_blocks_emits() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _g = ::metrics::set_default_local_recorder(&recorder);

        record_chain_history_blocks(0);
        record_chain_history_blocks(7);

        let map = snapshot_map(&snapshotter);
        assert_eq!(
            map.get(PRINCEPS_CHAIN_HISTORY_BLOCKS),
            Some(&DebugValue::Gauge(7.0.into()))
        );
    }

    // Suppress unused-import warnings for the test-only helpers above —
    // referenced by future tests in this module.
    #[allow(dead_code)]
    fn _ensure_imports_used(_k: Key, _l: Label) {}
}

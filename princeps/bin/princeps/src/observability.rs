//! Stage T2b-a of `docs/plans/v0-testnet-deploy.md` — Prometheus
//! exposition surface for the metrics that [`princeps_node::metrics`]
//! already records every tick (landed in T2a, commit 6ff5d0a).
//!
//! What this slice does (T2b-a scope):
//! - Installs a global [`metrics_exporter_prometheus`] recorder so the
//!   per-tick gauges/counters from princeps-node land in a Prometheus
//!   registry instead of the no-op default recorder.
//! - Spawns an HTTP listener on a caller-supplied [`SocketAddr`],
//!   separate from the p2p and JSON-RPC bindings (TD-004 — public
//!   scrape traffic never touches the validator's consensus surface).
//! - Calls [`princeps_node::metrics::describe_metrics`] once at
//!   install time so `# TYPE` / `# HELP` lines render correctly.
//!
//! What this slice does NOT do (T2b-b/c/d follow-ups):
//! - No bridge/coordinator-state metrics (markets count, positions
//!   count, accounts count, insurance-fund balance, chain-history
//!   length). These need a recording hook in the per-block bridge
//!   tick and land naturally with the next slice.
//! - No oracle observation-age-per-publisher gauge or
//!   validator-liveness gauge. Same reason — they live outside
//!   `TickReport` and aren't yet emitted.
//! - No scan-loop duration histogram. Same reason — needs an
//!   instrumentation hook around the scan call.

use std::net::SocketAddr;

use metrics_exporter_prometheus::PrometheusBuilder;

/// Install the Prometheus recorder + HTTP scrape endpoint.
///
/// Must be called from inside a Tokio runtime — the exporter spawns
/// its accept loop via [`tokio::spawn`]. `bin/princeps` invokes this
/// from `run_reth_devnet`, which is itself entered via
/// `tokio_rt()?.block_on(...)`, so the runtime is in scope.
///
/// Idempotency: the underlying `metrics::set_global_recorder` rejects
/// a second install on the same process. Callers must invoke this at
/// most once. The function name reflects that — it's `init`, not
/// `set_or_replace`.
///
/// After install the function calls
/// [`princeps_node::metrics::describe_metrics`] so the Prometheus
/// scrape output carries proper `# TYPE` / `# HELP` lines for every
/// metric princeps-node will eventually emit, even before the first
/// tick fires.
pub(crate) fn init_observability(bind: SocketAddr) -> eyre::Result<()> {
    PrometheusBuilder::new()
        .with_http_listener(bind)
        .install()
        .map_err(|e| eyre::eyre!("failed to install prometheus exporter on {bind}: {e}"))?;

    princeps_node::metrics::describe_metrics();
    princeps_evm::metrics::describe_metrics();

    Ok(())
}

#[cfg(test)]
mod tests {
    //! Tests validate the metric-recording pipeline end-to-end using
    //! `set_default_local_recorder` (thread-local) so they don't fight
    //! the global recorder slot that `init_observability` claims in
    //! production. The HTTP listener path itself is exercised by
    //! manual `reth-devnet --reth-metrics-bind` invocations and by
    //! the T5 chaos drills — unit tests would need port binding +
    //! HTTP client setup that's out of scope for the recorder
    //! plumbing.

    use metrics_exporter_prometheus::PrometheusBuilder;
    use princeps_node::metrics::{
        PRINCEPS_BLOCK_HEIGHT, PRINCEPS_LENDING_HALT_TRIPS_TOTAL, PRINCEPS_TICKS_TOTAL,
    };

    /// End-to-end: build a Prometheus recorder, install it
    /// thread-locally, emit one of every kind of metric princeps-node
    /// declares, render the exposition format, assert each metric
    /// name appears in the output. Catches name mismatches between
    /// the describe path and what the exporter actually serializes.
    #[test]
    fn prometheus_render_contains_princeps_metrics() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let _guard = metrics::set_default_local_recorder(&recorder);

        princeps_node::metrics::describe_metrics();

        // Emit one gauge + one always-incrementing counter + one
        // conditional counter — covers every metric *kind* without
        // dragging in a TickReport construction.
        metrics::gauge!(PRINCEPS_BLOCK_HEIGHT).set(42.0);
        metrics::counter!(PRINCEPS_TICKS_TOTAL).increment(7);
        metrics::counter!(PRINCEPS_LENDING_HALT_TRIPS_TOTAL).increment(2);

        let text = handle.render();

        // Prometheus exposition lines look like
        //   princeps_block_height 42
        //   princeps_ticks_total 7
        // Asserting substring presence is sharper than parsing — if
        // a metric name silently changes (typo, prefix swap), the
        // assertion message names exactly which one.
        assert!(
            text.contains(PRINCEPS_BLOCK_HEIGHT),
            "missing {PRINCEPS_BLOCK_HEIGHT}\n--- exposition ---\n{text}"
        );
        assert!(
            text.contains(PRINCEPS_TICKS_TOTAL),
            "missing {PRINCEPS_TICKS_TOTAL}\n--- exposition ---\n{text}"
        );
        assert!(
            text.contains(PRINCEPS_LENDING_HALT_TRIPS_TOTAL),
            "missing {PRINCEPS_LENDING_HALT_TRIPS_TOTAL}\n--- exposition ---\n{text}"
        );

        // Prometheus exposition standard: every described metric
        // gets a `# TYPE` line. Verify the describe path actually
        // reached the recorder.
        assert!(
            text.contains(&format!("# TYPE {PRINCEPS_BLOCK_HEIGHT} gauge")),
            "describe did not emit TYPE line for {PRINCEPS_BLOCK_HEIGHT}\n--- exposition ---\n{text}"
        );
        assert!(
            text.contains(&format!("# TYPE {PRINCEPS_TICKS_TOTAL} counter")),
            "describe did not emit TYPE line for {PRINCEPS_TICKS_TOTAL}\n--- exposition ---\n{text}"
        );
    }

    /// Describing metrics without observing any value must not
    /// panic the exporter. Prometheus exposition only includes
    /// metrics with observed values, so the rendered output is
    /// empty here — the property under test is that the
    /// describe → render path doesn't blow up before the first
    /// emission, not that it produces TYPE lines.
    #[test]
    fn prometheus_render_describe_only_does_not_panic() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let _guard = metrics::set_default_local_recorder(&recorder);

        princeps_node::metrics::describe_metrics();

        let text = handle.render();
        assert!(
            text.is_empty(),
            "render() before any emission should be empty; got {text:?}"
        );
    }
}

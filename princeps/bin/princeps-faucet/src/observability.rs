//! Prometheus scrape endpoint for the faucet, mirroring the
//! pattern in `princeps/bin/princeps/src/observability.rs`.
//!
//! T4a-1 doesn't yet emit any faucet-specific metrics — those
//! land in T4a-2 (rate-limit counters) and T4a-5 (transfer
//! latency histogram + faucet-wallet balance gauge). For now
//! the installer just claims the global recorder slot so the
//! scrape endpoint is up from the first request, returning
//! `# HELP` lines for the metrics that get described over time
//! as the faucet grows.

use std::net::SocketAddr;

use metrics_exporter_prometheus::PrometheusBuilder;

/// Install the Prometheus recorder + HTTP scrape endpoint.
///
/// Must be called from inside a Tokio runtime — the exporter
/// spawns its accept loop via `tokio::spawn`. The faucet's
/// `serve` entry point is already inside `tokio::main`-equivalent
/// scope, so the runtime is available.
///
/// Idempotency: the underlying `metrics::set_global_recorder`
/// rejects a second install on the same process. Callers must
/// invoke this at most once.
pub(crate) fn init_observability(bind: SocketAddr) -> eyre::Result<()> {
    PrometheusBuilder::new()
        .with_http_listener(bind)
        .install()
        .map_err(|e| eyre::eyre!("failed to install prometheus exporter on {bind}: {e}"))?;
    Ok(())
}

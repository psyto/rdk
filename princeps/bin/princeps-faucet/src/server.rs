//! axum router + handlers.
//!
//! T4a-1 scope: two endpoints, both read-only.
//!
//! - `GET /health` — liveness probe. Returns `{"status":"ok"}`
//!   200 unconditionally. Wired into load balancer / k8s
//!   readiness later.
//! - `GET /status` — operational view. Returns config fields
//!   callers need to verify they're hitting the right faucet
//!   (chain_id, listen_addr) plus runtime fields (version,
//!   uptime).
//!
//! Routes added in later slices:
//! - `POST /drip` lands in T4a-3 with stubbed transfer; real
//!   chain submission in T4a-5.
//! - Captcha verification middleware in T4a-4.

use std::sync::Arc;
use std::time::Instant;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use tracing::info;

use crate::config::FaucetConfig;
use crate::observability;

#[derive(Clone)]
struct AppState {
    config: Arc<FaucetConfig>,
    started_at: Instant,
}

/// Top-level serve loop. Binds the configured listener, installs
/// the optional Prometheus endpoint, builds the router, and
/// runs axum until shutdown (Ctrl-C / SIGTERM — currently
/// surfaced as a plain error return from `axum::serve`; graceful
/// shutdown wiring lands when there's mutable state worth
/// flushing).
pub(crate) async fn serve(config: FaucetConfig) -> eyre::Result<()> {
    if let Some(metrics_bind) = config.metrics_bind {
        observability::init_observability(metrics_bind)?;
        info!("metrics endpoint: http://{metrics_bind}/metrics");
    }

    let listener = tokio::net::TcpListener::bind(config.listen_addr)
        .await
        .map_err(|e| {
            eyre::eyre!("bind faucet HTTP listener on {}: {e}", config.listen_addr)
        })?;
    info!("faucet listening on http://{}", config.listen_addr);

    let state = AppState {
        config: Arc::new(config),
        started_at: Instant::now(),
    };
    let app = build_router(state);

    axum::serve(listener, app)
        .await
        .map_err(|e| eyre::eyre!("axum serve loop ended: {e}"))?;
    Ok(())
}

/// Build the router with all routes wired to the supplied state.
/// Split out from `serve` so tests can exercise it without
/// binding a real socket.
fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/status", get(status))
        .with_state(state)
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}

/// Liveness probe. Always 200 — production callers should treat
/// any non-200 as "the faucet process is down". Future slices
/// may demote this to "ok if faucet wallet balance > drip amount,
/// degraded otherwise" but at T4a-1 there's no balance to check.
async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

#[derive(Serialize)]
struct StatusResponse {
    /// Faucet binary version, baked in via `env!("CARGO_PKG_VERSION")`.
    version: &'static str,
    /// Chain ID this faucet drips against. A caller can compare
    /// against the chain they're configured for; mismatch means
    /// the faucet is for a different testnet.
    chain_id: u64,
    /// The address the HTTP listener bound on — confirms the
    /// reverse-proxy / DNS layer points at the intended process.
    listen_addr: String,
    /// Seconds since `serve` started. Useful for
    /// "did the faucet just restart?" telemetry.
    uptime_secs: u64,
}

/// Operational status. Read-only; safe to expose to the public
/// behind whatever rate limiting the reverse proxy applies. The
/// fields here are deliberately non-sensitive — no wallet
/// balance, no rate-limit table state, nothing that would leak
/// faucet-drain progress.
async fn status(State(state): State<AppState>) -> Json<StatusResponse> {
    Json(StatusResponse {
        version: env!("CARGO_PKG_VERSION"),
        chain_id: state.config.chain_id,
        listen_addr: state.config.listen_addr.to_string(),
        uptime_secs: state.started_at.elapsed().as_secs(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt as _;
    use tower::ServiceExt as _;

    fn test_state() -> AppState {
        AppState {
            config: Arc::new(FaucetConfig {
                comment: Vec::new(),
                listen_addr: "127.0.0.1:8080".parse().unwrap(),
                metrics_bind: None,
                chain_id: 424242,
            }),
            started_at: Instant::now(),
        }
    }

    /// Sends a GET to `uri` and returns the parsed JSON body +
    /// the status code. Single helper keeps the per-test boiler
    /// down to a couple of lines.
    async fn get_json(uri: &str) -> (StatusCode, serde_json::Value) {
        let app = build_router(test_state());
        let resp = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        (status, json)
    }

    /// `GET /health` returns 200 with `{"status":"ok"}`. Pinned
    /// because load-balancer probes will look for this exact
    /// shape.
    #[tokio::test]
    async fn health_returns_ok() {
        let (status, body) = get_json("/health").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, serde_json::json!({"status": "ok"}));
    }

    /// `GET /status` reflects the config fields the test state
    /// loaded. If a future field rename or removal breaks this,
    /// the assertion message names the field directly.
    #[tokio::test]
    async fn status_reflects_config_fields() {
        let (status, body) = get_json("/status").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["chain_id"], 424242, "chain_id round-trip");
        assert_eq!(body["listen_addr"], "127.0.0.1:8080", "listen_addr round-trip");
        assert_eq!(body["version"], env!("CARGO_PKG_VERSION"), "version baked in");
        // uptime is non-deterministic in absolute terms but must
        // be a finite non-negative integer.
        assert!(
            body["uptime_secs"].is_u64(),
            "uptime_secs must be u64: {body}"
        );
    }

    /// Unknown route returns 404 (axum default). Pins the
    /// behavior so a future change to a wildcard fallback
    /// surfaces in tests.
    #[tokio::test]
    async fn unknown_route_returns_404() {
        let app = build_router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/no-such-route")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}

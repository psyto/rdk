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

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::captcha::{self, CaptchaError, CaptchaVerifier};
use crate::chain::{AlloyEthSender, EthSender, SendError};
use crate::config::FaucetConfig;
use crate::observability;
use crate::rate_limit::{RateLimitRejection, SqliteRateLimiter};

#[derive(Clone)]
struct AppState {
    config: Arc<FaucetConfig>,
    started_at: Instant,
    /// SQLite-backed rate limiter. Constructed once at `serve`
    /// time, shared across requests via cheap clones (the inner
    /// connection is `Arc<Mutex<...>>`).
    rate_limiter: SqliteRateLimiter,
    /// Captcha verifier. Trait-object so production
    /// (HCaptchaVerifier) and local-dev (DisabledVerifier) and
    /// tests (MockCaptchaVerifier) all flow through one shape.
    captcha_verifier: Arc<dyn CaptchaVerifier>,
    /// Chain transfer. AlloyEthSender in production,
    /// MockEthSender in tests.
    eth_sender: Arc<dyn EthSender>,
}

/// Top-level serve loop. Binds the configured listener, installs
/// the optional Prometheus endpoint, builds the router, and
/// runs axum until shutdown (Ctrl-C / SIGTERM — currently
/// surfaced as a plain error return from `axum::serve`; graceful
/// shutdown wiring lands when there's mutable state worth
/// flushing).
pub(crate) async fn serve(
    config: FaucetConfig,
    wallet_passphrase: String,
) -> eyre::Result<()> {
    if let Some(metrics_bind) = config.metrics_bind {
        observability::init_observability(metrics_bind)?;
        info!("metrics endpoint: http://{metrics_bind}/metrics");
    }

    // Open the rate-limit DB before binding the HTTP listener
    // so a misconfigured state_db_path produces a clean boot
    // failure rather than a successful boot that serves 500s.
    let rate_limiter =
        SqliteRateLimiter::open(&config.state_db_path, config.rate_limit)?;
    info!("rate-limit state: {}", config.state_db_path.display());

    // Captcha verifier construction — the build_verifier helper
    // emits a `warn!` if the provider is `Disabled` so the boot
    // log makes a misconfigured production deploy visible.
    let captcha_verifier = captcha::build_verifier(&config.captcha);

    // T4a-5: load the wallet + build the chain sender BEFORE
    // binding the HTTP listener. A misconfigured keystore or
    // RPC URL must fail at boot, not at the first drip.
    let wallet = crate::chain::load_wallet(
        &config.chain.wallet_keystore_path,
        wallet_passphrase.as_bytes(),
    )?;
    info!("wallet keystore loaded: address={}", wallet.address);
    let alloy_sender =
        AlloyEthSender::new(wallet, config.chain.rpc_url.clone(), config.chain_id)?;
    alloy_sender.verify_chain_id().await?;
    let eth_sender: Arc<dyn EthSender> = Arc::new(alloy_sender);

    let listener = tokio::net::TcpListener::bind(config.listen_addr)
        .await
        .map_err(|e| {
            eyre::eyre!("bind faucet HTTP listener on {}: {e}", config.listen_addr)
        })?;
    info!("faucet listening on http://{}", config.listen_addr);

    let state = AppState {
        config: Arc::new(config),
        started_at: Instant::now(),
        rate_limiter,
        captcha_verifier,
        eth_sender,
    };
    let app = build_router(state);

    // `into_make_service_with_connect_info` is what plumbs the
    // peer SocketAddr into the request extensions so the drip
    // handler can extract it via `ConnectInfo`. Without this,
    // the ConnectInfo extractor returns 500.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
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
        .route("/drip", post(drip))
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
    /// Configured rate limits. Surfaced so callers know what
    /// they're up against before submitting `POST /drip`.
    /// Deliberately publishes the *configured* limits, not the
    /// *current* row counts — leaking row counts would leak
    /// faucet-drain progress to a scripted attacker.
    rate_limits: RateLimitSummary,
}

#[derive(Serialize)]
struct RateLimitSummary {
    per_ip_max_drips: u32,
    per_ip_window_secs: u64,
    per_recipient_max_drips: u32,
    per_recipient_window_secs: u64,
    global_max_drips: u32,
    global_window_secs: u64,
}

/// Operational status. Read-only; safe to expose to the public
/// behind whatever rate limiting the reverse proxy applies. The
/// fields here are deliberately non-sensitive — no wallet
/// balance, no rate-limit table state, nothing that would leak
/// faucet-drain progress.
// === POST /drip (T4a-3) =====================================================
//
// Happy path for the drip request. Stubs the chain submission —
// the response `tx_hash` is all-zero until T4a-5 wires real
// alloy-based broadcast. What this slice DOES do:
//
//   - Parse the request body. `address` is required; `captcha_token`
//     is accepted but NOT verified (verification lands in T4a-4).
//   - Validate + normalize the recipient address (0x-prefixed,
//     40 hex chars, lowercased).
//   - Extract the client IP — `X-Forwarded-For` first (production
//     deploys behind a reverse proxy that sets it), peer
//     `SocketAddr` from `ConnectInfo` as fallback.
//   - Atomic rate-limit check via `SqliteRateLimiter::check_and_record`.
//   - 200 with stubbed tx_hash on success, 429 on rate-limit,
//     400 on bad input.
//
// Security note on X-Forwarded-For: this slice trusts the header
// unconditionally. The deployment runbook (T4c) MUST require that
// operators terminate inbound at a reverse proxy that STRIPS any
// inbound X-F-F and inserts its own. Without that, an attacker
// can forge the header and dodge per-IP limits — at v0 testnet
// the per-recipient + global limits still provide bounded drain.

#[derive(Debug, Deserialize)]
struct DripRequest {
    /// Recipient address. `0x` prefix required, 40 hex chars,
    /// case-insensitive on input — normalized lowercase before
    /// hitting the rate limiter so `0xABC...` and `0xabc...`
    /// share a budget.
    address: String,
    /// Captcha token. T4a-4 will verify this against the
    /// configured provider; T4a-3 only checks for presence —
    /// missing token still gets through but produces a tracing
    /// warning so operators can see captcha-less traffic in
    /// the logs while T4a-4 is in flight.
    #[serde(default)]
    captcha_token: Option<String>,
}

#[derive(Debug, Serialize)]
struct DripResponse {
    /// Stubbed at T4a-3 — always all-zero. T4a-5 returns the
    /// real broadcast tx hash here. The response shape doesn't
    /// change between T4a-3 and T4a-5; only the value does.
    tx_hash: String,
    /// Normalized lowercase recipient address.
    recipient: String,
    #[serde(with = "crate::config::u128_str")]
    eth_amount_wei: u128,
    #[serde(with = "crate::config::u128_str")]
    usdc_amount_base_units: u128,
}

/// Handler-side error type. `IntoResponse` maps each variant to
/// a status + JSON body so the call sites can just `?` their
/// way through the normal flow.
#[derive(Debug)]
enum FaucetError {
    /// Address validation failed; the inner string is the
    /// operator-visible reason ("must start with 0x", etc).
    BadAddress(String),
    /// Captcha provider is enabled but the request didn't
    /// include a token.
    CaptchaMissing,
    /// Captcha provider explicitly rejected the token.
    CaptchaFailed(String),
    /// Couldn't reach the captcha provider. 503 — this is
    /// faucet-side infrastructure, not user error. Operators
    /// page-able alongside oracle staleness.
    CaptchaUnreachable(String),
    /// Rate limiter said no. Variant carries the specific
    /// dimension hit so the response is informative.
    RateLimited(RateLimitRejection),
    /// Chain RPC was unreachable, returned a JSON-RPC error,
    /// or rejected the signed transaction. 503 to the client;
    /// the root cause is logged at error!. Distinct from the
    /// captcha-unreachable variant so alert rules can separate
    /// the two infra dependencies.
    ChainUnavailable(String),
    /// SQLite error or any other unexpected failure. The inner
    /// error is logged at `error!`; the response is generic.
    Internal(eyre::Report),
}

impl From<SendError> for FaucetError {
    fn from(e: SendError) -> Self {
        match e {
            // Rpc / RpcError both mean the chain is unavailable
            // for this drip — bucket them together client-side.
            // The server-side log distinguishes via the message.
            SendError::Rpc(s) => Self::ChainUnavailable(format!("rpc: {s}")),
            SendError::RpcError { reason } => {
                Self::ChainUnavailable(format!("rpc returned error: {reason}"))
            }
            SendError::Sign(s) => Self::Internal(eyre::eyre!("signing failed: {s}")),
            SendError::Internal(s) => Self::Internal(eyre::eyre!("send_eth internal: {s}")),
        }
    }
}

impl From<CaptchaError> for FaucetError {
    fn from(e: CaptchaError) -> Self {
        match e {
            CaptchaError::Missing => Self::CaptchaMissing,
            CaptchaError::Rejected { reason } => Self::CaptchaFailed(reason),
            CaptchaError::Network { reason } => Self::CaptchaUnreachable(reason),
        }
    }
}

impl IntoResponse for FaucetError {
    fn into_response(self) -> Response {
        let (status, body) = match self {
            Self::BadAddress(msg) => (
                StatusCode::BAD_REQUEST,
                serde_json::json!({ "error": msg, "code": "bad_address" }),
            ),
            Self::CaptchaMissing => (
                StatusCode::BAD_REQUEST,
                serde_json::json!({
                    "error": "captcha_token required",
                    "code": "captcha_missing",
                }),
            ),
            Self::CaptchaFailed(reason) => (
                StatusCode::BAD_REQUEST,
                serde_json::json!({
                    "error": format!("captcha verification failed: {reason}"),
                    "code": "captcha_failed",
                }),
            ),
            Self::CaptchaUnreachable(reason) => {
                tracing::error!("captcha provider unreachable: {reason}");
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    serde_json::json!({
                        "error": "captcha provider unreachable",
                        "code": "captcha_unreachable",
                    }),
                )
            }
            Self::RateLimited(r) => (
                StatusCode::TOO_MANY_REQUESTS,
                serde_json::json!({ "error": r.to_string(), "code": "rate_limited" }),
            ),
            Self::ChainUnavailable(reason) => {
                tracing::error!("chain transfer failed: {reason}");
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    serde_json::json!({
                        "error": "chain transfer failed",
                        "code": "chain_unavailable",
                    }),
                )
            }
            Self::Internal(e) => {
                tracing::error!("faucet internal error: {e:?}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    serde_json::json!({ "error": "internal error", "code": "internal" }),
                )
            }
        };
        (status, Json(body)).into_response()
    }
}

/// Validate a user-supplied address. Returns the normalized
/// lowercase form (`0x` + 40 lowercase hex chars) on success.
/// Pure function — easy to unit-test without HTTP plumbing.
fn normalize_address(raw: &str) -> Result<String, String> {
    let stripped = raw
        .strip_prefix("0x")
        .ok_or_else(|| "address must start with 0x".to_string())?;
    if stripped.len() != 40 {
        return Err(format!(
            "address must be 40 hex chars after 0x, got {}",
            stripped.len()
        ));
    }
    if !stripped.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("address must be hexadecimal after 0x".to_string());
    }
    Ok(format!("0x{}", stripped.to_lowercase()))
}

/// Extract the client IP from request headers, preferring
/// `X-Forwarded-For` (production deploys behind a reverse
/// proxy) and falling back to the peer `SocketAddr` from
/// `ConnectInfo`. Last-resort fallback is `0.0.0.0` so per-IP
/// rate-limit still has SOME bucket key even if neither source
/// resolves — the global cap is the load-bearing protection
/// in that misconfigured case.
fn client_ip(headers: &HeaderMap, connect_info: Option<SocketAddr>) -> IpAddr {
    headers
        .get("x-forwarded-for")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.split(',').next())
        .and_then(|s| s.trim().parse::<IpAddr>().ok())
        .or_else(|| connect_info.map(|sa| sa.ip()))
        .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
}

/// `POST /drip` — the request-flow entry point. T4a-3 stubs the
/// downstream tx broadcast; T4a-5 replaces the stub with a real
/// alloy-based send.
async fn drip(
    State(state): State<AppState>,
    connect_info: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    Json(req): Json<DripRequest>,
) -> Result<Json<DripResponse>, FaucetError> {
    // Address validation first — cheap + deterministic.
    let recipient = normalize_address(&req.address).map_err(FaucetError::BadAddress)?;

    let ip = client_ip(&headers, connect_info.map(|ci| ci.0));

    // Captcha gate next — BEFORE rate-limit recording so a
    // rejected captcha doesn't consume the user's per-IP /
    // per-recipient / global budget. Mirrors how the rate
    // limiter itself doesn't record rejected attempts.
    state
        .captcha_verifier
        .verify(req.captcha_token.as_deref(), Some(ip))
        .await?;

    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| FaucetError::Internal(eyre::eyre!("system clock before unix epoch: {e}")))?
        .as_secs();
    let now_i64 = i64::try_from(now_secs)
        .map_err(|e| FaucetError::Internal(eyre::eyre!("system clock overflows i64: {e}")))?;

    let outcome = state
        .rate_limiter
        .check_and_record(&ip.to_string(), &recipient, now_i64)
        .map_err(FaucetError::Internal)?;
    if let Err(rejection) = outcome {
        return Err(FaucetError::RateLimited(rejection));
    }

    // T4a-5: real broadcast. The recipient was normalized to
    // lowercase 0x... above; alloy's Address::from_str accepts
    // both the lowercase and EIP-55 mixed-case forms.
    let recipient_addr: alloy_primitives::Address = recipient
        .parse()
        .map_err(|e| FaucetError::Internal(eyre::eyre!("parse recipient: {e}")))?;
    let amount_wei = alloy_primitives::U256::from(state.config.drip.eth_amount_wei);
    let tx_hash = state
        .eth_sender
        .send_eth(recipient_addr, amount_wei)
        .await?;

    Ok(Json(DripResponse {
        tx_hash: format!("0x{}", hex::encode(tx_hash.as_slice())),
        recipient,
        eth_amount_wei: state.config.drip.eth_amount_wei,
        usdc_amount_base_units: state.config.drip.usdc_amount_base_units,
    }))
}

async fn status(State(state): State<AppState>) -> Json<StatusResponse> {
    let rl = state.config.rate_limit;
    Json(StatusResponse {
        version: env!("CARGO_PKG_VERSION"),
        chain_id: state.config.chain_id,
        listen_addr: state.config.listen_addr.to_string(),
        uptime_secs: state.started_at.elapsed().as_secs(),
        rate_limits: RateLimitSummary {
            per_ip_max_drips: rl.per_ip_max_drips,
            per_ip_window_secs: rl.per_ip_window_secs,
            per_recipient_max_drips: rl.per_recipient_max_drips,
            per_recipient_window_secs: rl.per_recipient_window_secs,
            global_max_drips: rl.global_max_drips,
            global_window_secs: rl.global_window_secs,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt as _;
    use tower::ServiceExt as _;

    /// Build an AppState with a passing-mock captcha. Most
    /// tests use this; a couple of T4a-4 tests swap in
    /// always_fail / network_error.
    fn test_state() -> AppState {
        test_state_with_verifier(Arc::new(
            crate::captcha::MockCaptchaVerifier::always_pass(),
        ))
    }

    fn test_state_with_verifier(verifier: Arc<dyn CaptchaVerifier>) -> AppState {
        test_state_with_components(
            verifier,
            Arc::new(crate::chain::MockEthSender::always_succeed()),
        )
    }

    fn test_state_with_sender(sender: Arc<dyn EthSender>) -> AppState {
        test_state_with_components(
            Arc::new(crate::captcha::MockCaptchaVerifier::always_pass()),
            sender,
        )
    }

    fn test_state_with_components(
        verifier: Arc<dyn CaptchaVerifier>,
        sender: Arc<dyn EthSender>,
    ) -> AppState {
        let rate_limit = crate::rate_limit::RateLimitConfig::testnet_defaults();
        AppState {
            config: Arc::new(FaucetConfig {
                comment: Vec::new(),
                listen_addr: "127.0.0.1:8080".parse().unwrap(),
                metrics_bind: None,
                chain_id: 424242,
                rate_limit,
                state_db_path: std::path::PathBuf::from(":memory:"),
                drip: crate::config::DripAmounts {
                    eth_amount_wei: 100_000_000_000_000_000,
                    usdc_amount_base_units: 10_000_000_000,
                },
                captcha: crate::captcha::CaptchaConfig::Disabled,
                chain: crate::config::ChainConfig {
                    rpc_url: "http://127.0.0.1:8545".to_string(),
                    wallet_keystore_path: std::path::PathBuf::from("/dev/null"),
                },
            }),
            started_at: Instant::now(),
            rate_limiter: SqliteRateLimiter::open_in_memory(rate_limit).unwrap(),
            captcha_verifier: verifier,
            eth_sender: sender,
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
        // T4a-2: rate-limit summary surfaced — exposes the
        // testnet defaults wired in test_state.
        let rl = &body["rate_limits"];
        assert_eq!(rl["per_ip_max_drips"], 1);
        assert_eq!(rl["per_ip_window_secs"], 86400);
        assert_eq!(rl["per_recipient_max_drips"], 1);
        assert_eq!(rl["per_recipient_window_secs"], 86400);
        assert_eq!(rl["global_max_drips"], 100);
        assert_eq!(rl["global_window_secs"], 3600);
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

    // === T4a-3: client_ip + normalize_address unit tests ====================

    #[test]
    fn client_ip_prefers_xff_first_entry() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            "203.0.113.7, 10.0.0.1, 10.0.0.2".parse().unwrap(),
        );
        let ip = client_ip(&headers, Some("127.0.0.1:1".parse().unwrap()));
        assert_eq!(ip, "203.0.113.7".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn client_ip_falls_back_to_connect_info_when_no_xff() {
        let headers = HeaderMap::new();
        let connect = Some("198.51.100.5:55000".parse().unwrap());
        let ip = client_ip(&headers, connect);
        assert_eq!(ip, "198.51.100.5".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn client_ip_unspecified_when_no_signal() {
        let headers = HeaderMap::new();
        let ip = client_ip(&headers, None);
        assert_eq!(ip, IpAddr::V4(Ipv4Addr::UNSPECIFIED));
    }

    #[test]
    fn client_ip_malformed_xff_falls_back_to_connect_info() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "not-an-ip".parse().unwrap());
        let ip = client_ip(&headers, Some("198.51.100.5:1".parse().unwrap()));
        assert_eq!(ip, "198.51.100.5".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn normalize_address_canonicalizes_mixed_case() {
        let out = normalize_address("0xAbCdEf1234567890aBcDeF1234567890ABCDEF12").unwrap();
        assert_eq!(out, "0xabcdef1234567890abcdef1234567890abcdef12");
    }

    #[test]
    fn normalize_address_rejects_missing_prefix() {
        let err = normalize_address("AbCdEf1234567890aBcDeF1234567890ABCDEF12").unwrap_err();
        assert!(err.contains("0x"), "err: {err}");
    }

    #[test]
    fn normalize_address_rejects_wrong_length() {
        let err = normalize_address("0xabc").unwrap_err();
        assert!(err.contains("40 hex chars"), "err: {err}");
    }

    #[test]
    fn normalize_address_rejects_non_hex() {
        let err = normalize_address("0xabcdef1234567890abcdef1234567890abcdef1Z").unwrap_err();
        assert!(err.contains("hexadecimal"), "err: {err}");
    }

    // === T4a-3: POST /drip via oneshot =====================================

    /// Helper: send a POST /drip with the given body + headers,
    /// return the status code and parsed JSON.
    async fn post_drip(
        state: AppState,
        body: &str,
        extra_headers: &[(&str, &str)],
    ) -> (StatusCode, serde_json::Value) {
        let app = build_router(state);
        let mut req = Request::builder()
            .method("POST")
            .uri("/drip")
            .header("content-type", "application/json");
        for (k, v) in extra_headers {
            req = req.header(*k, *v);
        }
        let resp = app
            .oneshot(req.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value =
            serde_json::from_slice(&bytes).expect("response must be JSON");
        (status, json)
    }

    /// Happy path: valid request → 200 with stubbed tx_hash,
    /// recipient normalized to lowercase, drip amounts echoed
    /// from config.
    #[tokio::test]
    async fn drip_happy_path_returns_200_with_stub_hash() {
        let state = test_state();
        let body = r#"{
            "address": "0xABCDEF1234567890abcdef1234567890ABCDEF12",
            "captcha_token": "anything"
        }"#;
        let (status, json) = post_drip(state, body, &[("x-forwarded-for", "1.2.3.4")]).await;
        assert_eq!(status, StatusCode::OK, "body: {json}");
        // T4a-5: the MockEthSender::always_succeed returns
        // 0xab repeated. Replaces the T4a-3 all-zero stub.
        assert_eq!(
            json["tx_hash"],
            format!("0x{}", "ab".repeat(32)),
            "MockEthSender returns the fixed test hash",
        );
        assert_eq!(
            json["recipient"], "0xabcdef1234567890abcdef1234567890abcdef12",
            "recipient must be lowercased"
        );
        assert_eq!(json["eth_amount_wei"], "100000000000000000");
        assert_eq!(json["usdc_amount_base_units"], "10000000000");
    }

    /// T4a-4: with the captcha provider enabled (the default
    /// mock verifier requires a token), a drip request without
    /// captcha_token is rejected with 400 + code captcha_missing.
    /// Replaces the T4a-3 "missing-token-still-succeeds" test;
    /// the gate now demands a token unless provider is Disabled.
    #[tokio::test]
    async fn drip_without_captcha_token_returns_400_when_enabled() {
        let state = test_state();
        let body = r#"{"address": "0xabcdef1234567890abcdef1234567890abcdef12"}"#;
        let (status, json) = post_drip(state, body, &[("x-forwarded-for", "1.2.3.4")]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json["code"], "captcha_missing");
    }

    /// T4a-4: when the provider is configured `Disabled`
    /// (local-dev path), missing captcha_token is fine.
    #[tokio::test]
    async fn drip_without_captcha_token_succeeds_when_disabled() {
        let state = test_state_with_verifier(Arc::new(crate::captcha::DisabledVerifier));
        let body = r#"{"address": "0xabcdef1234567890abcdef1234567890abcdef12"}"#;
        let (status, _json) = post_drip(state, body, &[("x-forwarded-for", "1.2.3.4")]).await;
        assert_eq!(status, StatusCode::OK);
    }

    /// T4a-4: provider rejected the token → 400 + code
    /// captcha_failed + the provider's reason surfaced.
    #[tokio::test]
    async fn drip_with_failing_captcha_returns_400() {
        let state = test_state_with_verifier(Arc::new(
            crate::captcha::MockCaptchaVerifier::always_fail("expired-token"),
        ));
        let body = r#"{"address": "0xabcdef1234567890abcdef1234567890abcdef12", "captcha_token": "x"}"#;
        let (status, json) = post_drip(state, body, &[("x-forwarded-for", "1.2.3.4")]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json["code"], "captcha_failed");
        assert!(
            json["error"].as_str().unwrap().contains("expired-token"),
            "error: {json}"
        );
    }

    /// T4a-4: provider was unreachable → 503 + code
    /// captcha_unreachable. Distinct from user-error 400 so
    /// operator dashboards / alert rules can separate the
    /// signals.
    #[tokio::test]
    async fn drip_with_captcha_network_error_returns_503() {
        let state = test_state_with_verifier(Arc::new(
            crate::captcha::MockCaptchaVerifier::network_error("dns failed"),
        ));
        let body = r#"{"address": "0xabcdef1234567890abcdef1234567890abcdef12", "captcha_token": "x"}"#;
        let (status, json) = post_drip(state, body, &[("x-forwarded-for", "1.2.3.4")]).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(json["code"], "captcha_unreachable");
    }

    /// T4a-5: chain RPC unavailable → 503 with code
    /// chain_unavailable. Maps SendError::Rpc through to the
    /// client + logs the reason.
    #[tokio::test]
    async fn drip_chain_unreachable_returns_503() {
        let state = test_state_with_sender(Arc::new(
            crate::chain::MockEthSender::rpc_unreachable("connection refused"),
        ));
        let body = r#"{"address": "0xabcdef1234567890abcdef1234567890abcdef12", "captcha_token": "x"}"#;
        let (status, json) = post_drip(state, body, &[("x-forwarded-for", "1.2.3.4")]).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(json["code"], "chain_unavailable");
    }

    /// T4a-5: RPC returned a JSON-RPC error envelope (e.g.
    /// nonce too low, insufficient funds). Same 503 surface,
    /// distinct from the transport-layer error above.
    #[tokio::test]
    async fn drip_chain_rpc_error_returns_503() {
        let state = test_state_with_sender(Arc::new(
            crate::chain::MockEthSender::rpc_error("insufficient funds"),
        ));
        let body = r#"{"address": "0xabcdef1234567890abcdef1234567890abcdef12", "captcha_token": "x"}"#;
        let (status, json) = post_drip(state, body, &[("x-forwarded-for", "1.2.3.4")]).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(json["code"], "chain_unavailable");
    }

    /// T4a-4: failing captcha must NOT consume the user's
    /// rate-limit budget — the captcha gate runs before the
    /// rate-limit recording, so a rejected request leaves the
    /// drip table untouched. Confirmed by a follow-up drip
    /// from the same IP+recipient that succeeds.
    #[tokio::test]
    async fn drip_failing_captcha_does_not_consume_rate_limit_budget() {
        // First request: failing-captcha state.
        let failing_state = test_state_with_verifier(Arc::new(
            crate::captcha::MockCaptchaVerifier::always_fail("bad-token"),
        ));
        // Capture the rate-limiter so we can hand it to the
        // second state — both states must share the same
        // SQLite-backed budget for this test to be meaningful.
        let rate_limiter = failing_state.rate_limiter.clone();

        let body = r#"{"address": "0xabcdef1234567890abcdef1234567890abcdef12", "captcha_token": "x"}"#;
        let (s1, _) = post_drip(failing_state, body, &[("x-forwarded-for", "1.2.3.4")]).await;
        assert_eq!(s1, StatusCode::BAD_REQUEST);

        // Second request: passing-captcha state, BUT sharing
        // the rate-limit budget.
        let mut passing_state = test_state();
        passing_state.rate_limiter = rate_limiter;
        let (s2, _) = post_drip(passing_state, body, &[("x-forwarded-for", "1.2.3.4")]).await;
        assert_eq!(
            s2,
            StatusCode::OK,
            "rejected-captcha request must not consume rate-limit budget"
        );
    }

    /// Bad address (wrong length) returns 400 with the
    /// validation error in the body.
    #[tokio::test]
    async fn drip_bad_address_returns_400() {
        let state = test_state();
        let body = r#"{"address": "0xshort", "captcha_token": "x"}"#;
        let (status, json) = post_drip(state, body, &[("x-forwarded-for", "1.2.3.4")]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json["code"], "bad_address");
        assert!(
            json["error"].as_str().unwrap().contains("40 hex chars"),
            "error: {json}"
        );
    }

    /// Missing `address` field is a JSON-deserialize failure
    /// inside axum's `Json` extractor — returns 422
    /// (Unprocessable Entity) with a plain-text body. The
    /// status code is what callers branch on; the body shape
    /// is axum's, not ours, so we don't pin it here.
    #[tokio::test]
    async fn drip_missing_address_returns_4xx() {
        let app = build_router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/drip")
                    .header("content-type", "application/json")
                    .header("x-forwarded-for", "1.2.3.4")
                    .body(Body::from(r#"{"captcha_token": "x"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            resp.status().is_client_error(),
            "expected 4xx, got {}",
            resp.status()
        );
    }

    /// Second drip from the same IP inside the per-IP window
    /// is rejected with 429 + rate_limited code. End-to-end
    /// verification that the rate limiter is wired into the
    /// handler, not just sitting in AppState.
    #[tokio::test]
    async fn drip_rate_limited_returns_429() {
        let state = test_state();
        let body1 = r#"{"address": "0x1111111111111111111111111111111111111111", "captcha_token": "x"}"#;
        let body2 = r#"{"address": "0x2222222222222222222222222222222222222222", "captcha_token": "x"}"#;
        let xff = [("x-forwarded-for", "1.2.3.4")];

        let (s1, _) = post_drip(state.clone(), body1, &xff).await;
        assert_eq!(s1, StatusCode::OK);
        // Same IP, fresh recipient → per-IP limit hits.
        let (s2, j2) = post_drip(state, body2, &xff).await;
        assert_eq!(s2, StatusCode::TOO_MANY_REQUESTS, "body: {j2}");
        assert_eq!(j2["code"], "rate_limited");
        assert!(
            j2["error"].as_str().unwrap().contains("per-IP"),
            "error: {j2}"
        );
    }

    /// X-Forwarded-For is the rate-limit key — two requests
    /// with different X-F-F headers from the same connection
    /// must each get their own per-IP bucket. Confirms the
    /// reverse-proxy assumption documented at the handler.
    #[tokio::test]
    async fn drip_xff_partitions_per_ip_buckets() {
        let state = test_state();
        let body1 = r#"{"address": "0x1111111111111111111111111111111111111111", "captcha_token": "x"}"#;
        let body2 = r#"{"address": "0x2222222222222222222222222222222222222222", "captcha_token": "x"}"#;

        let (s1, _) = post_drip(state.clone(), body1, &[("x-forwarded-for", "1.2.3.4")]).await;
        assert_eq!(s1, StatusCode::OK);
        let (s2, _) = post_drip(state, body2, &[("x-forwarded-for", "5.6.7.8")]).await;
        assert_eq!(s2, StatusCode::OK, "different X-F-F should partition buckets");
    }
}

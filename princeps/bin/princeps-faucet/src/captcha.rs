//! Stage T4a-4 of `docs/plans/v0-testnet-deploy.md` — captcha
//! verification gate for `POST /drip`.
//!
//! At v0 the only supported provider is **hCaptcha**:
//!
//! - Free tier sufficient for testnet faucet traffic.
//! - No Google dependency / privacy-friendly.
//! - Same `siteverify` API shape as reCAPTCHA, so swapping for
//!   that later is mostly a hostname change.
//!
//! The configured provider is one of:
//!
//! - `"disabled"` — skip verification entirely. Useful for
//!   local dev (no captcha keys to provision) and for the
//!   initial bring-up window before captcha secrets land in
//!   the deployment. **Documented loudly as local-dev only**;
//!   production deployments MUST set `"hcaptcha"`.
//! - `"hcaptcha"` — POST to `https://api.hcaptcha.com/siteverify`
//!   with the secret + token; the provider's `success: bool`
//!   field gates the drip.
//!
//! ## Failure modes
//!
//! [`CaptchaError`] distinguishes three operator-visible cases:
//!
//! - `Missing` — drip request didn't include `captcha_token`
//!   AND the provider isn't `Disabled`. Operator-visible as
//!   400 `code: "captcha_missing"`.
//! - `Rejected` — provider said the token was invalid (expired,
//!   already-used, wrong site, etc.). 400 `code: "captcha_failed"`.
//! - `Network` — couldn't reach the provider. 503 / 500 because
//!   this is faucet-side infrastructure failure, not user error.
//!
//! ## Testability
//!
//! [`CaptchaVerifier`] is a trait so tests can plug in
//! [`MockCaptchaVerifier`] without making outbound HTTP calls.
//! Production uses [`HCaptchaVerifier`]; local-dev uses
//! [`DisabledVerifier`].

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::warn;

/// Operator-facing captcha selection. Lives in
/// [`crate::config::FaucetConfig::captcha`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "lowercase")]
pub(crate) enum CaptchaConfig {
    /// No verification. Local-dev only — production MUST switch
    /// to `Hcaptcha`. The boot path logs a `warn!` on every
    /// boot under this variant so a misconfigured production
    /// deploy is visible in journalctl.
    Disabled,
    /// hCaptcha siteverify against `https://api.hcaptcha.com/siteverify`.
    Hcaptcha {
        /// hCaptcha account secret. Treat as sensitive — chmod
        /// the config file 0o640 and limit who reads it. The
        /// faucet logs this nowhere (the boot line names the
        /// provider only).
        secret: String,
        /// hCaptcha site key. NOT used server-side — the
        /// browser-side widget needs it. Documented in the
        /// config so the deployment runbook (T4c) has it in
        /// one place. Optional from the parser's POV; absence
        /// is not a server-side failure.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        site_key: Option<String>,
    },
}

/// Why a verification failed. Maps to the drip handler's HTTP
/// status + JSON error code.
#[derive(Debug)]
pub(crate) enum CaptchaError {
    /// Provider is enabled but the request omitted `captcha_token`.
    Missing,
    /// Provider explicitly rejected the token. `reason` is the
    /// provider's first error code, or a generic message.
    Rejected { reason: String },
    /// Couldn't reach the provider — DNS, TCP, timeout, etc.
    Network { reason: String },
}

impl std::fmt::Display for CaptchaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => f.write_str("captcha_token required"),
            Self::Rejected { reason } => write!(f, "captcha verification failed: {reason}"),
            Self::Network { reason } => write!(f, "captcha provider unreachable: {reason}"),
        }
    }
}

/// Verify a captcha token. The `client_ip` is optional context
/// the provider can use to bind the token to a session — not
/// load-bearing on accept/reject by itself.
#[async_trait]
pub(crate) trait CaptchaVerifier: Send + Sync {
    async fn verify(
        &self,
        token: Option<&str>,
        client_ip: Option<IpAddr>,
    ) -> Result<(), CaptchaError>;
}

/// Construct the production verifier from a [`CaptchaConfig`].
/// Boxed `Arc<dyn>` so `AppState` (which must be `Clone`) can
/// carry it without leaking the concrete type through the
/// router.
pub(crate) fn build_verifier(
    config: &CaptchaConfig,
) -> Arc<dyn CaptchaVerifier> {
    match config {
        CaptchaConfig::Disabled => {
            warn!(
                "captcha provider is disabled — POST /drip will accept any \
                 request that passes rate-limit. DO NOT use in production."
            );
            Arc::new(DisabledVerifier)
        }
        CaptchaConfig::Hcaptcha { secret, .. } => {
            Arc::new(HCaptchaVerifier::new(secret.clone()))
        }
    }
}

// === implementations =======================================================

/// No-op verifier — accepts every request, including missing
/// tokens. Local-dev only.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DisabledVerifier;

#[async_trait]
impl CaptchaVerifier for DisabledVerifier {
    async fn verify(
        &self,
        _token: Option<&str>,
        _client_ip: Option<IpAddr>,
    ) -> Result<(), CaptchaError> {
        Ok(())
    }
}

/// hCaptcha siteverify POST. One `reqwest::Client` reused
/// across requests so the TLS session pool isn't blown each
/// drip.
#[derive(Debug, Clone)]
pub(crate) struct HCaptchaVerifier {
    client: reqwest::Client,
    secret: String,
    endpoint: String,
}

impl HCaptchaVerifier {
    pub(crate) fn new(secret: String) -> Self {
        Self::new_with_endpoint(secret, "https://api.hcaptcha.com/siteverify".to_string())
    }

    /// Construct against an arbitrary endpoint — used by tests
    /// to point at a local httpbin / wiremock without touching
    /// the public hCaptcha service.
    fn new_with_endpoint(secret: String, endpoint: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("reqwest client build");
        Self {
            client,
            secret,
            endpoint,
        }
    }
}

#[derive(Deserialize)]
struct HCaptchaResponse {
    success: bool,
    #[serde(rename = "error-codes", default)]
    error_codes: Vec<String>,
}

#[async_trait]
impl CaptchaVerifier for HCaptchaVerifier {
    async fn verify(
        &self,
        token: Option<&str>,
        client_ip: Option<IpAddr>,
    ) -> Result<(), CaptchaError> {
        let token = token.ok_or(CaptchaError::Missing)?;
        if token.is_empty() {
            return Err(CaptchaError::Missing);
        }

        // hCaptcha siteverify expects form-urlencoded. `remoteip`
        // is optional; we always send it when we know it because
        // the provider can use it to flag suspicious sources even
        // when the token verifies.
        let mut form: Vec<(&str, String)> =
            vec![("secret", self.secret.clone()), ("response", token.to_string())];
        if let Some(ip) = client_ip {
            form.push(("remoteip", ip.to_string()));
        }

        let resp = self
            .client
            .post(&self.endpoint)
            .form(&form)
            .send()
            .await
            .map_err(|e| CaptchaError::Network {
                reason: e.to_string(),
            })?;

        if !resp.status().is_success() {
            return Err(CaptchaError::Network {
                reason: format!("hCaptcha returned HTTP {}", resp.status()),
            });
        }

        let body: HCaptchaResponse = resp.json().await.map_err(|e| CaptchaError::Network {
            reason: format!("malformed hCaptcha response: {e}"),
        })?;

        if body.success {
            Ok(())
        } else {
            let reason = body
                .error_codes
                .first()
                .cloned()
                .unwrap_or_else(|| "no error code provided".to_string());
            Err(CaptchaError::Rejected { reason })
        }
    }
}

// === test-only verifier =====================================================

/// Mock for unit tests. Construct via the helper constructors to
/// avoid plumbing a clone of the inner config through every test
/// site.
#[cfg(test)]
#[derive(Debug, Clone)]
pub(crate) struct MockCaptchaVerifier {
    behavior: MockBehavior,
}

#[cfg(test)]
#[derive(Debug, Clone)]
enum MockBehavior {
    AlwaysPass,
    AlwaysFail(String),
    NetworkError(String),
}

#[cfg(test)]
impl MockCaptchaVerifier {
    pub(crate) fn always_pass() -> Self {
        Self {
            behavior: MockBehavior::AlwaysPass,
        }
    }

    pub(crate) fn always_fail(reason: impl Into<String>) -> Self {
        Self {
            behavior: MockBehavior::AlwaysFail(reason.into()),
        }
    }

    pub(crate) fn network_error(reason: impl Into<String>) -> Self {
        Self {
            behavior: MockBehavior::NetworkError(reason.into()),
        }
    }
}

#[cfg(test)]
#[async_trait]
impl CaptchaVerifier for MockCaptchaVerifier {
    async fn verify(
        &self,
        token: Option<&str>,
        _client_ip: Option<IpAddr>,
    ) -> Result<(), CaptchaError> {
        match &self.behavior {
            MockBehavior::AlwaysPass => {
                // Still enforce the Missing path so callers can
                // distinguish "provider enabled, missing token"
                // from "provider disabled".
                if token.is_none() {
                    return Err(CaptchaError::Missing);
                }
                Ok(())
            }
            MockBehavior::AlwaysFail(reason) => Err(CaptchaError::Rejected {
                reason: reason.clone(),
            }),
            MockBehavior::NetworkError(reason) => Err(CaptchaError::Network {
                reason: reason.clone(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Disabled verifier accepts both Some and None tokens —
    /// dev-loop ergonomics depend on this.
    #[tokio::test]
    async fn disabled_accepts_anything() {
        let v = DisabledVerifier;
        assert!(v.verify(Some("anything"), None).await.is_ok());
        assert!(v.verify(None, None).await.is_ok());
        assert!(v.verify(Some(""), None).await.is_ok());
    }

    /// Mock-pass verifier accepts non-empty Some tokens and
    /// rejects None with Missing. This is the contract the drip
    /// handler depends on for the "provider enabled but token
    /// absent" path.
    #[tokio::test]
    async fn mock_pass_requires_token_present() {
        let v = MockCaptchaVerifier::always_pass();
        assert!(v.verify(Some("tok"), None).await.is_ok());
        assert!(matches!(
            v.verify(None, None).await,
            Err(CaptchaError::Missing)
        ));
    }

    /// Mock-fail verifier rejects with the configured reason
    /// surfaced in the error message.
    #[tokio::test]
    async fn mock_fail_surfaces_reason() {
        let v = MockCaptchaVerifier::always_fail("expired-token");
        let err = v.verify(Some("tok"), None).await.unwrap_err();
        match err {
            CaptchaError::Rejected { reason } => assert_eq!(reason, "expired-token"),
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    /// Mock-network-error verifier returns a Network variant —
    /// the handler maps this to 500/503, not 400, because it's
    /// faucet-side infrastructure failure.
    #[tokio::test]
    async fn mock_network_error_propagates() {
        let v = MockCaptchaVerifier::network_error("timeout");
        let err = v.verify(Some("tok"), None).await.unwrap_err();
        assert!(matches!(err, CaptchaError::Network { .. }));
    }

    /// CaptchaConfig round-trips through serde correctly with
    /// the externally-tagged `provider` discriminator.
    #[test]
    fn captcha_config_round_trips_via_serde() {
        let disabled = serde_json::from_str::<CaptchaConfig>(r#"{ "provider": "disabled" }"#)
            .unwrap();
        assert!(matches!(disabled, CaptchaConfig::Disabled));

        let hcap = serde_json::from_str::<CaptchaConfig>(
            r#"{ "provider": "hcaptcha", "secret": "S", "site_key": "K" }"#,
        )
        .unwrap();
        match hcap {
            CaptchaConfig::Hcaptcha { secret, site_key } => {
                assert_eq!(secret, "S");
                assert_eq!(site_key.as_deref(), Some("K"));
            }
            _ => panic!("expected Hcaptcha variant"),
        }

        // site_key is optional — pure server-side parsing works
        // without it.
        let hcap_no_sitekey = serde_json::from_str::<CaptchaConfig>(
            r#"{ "provider": "hcaptcha", "secret": "S" }"#,
        )
        .unwrap();
        assert!(matches!(
            hcap_no_sitekey,
            CaptchaConfig::Hcaptcha { site_key: None, .. }
        ));
    }

    /// CaptchaError Display impl carries the reason — pinned
    /// because the drip handler embeds the message in the 400
    /// response body.
    #[test]
    fn captcha_error_display_includes_reason() {
        assert_eq!(CaptchaError::Missing.to_string(), "captcha_token required");
        assert_eq!(
            CaptchaError::Rejected {
                reason: "expired".into()
            }
            .to_string(),
            "captcha verification failed: expired"
        );
        assert_eq!(
            CaptchaError::Network {
                reason: "dns".into()
            }
            .to_string(),
            "captcha provider unreachable: dns"
        );
    }

    /// HCaptchaVerifier::new_with_endpoint round-trips the
    /// secret + endpoint so production callers can construct
    /// it from the parsed config without surprises.
    #[test]
    fn hcaptcha_verifier_constructs() {
        let v = HCaptchaVerifier::new_with_endpoint(
            "secret".into(),
            "https://example.com/x".into(),
        );
        assert_eq!(v.secret, "secret");
        assert_eq!(v.endpoint, "https://example.com/x");
    }
}

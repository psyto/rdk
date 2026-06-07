//! Faucet config — JSON on disk, mirroring the genesis-file
//! convention used elsewhere in the princeps repo (see
//! `princeps/genesis/testnet-genesis.json` for the established
//! pattern: `_comment` array for in-file documentation, real
//! fields below).
//!
//! T4a-1 ships the minimal shape needed for the skeleton:
//! HTTP bind, optional Prometheus bind, chain ID. Later slices
//! grow this struct:
//! - T4a-2 will add the rate-limit knobs + SQLite state path.
//! - T4a-3 will add drip amounts (ETH wei, USDC base units).
//! - T4a-4 will add captcha provider config.
//! - T4a-5 will add the chain RPC URL + faucet key reference.

use std::fs;
use std::net::SocketAddr;
use std::path::Path;

use eyre::Context as _;
use serde::{Deserialize, Serialize};

/// The on-disk faucet config.
///
/// `_comment` is the documentation-block convention used by
/// `testnet-genesis.json`; it's preserved on round-trip and
/// otherwise ignored by the loader. Operators put deployment
/// notes there so the committed config is self-describing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct FaucetConfig {
    #[serde(default, skip_serializing_if = "Vec::is_empty", rename = "_comment")]
    pub comment: Vec<String>,

    /// Where the HTTP API listens. The faucet binds here from
    /// `server::serve`; production deployments typically put this
    /// behind a TLS-terminating reverse proxy.
    pub listen_addr: SocketAddr,

    /// Optional Prometheus scrape endpoint. When `None`, no
    /// recorder is installed and the `metrics::*` macros in the
    /// faucet (once they exist) become no-ops. TD-006 / TD-004
    /// expect this to live on a different port from `listen_addr`
    /// so public traffic and internal scraping don't share a
    /// surface.
    #[serde(default)]
    pub metrics_bind: Option<SocketAddr>,

    /// Chain ID this faucet serves. Surfaced via `GET /status` so
    /// callers can verify they're talking to the right faucet
    /// before submitting a `POST /drip`. Mirrors the `chain_id`
    /// field in `testnet-genesis.json` (placeholder `424242` until
    /// T1a allocates the real number).
    pub chain_id: u64,
}

impl FaucetConfig {
    /// Read + parse the config at `path`. Errors carry the path
    /// for diagnosability — operators boot through systemd and
    /// the journalctl line is often the only signal they get.
    pub(crate) fn load(path: &Path) -> eyre::Result<Self> {
        let bytes = fs::read(path)
            .with_context(|| format!("read faucet config at {}", path.display()))?;
        let cfg: Self = serde_json::from_slice(&bytes)
            .map_err(|e| eyre::eyre!("malformed faucet config at {}: {e}", path.display()))?;
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_config(dir: &TempDir, body: &str) -> std::path::PathBuf {
        let path = dir.path().join("faucet.json");
        std::fs::write(&path, body).unwrap();
        path
    }

    /// Round-trip: full-shape config parses, re-serializes,
    /// re-parses identical. Pins the on-disk format so any future
    /// field add/rename surfaces here.
    #[test]
    fn round_trip_preserves_fields() {
        let dir = TempDir::new().unwrap();
        let body = r#"{
            "_comment": ["first-cut faucet config for v0"],
            "listen_addr": "127.0.0.1:8080",
            "metrics_bind": "127.0.0.1:9091",
            "chain_id": 424242
        }"#;
        let path = write_config(&dir, body);
        let cfg = FaucetConfig::load(&path).expect("load");
        assert_eq!(cfg.chain_id, 424242);
        assert_eq!(cfg.listen_addr.port(), 8080);
        assert_eq!(cfg.metrics_bind.unwrap().port(), 9091);
        assert_eq!(cfg.comment, vec!["first-cut faucet config for v0"]);

        let json = serde_json::to_string_pretty(&cfg).unwrap();
        let reparsed: FaucetConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(reparsed.chain_id, cfg.chain_id);
        assert_eq!(reparsed.listen_addr, cfg.listen_addr);
        assert_eq!(reparsed.metrics_bind, cfg.metrics_bind);
        assert_eq!(reparsed.comment, cfg.comment);
    }

    /// `metrics_bind` and `_comment` are optional. A bare-bones
    /// config without them must still parse.
    #[test]
    fn omits_optional_fields_cleanly() {
        let dir = TempDir::new().unwrap();
        let body = r#"{
            "listen_addr": "0.0.0.0:8080",
            "chain_id": 424242
        }"#;
        let path = write_config(&dir, body);
        let cfg = FaucetConfig::load(&path).expect("load");
        assert!(cfg.metrics_bind.is_none());
        assert!(cfg.comment.is_empty());
    }

    /// Missing required field → parse error naming the path,
    /// not a panic.
    #[test]
    fn missing_required_field_errors_with_path() {
        let dir = TempDir::new().unwrap();
        let body = r#"{ "listen_addr": "127.0.0.1:8080" }"#; // missing chain_id
        let path = write_config(&dir, body);
        let err = FaucetConfig::load(&path).expect_err("must error");
        let msg = err.to_string();
        assert!(msg.contains("malformed faucet config"), "msg: {msg}");
        assert!(msg.contains(&path.display().to_string()), "msg: {msg}");
    }

    /// Missing file → error naming the path. Distinct from the
    /// malformed-content error above.
    #[test]
    fn missing_file_errors_with_path() {
        let bogus = std::path::PathBuf::from("/no/such/faucet/config.json");
        let err = FaucetConfig::load(&bogus).expect_err("must error");
        let msg = err.to_string();
        assert!(msg.contains("read faucet config"), "msg: {msg}");
        assert!(msg.contains(&bogus.display().to_string()), "msg: {msg}");
    }

    /// Malformed JSON → error naming the path. Catches typos in
    /// committed configs.
    #[test]
    fn malformed_json_errors_with_path() {
        let dir = TempDir::new().unwrap();
        let path = write_config(&dir, "not json");
        let err = FaucetConfig::load(&path).expect_err("must error");
        let msg = err.to_string();
        assert!(msg.contains("malformed faucet config"));
        assert!(msg.contains(&path.display().to_string()));
    }
}

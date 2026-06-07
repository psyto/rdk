//! Stage T4a-5 of `docs/plans/v0-testnet-deploy.md` — real chain
//! transfer for the faucet.
//!
//! Replaces the T4a-3 stub `0x000…000` tx_hash with the actual
//! broadcast hash. The faucet:
//!
//! 1. Loads its wallet from an encrypted Ethereum V3 keystore
//!    (the de-facto secp256k1 keystore format — readable by
//!    geth, `cast wallet`, foundry, and anything else in the
//!    Ethereum ecosystem). Reuses `alloy_signer_local::LocalSigner`
//!    so the faucet binary doesn't reimplement scrypt + AES-128-CTR.
//! 2. Builds three JSON-RPC requests against the configured RPC
//!    URL: `eth_chainId` (boot-time sanity check), `eth_gasPrice`
//!    (sized as the EIP-1559 max-fee + tip), `eth_getTransactionCount`
//!    (nonce), and `eth_sendRawTransaction` (the broadcast).
//! 3. Signs an EIP-1559 transaction via the LocalSigner and
//!    submits the EIP-2718-envelope bytes hex-encoded.
//!
//! ## USDC scope
//!
//! **Out of scope at v0.** Princeps USDC is bridge-side
//! accounting indexed by `AccountId(u64)`, not an ERC-20 at an
//! EVM address. Crediting USDC to an external user requires
//! mapping the user's `0x…` address to an `AccountId`, and no
//! such mapping convention exists yet at the system level —
//! letting the faucet pick one unilaterally would lock in a
//! choice that affects every downstream bridge flow (deposit,
//! borrow, withdraw, liquidation). The v0-testnet-deploy plan
//! footer documents this deferral. T4a-5 ships ETH-only; USDC
//! lands in a later slice once the address↔AccountId story is
//! settled.
//!
//! ## Testability
//!
//! [`EthSender`] is a trait so handler tests don't have to
//! stand up a live RPC. Production uses [`AlloyEthSender`];
//! tests inject [`MockEthSender`] with configurable
//! success/failure behaviors. Same shape as `CaptchaVerifier`.

use std::path::Path;

use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy_network::TxSignerSync as _;
use alloy_primitives::{Address, Bytes, TxKind, B256, U256};
use alloy_rlp::Encodable as _;
use alloy_signer_local::PrivateKeySigner;
use async_trait::async_trait;
use eyre::Context as _;
use serde::Deserialize;
use tracing::{info, warn};

// === Wallet loading =========================================================

/// Loaded faucet wallet — owns the in-memory secp256k1 signer
/// and remembers its own EVM address for tx construction.
#[derive(Clone)]
pub(crate) struct FaucetWallet {
    pub signer: PrivateKeySigner,
    pub address: Address,
}

impl std::fmt::Debug for FaucetWallet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FaucetWallet")
            .field("address", &self.address)
            .field("signer", &"<redacted>")
            .finish()
    }
}

/// Decrypt a V3 keystore from disk under `passphrase`. Errors are
/// path-naming so a misconfigured systemd `LoadCredential=` is
/// diagnosable from journalctl.
pub(crate) fn load_wallet(
    keystore_path: &Path,
    passphrase: &[u8],
) -> eyre::Result<FaucetWallet> {
    if passphrase.is_empty() {
        eyre::bail!("wallet keystore passphrase must not be empty");
    }
    let signer = PrivateKeySigner::decrypt_keystore(keystore_path, passphrase).map_err(|e| {
        eyre::eyre!(
            "decrypt wallet keystore at {}: {e}",
            keystore_path.display()
        )
    })?;
    let address = signer.address();
    Ok(FaucetWallet { signer, address })
}

// === EthSender trait + impls ================================================

/// Why an ETH send failed. The drip handler maps these to
/// 503 (chain/RPC infra) or 500 (faucet-internal logic).
#[derive(Debug)]
pub(crate) enum SendError {
    /// JSON-RPC call failed at the transport layer — DNS,
    /// connection refused, timeout, non-2xx HTTP status.
    Rpc(String),
    /// RPC returned a JSON-RPC error envelope: malformed tx,
    /// nonce too low, insufficient funds, etc. `reason`
    /// carries the provider's error message verbatim.
    RpcError { reason: String },
    /// Signing the transaction failed locally. Should be
    /// unreachable on a well-formed tx; surfaces as 500.
    Sign(String),
    /// Anything else (encoding bug, etc).
    Internal(String),
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rpc(s) => write!(f, "rpc transport error: {s}"),
            Self::RpcError { reason } => write!(f, "rpc returned error: {reason}"),
            Self::Sign(s) => write!(f, "signing failed: {s}"),
            Self::Internal(s) => write!(f, "internal: {s}"),
        }
    }
}

/// Send `amount_wei` ETH to `recipient` from the faucet wallet.
/// Returns the broadcast tx hash on success.
#[async_trait]
pub(crate) trait EthSender: Send + Sync {
    async fn send_eth(
        &self,
        recipient: Address,
        amount_wei: U256,
    ) -> Result<B256, SendError>;
}

/// Production sender: signs an EIP-1559 tx with the configured
/// wallet and submits via JSON-RPC.
#[derive(Clone)]
pub(crate) struct AlloyEthSender {
    wallet: FaucetWallet,
    rpc_client: reqwest::Client,
    rpc_url: String,
    chain_id: u64,
}

impl std::fmt::Debug for AlloyEthSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AlloyEthSender")
            .field("wallet_address", &self.wallet.address)
            .field("rpc_url", &self.rpc_url)
            .field("chain_id", &self.chain_id)
            .finish()
    }
}

impl AlloyEthSender {
    pub(crate) fn new(
        wallet: FaucetWallet,
        rpc_url: String,
        chain_id: u64,
    ) -> eyre::Result<Self> {
        let rpc_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| eyre::eyre!("build reqwest client: {e}"))?;
        Ok(Self {
            wallet,
            rpc_client,
            rpc_url,
            chain_id,
        })
    }

    /// Best-effort boot-time confirmation that the configured
    /// RPC URL is reachable AND serving the expected chain ID.
    /// Surfaces a clean error at boot rather than a confusing
    /// "wrong-chain signature" failure on the first drip.
    pub(crate) async fn verify_chain_id(&self) -> eyre::Result<()> {
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "eth_chainId",
            "params": [],
        });
        let resp: RpcResponse<String> = self
            .rpc_client
            .post(&self.rpc_url)
            .json(&req)
            .send()
            .await
            .with_context(|| format!("eth_chainId against {}", self.rpc_url))?
            .json()
            .await
            .with_context(|| "parse eth_chainId response")?;
        let chain_hex = resp.into_result()
            .map_err(|e| eyre::eyre!("eth_chainId returned error: {e}"))?;
        let chain_id = u64::from_str_radix(chain_hex.trim_start_matches("0x"), 16)
            .map_err(|e| eyre::eyre!("malformed chain id {chain_hex:?}: {e}"))?;
        if chain_id != self.chain_id {
            eyre::bail!(
                "RPC reports chain_id {} but faucet config says {} — refusing to boot",
                chain_id,
                self.chain_id
            );
        }
        info!(
            "wallet {} ready against {} (chain {})",
            self.wallet.address, self.rpc_url, self.chain_id
        );
        Ok(())
    }

    async fn get_nonce(&self) -> Result<u64, SendError> {
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "eth_getTransactionCount",
            "params": [self.wallet.address, "pending"],
        });
        let resp: RpcResponse<String> = self
            .rpc_client
            .post(&self.rpc_url)
            .json(&req)
            .send()
            .await
            .map_err(|e| SendError::Rpc(e.to_string()))?
            .json()
            .await
            .map_err(|e| SendError::Rpc(format!("parse response: {e}")))?;
        let nonce_hex = resp.into_result().map_err(|reason| SendError::RpcError { reason })?;
        u64::from_str_radix(nonce_hex.trim_start_matches("0x"), 16)
            .map_err(|e| SendError::Internal(format!("malformed nonce {nonce_hex:?}: {e}")))
    }

    async fn get_gas_price(&self) -> Result<u128, SendError> {
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "eth_gasPrice",
            "params": [],
        });
        let resp: RpcResponse<String> = self
            .rpc_client
            .post(&self.rpc_url)
            .json(&req)
            .send()
            .await
            .map_err(|e| SendError::Rpc(e.to_string()))?
            .json()
            .await
            .map_err(|e| SendError::Rpc(format!("parse response: {e}")))?;
        let price_hex = resp.into_result().map_err(|reason| SendError::RpcError { reason })?;
        u128::from_str_radix(price_hex.trim_start_matches("0x"), 16)
            .map_err(|e| SendError::Internal(format!("malformed gas price {price_hex:?}: {e}")))
    }

    async fn send_raw_transaction(&self, encoded: &[u8]) -> Result<B256, SendError> {
        let hex_tx = format!("0x{}", hex::encode(encoded));
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "eth_sendRawTransaction",
            "params": [hex_tx],
        });
        let resp: RpcResponse<String> = self
            .rpc_client
            .post(&self.rpc_url)
            .json(&req)
            .send()
            .await
            .map_err(|e| SendError::Rpc(e.to_string()))?
            .json()
            .await
            .map_err(|e| SendError::Rpc(format!("parse response: {e}")))?;
        let hash_hex = resp.into_result().map_err(|reason| SendError::RpcError { reason })?;
        let bytes = hex::decode(hash_hex.trim_start_matches("0x"))
            .map_err(|e| SendError::Internal(format!("malformed tx hash {hash_hex:?}: {e}")))?;
        if bytes.len() != 32 {
            return Err(SendError::Internal(format!(
                "tx hash wrong length: {} bytes",
                bytes.len()
            )));
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(&bytes);
        Ok(B256::from(out))
    }
}

#[async_trait]
impl EthSender for AlloyEthSender {
    async fn send_eth(
        &self,
        recipient: Address,
        amount_wei: U256,
    ) -> Result<B256, SendError> {
        let nonce = self.get_nonce().await?;
        let gas_price = self.get_gas_price().await?;

        // EIP-1559 sizing: use gas_price as the max_fee_per_gas
        // and a small tip for max_priority_fee_per_gas. Reth-shape
        // testnets usually have very low base fees so this
        // generous sizing keeps simple-transfer txs from getting
        // stuck. Tip is 1 gwei.
        let max_fee_per_gas = gas_price.max(2_000_000_000); // floor 2 gwei
        let max_priority_fee_per_gas = 1_000_000_000_u128.min(max_fee_per_gas);

        let mut tx = TxEip1559 {
            chain_id: self.chain_id,
            nonce,
            gas_limit: 21_000, // standard simple-transfer gas
            max_fee_per_gas,
            max_priority_fee_per_gas,
            to: TxKind::Call(recipient),
            value: amount_wei,
            access_list: Default::default(),
            input: Bytes::new(),
        };
        let signature = self
            .wallet
            .signer
            .sign_transaction_sync(&mut tx)
            .map_err(|e| SendError::Sign(e.to_string()))?;

        let signed = tx.into_signed(signature);
        let envelope = TxEnvelope::Eip1559(signed);
        let mut buf = Vec::with_capacity(256);
        envelope.encode(&mut buf);

        self.send_raw_transaction(&buf).await
    }
}

// === minimal JSON-RPC envelope ============================================

#[derive(Deserialize)]
struct RpcResponse<T> {
    #[serde(default)]
    result: Option<T>,
    #[serde(default)]
    error: Option<RpcError>,
}

#[derive(Deserialize)]
struct RpcError {
    #[allow(dead_code)]
    code: i64,
    message: String,
}

impl<T> RpcResponse<T> {
    fn into_result(self) -> Result<T, String> {
        if let Some(e) = self.error {
            return Err(e.message);
        }
        self.result.ok_or_else(|| "empty result".to_string())
    }
}

// === mock for tests ========================================================

#[cfg(test)]
#[derive(Debug, Clone)]
pub(crate) struct MockEthSender {
    behavior: MockBehavior,
}

#[cfg(test)]
#[derive(Debug, Clone)]
enum MockBehavior {
    AlwaysSucceed { fixed_hash: B256 },
    RpcError(String),
    RpcLayer(String),
}

#[cfg(test)]
impl MockEthSender {
    pub(crate) fn always_succeed_with(fixed_hash: B256) -> Self {
        Self {
            behavior: MockBehavior::AlwaysSucceed { fixed_hash },
        }
    }

    pub(crate) fn always_succeed() -> Self {
        Self::always_succeed_with(B256::repeat_byte(0xab))
    }

    pub(crate) fn rpc_error(reason: impl Into<String>) -> Self {
        Self {
            behavior: MockBehavior::RpcError(reason.into()),
        }
    }

    pub(crate) fn rpc_unreachable(reason: impl Into<String>) -> Self {
        Self {
            behavior: MockBehavior::RpcLayer(reason.into()),
        }
    }
}

#[cfg(test)]
#[async_trait]
impl EthSender for MockEthSender {
    async fn send_eth(
        &self,
        _recipient: Address,
        _amount_wei: U256,
    ) -> Result<B256, SendError> {
        match &self.behavior {
            MockBehavior::AlwaysSucceed { fixed_hash } => Ok(*fixed_hash),
            MockBehavior::RpcError(reason) => Err(SendError::RpcError {
                reason: reason.clone(),
            }),
            MockBehavior::RpcLayer(reason) => Err(SendError::Rpc(reason.clone())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mock sender always returns the configured hash.
    #[tokio::test]
    async fn mock_succeed_returns_fixed_hash() {
        let h = B256::repeat_byte(0xee);
        let s = MockEthSender::always_succeed_with(h);
        let out = s
            .send_eth(Address::ZERO, U256::from(1u64))
            .await
            .expect("ok");
        assert_eq!(out, h);
    }

    /// Mock-rpc-error → SendError::RpcError. Maps to 503 in
    /// the handler.
    #[tokio::test]
    async fn mock_rpc_error_returns_rpc_error_variant() {
        let s = MockEthSender::rpc_error("nonce too low");
        let err = s
            .send_eth(Address::ZERO, U256::from(1u64))
            .await
            .expect_err("must fail");
        match err {
            SendError::RpcError { reason } => assert_eq!(reason, "nonce too low"),
            other => panic!("expected RpcError, got {other}"),
        }
    }

    /// Mock-rpc-unreachable → SendError::Rpc. Also 503 in the
    /// handler.
    #[tokio::test]
    async fn mock_rpc_unreachable_returns_rpc_variant() {
        let s = MockEthSender::rpc_unreachable("connection refused");
        let err = s
            .send_eth(Address::ZERO, U256::from(1u64))
            .await
            .expect_err("must fail");
        assert!(matches!(err, SendError::Rpc(_)));
    }

    /// SendError Display includes the reason. Pinned because
    /// the drip handler embeds it in the 503 body.
    #[test]
    fn send_error_display_carries_reason() {
        assert!(
            SendError::Rpc("dns".into())
                .to_string()
                .contains("dns")
        );
        assert!(
            SendError::RpcError {
                reason: "nonce".into()
            }
            .to_string()
            .contains("nonce")
        );
        assert!(
            SendError::Sign("bad key".into())
                .to_string()
                .contains("bad key")
        );
    }

    /// `load_wallet` rejects empty passphrase before touching
    /// the filesystem. Same footgun guard as the validator
    /// keystore loader from T3a-2.
    #[test]
    fn load_wallet_rejects_empty_passphrase() {
        let err =
            load_wallet(std::path::Path::new("/nonexistent"), b"").expect_err("must fail");
        assert!(err.to_string().contains("must not be empty"));
        // We didn't touch the filesystem path — the error
        // didn't surface `/nonexistent` because we bailed first.
    }

    /// `load_wallet` errors on a missing file with a
    /// path-naming error. Same diagnosability requirement as
    /// the validator-keystore loader.
    #[test]
    fn load_wallet_missing_file_errors_with_path() {
        let err = load_wallet(std::path::Path::new("/no/such/keystore.json"), b"pw")
            .expect_err("must fail");
        let s = err.to_string();
        assert!(s.contains("decrypt wallet keystore"), "err: {s}");
        assert!(s.contains("/no/such/keystore.json"), "err: {s}");
    }

    /// JSON-RPC envelope round-trips both result and error
    /// variants. Pin against silent regressions when serde
    /// derive macros change.
    #[test]
    fn rpc_response_deserialize_paths() {
        let ok: RpcResponse<String> =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":1,"result":"0x1"}"#).unwrap();
        assert_eq!(ok.into_result().unwrap(), "0x1");

        let err: RpcResponse<String> = serde_json::from_str(
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"nonce too low"}}"#,
        )
        .unwrap();
        assert_eq!(err.into_result().unwrap_err(), "nonce too low");
    }
}

// `warn` is referenced by future EIP-1559 fee-sizing logic
// that will surface "RPC reports unusually high gas price"
// observations. Suppress the dead-code warning until that
// path lands.
#[allow(dead_code)]
fn _keep_warn_import_alive() {
    warn!("placeholder");
}

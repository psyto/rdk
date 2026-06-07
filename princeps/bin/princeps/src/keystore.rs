//! Stage T3a-2 of `docs/plans/v0-testnet-deploy.md` — encrypted
//! keystore envelope for the validator's Ed25519 private key.
//!
//! What this module provides:
//! - [`Keystore`] — the on-disk JSON envelope (version + kind +
//!   plaintext pubkey + scrypt-KDF params + AES-256-GCM cipher
//!   blob). serde-derived so it round-trips through serde_json.
//! - [`encrypt`] / [`decrypt`] — pure functions that take the
//!   passphrase as `&[u8]` so callers can decide how to source it
//!   (TTY prompt, file, env, etc.) without this module taking a
//!   stdin dependency.
//! - [`ScryptParams::production`] / [`ScryptParams::for_tests`] —
//!   parameter presets. Production is `N=131072, r=8, p=1` — the
//!   standard interactive-login profile recommended by RFC 7914.
//!   Tests use `N=16, r=8, p=1` so the test suite stays sub-second.
//!
//! Design choices worth pinning:
//! - **Pubkey is plaintext** in the envelope so an operator can
//!   identify which file is which without unlocking. After decrypt,
//!   the recovered private key's pubkey is checked against this
//!   field — if they disagree, the file is rejected as
//!   tampered/corrupted. This catches the otherwise-undetectable
//!   "swap the pubkey AND ciphertext" attack against AEAD.
//! - **AES-256-GCM** gives authenticated encryption — wrong
//!   passphrase or any single bit of ciphertext tampering fails
//!   closed at decrypt with a clean error, not undefined-behavior
//!   plaintext.
//! - **scrypt** is the standard KDF for keystore files (Ethereum
//!   V3, EIP-2335). Parameters are stored in the envelope so the
//!   defaults can change without orphaning old files.
//! - **Version + kind discriminators** so misidentifying a file
//!   (e.g. trying to load an oracle keystore as a validator
//!   keystore) fails fast with a clear error.

use std::path::Path;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use eyre::Context as _;
use informalsystems_malachitebft_signing_ed25519::PrivateKey;
use rand::rngs::OsRng;
use rand::RngCore as _;
use serde::{Deserialize, Serialize};

/// Bump on any breaking change to the envelope wire format.
/// Stored explicitly in the JSON so a future loader can reject
/// unknown versions rather than misparse them.
pub(crate) const KEYSTORE_VERSION: u32 = 1;

/// Discriminator so loading an oracle/operator keystore as a
/// validator keystore fails fast instead of silently producing
/// the wrong key type downstream.
pub(crate) const KEYSTORE_KIND_VALIDATOR: &str = "princeps-validator-keystore";

/// AES-256 key length in bytes — the scrypt output buffer size.
const AES_KEY_LEN: usize = 32;

/// AES-GCM nonce length in bytes (96-bit, the default for the
/// `aes-gcm` crate's `Aes256Gcm` alias).
const GCM_NONCE_LEN: usize = 12;

/// scrypt salt length in bytes. 16 bytes / 128 bits is the
/// standard length used by Ethereum V3 keystores.
const SCRYPT_SALT_LEN: usize = 16;

/// Standard filename for the encrypted keystore under `--data-dir`.
/// Distinct basename from `validator-key.json` so a host that holds
/// both files (e.g. mid-rotation) doesn't conflate them.
pub(crate) const VALIDATOR_KEYSTORE_FILENAME: &str = "validator-keystore.json";

/// On-disk JSON envelope. Fields are written in the order declared
/// here for diff-stability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Keystore {
    pub version: u32,
    pub kind: String,
    /// Hex-encoded Ed25519 public key (no `0x` prefix). Lives in
    /// plaintext so an operator can identify the file.
    pub pubkey: String,
    pub kdf: KdfEnvelope,
    pub cipher: CipherEnvelope,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct KdfEnvelope {
    /// KDF function name. Pinned to `"scrypt"` at v1.
    #[serde(rename = "fn")]
    pub function: String,
    /// scrypt N parameter (CPU/memory cost). Power of 2.
    pub n: u32,
    /// scrypt r parameter (block size).
    pub r: u32,
    /// scrypt p parameter (parallelization).
    pub p: u32,
    /// Hex-encoded salt.
    pub salt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CipherEnvelope {
    /// Cipher function name. Pinned to `"aes-256-gcm"` at v1.
    #[serde(rename = "fn")]
    pub function: String,
    /// Hex-encoded 96-bit nonce.
    pub nonce: String,
    /// Hex-encoded ciphertext || GCM tag.
    pub ciphertext: String,
}

/// scrypt parameter preset. Encapsulated so callers don't pick
/// the raw numbers — the production profile and the test profile
/// are the only two valid choices.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ScryptParams {
    pub n: u32,
    pub r: u32,
    pub p: u32,
}

impl ScryptParams {
    /// RFC 7914 interactive-login profile. Roughly 100-200 ms to
    /// derive on a modern laptop core; ~128 MiB working set.
    pub(crate) fn production() -> Self {
        Self {
            n: 131_072,
            r: 8,
            p: 1,
        }
    }

    /// Cheapest legal scrypt cost so the test suite stays
    /// sub-second. Same KDF, vastly weaker — must NEVER be used
    /// for keys that protect anything real. `pub(crate)` so the
    /// resolver tests in `validator_keygen` can also reach it.
    #[cfg(test)]
    pub(crate) fn for_tests() -> Self {
        Self {
            n: 16,
            r: 8,
            p: 1,
        }
    }

    fn to_scrypt_params(self) -> eyre::Result<scrypt::Params> {
        if !self.n.is_power_of_two() {
            eyre::bail!("scrypt N must be a power of two, got {}", self.n);
        }
        let log_n: u8 = self.n.trailing_zeros().try_into().map_err(|_| {
            eyre::eyre!("scrypt N too large: {}", self.n)
        })?;
        scrypt::Params::new(log_n, self.r, self.p, AES_KEY_LEN)
            .map_err(|e| eyre::eyre!("invalid scrypt params (n={}, r={}, p={}): {e}", self.n, self.r, self.p))
    }
}

/// Encrypt `private` under `passphrase` using the supplied scrypt
/// cost. Salt + nonce are freshly minted with OsRng on every call,
/// so the same key + passphrase always produces a different file.
//
// The `#[allow(deprecated)]` is for `Nonce::from_slice`. The
// deprecation is transitive — it fires because generic-array 1.x
// is pulled in elsewhere in the workspace, even though aes-gcm
// 0.10 itself still expects this call shape. Will resolve when
// aes-gcm releases a version built on generic-array 1.x.
#[allow(deprecated)]
pub(crate) fn encrypt(
    private: &PrivateKey,
    passphrase: &[u8],
    scrypt_params: ScryptParams,
) -> eyre::Result<Keystore> {
    if passphrase.is_empty() {
        eyre::bail!("passphrase must not be empty");
    }

    let mut salt = [0u8; SCRYPT_SALT_LEN];
    let mut nonce_bytes = [0u8; GCM_NONCE_LEN];
    OsRng.fill_bytes(&mut salt);
    OsRng.fill_bytes(&mut nonce_bytes);

    let params = scrypt_params.to_scrypt_params()?;
    let mut derived_key = [0u8; AES_KEY_LEN];
    scrypt::scrypt(passphrase, &salt, &params, &mut derived_key)
        .map_err(|e| eyre::eyre!("scrypt derivation failed: {e}"))?;

    let cipher = Aes256Gcm::new_from_slice(&derived_key)
        .map_err(|e| eyre::eyre!("invalid AES-256 key length: {e}"))?;
    let nonce = Nonce::from_slice(&nonce_bytes);
    let priv_bytes = private.inner().to_bytes();
    let ciphertext = cipher
        .encrypt(nonce, priv_bytes.as_ref())
        .map_err(|e| eyre::eyre!("AES-GCM encryption failed: {e}"))?;

    let public = private.public_key();
    Ok(Keystore {
        version: KEYSTORE_VERSION,
        kind: KEYSTORE_KIND_VALIDATOR.to_string(),
        pubkey: hex::encode(public.as_bytes()),
        kdf: KdfEnvelope {
            function: "scrypt".into(),
            n: scrypt_params.n,
            r: scrypt_params.r,
            p: scrypt_params.p,
            salt: hex::encode(salt),
        },
        cipher: CipherEnvelope {
            function: "aes-256-gcm".into(),
            nonce: hex::encode(nonce_bytes),
            ciphertext: hex::encode(&ciphertext),
        },
    })
}

/// Decrypt `keystore` with `passphrase`. Fails closed on any of:
/// unknown version, wrong kind, unknown KDF/cipher names, wrong
/// passphrase (AEAD tag mismatch), tampered ciphertext (AEAD tag
/// mismatch), or recovered pubkey not matching the plaintext
/// `pubkey` field.
#[allow(dead_code)] // production callers (reth-devnet load path) wire up in T3a-3
#[allow(deprecated)] // see comment on `encrypt` re: transitive generic-array deprecation
pub(crate) fn decrypt(keystore: &Keystore, passphrase: &[u8]) -> eyre::Result<PrivateKey> {
    if keystore.version != KEYSTORE_VERSION {
        eyre::bail!(
            "unsupported keystore version {} (this build expects {})",
            keystore.version,
            KEYSTORE_VERSION
        );
    }
    if keystore.kind != KEYSTORE_KIND_VALIDATOR {
        eyre::bail!(
            "wrong keystore kind {:?} (this build expects {:?})",
            keystore.kind,
            KEYSTORE_KIND_VALIDATOR
        );
    }
    if keystore.kdf.function != "scrypt" {
        eyre::bail!("unsupported kdf.fn {:?} (only \"scrypt\" supported)", keystore.kdf.function);
    }
    if keystore.cipher.function != "aes-256-gcm" {
        eyre::bail!(
            "unsupported cipher.fn {:?} (only \"aes-256-gcm\" supported)",
            keystore.cipher.function
        );
    }
    if passphrase.is_empty() {
        eyre::bail!("passphrase must not be empty");
    }

    let salt = hex::decode(&keystore.kdf.salt).map_err(|e| eyre::eyre!("invalid kdf.salt hex: {e}"))?;
    let nonce_bytes = hex::decode(&keystore.cipher.nonce)
        .map_err(|e| eyre::eyre!("invalid cipher.nonce hex: {e}"))?;
    if nonce_bytes.len() != GCM_NONCE_LEN {
        eyre::bail!(
            "cipher.nonce length {} bytes; expected {GCM_NONCE_LEN}",
            nonce_bytes.len()
        );
    }
    let ciphertext = hex::decode(&keystore.cipher.ciphertext)
        .map_err(|e| eyre::eyre!("invalid cipher.ciphertext hex: {e}"))?;

    let scrypt_params = ScryptParams {
        n: keystore.kdf.n,
        r: keystore.kdf.r,
        p: keystore.kdf.p,
    };
    let params = scrypt_params.to_scrypt_params()?;
    let mut derived_key = [0u8; AES_KEY_LEN];
    scrypt::scrypt(passphrase, &salt, &params, &mut derived_key)
        .map_err(|e| eyre::eyre!("scrypt derivation failed: {e}"))?;

    let cipher = Aes256Gcm::new_from_slice(&derived_key)
        .map_err(|e| eyre::eyre!("invalid AES-256 key length: {e}"))?;
    let nonce = Nonce::from_slice(&nonce_bytes);
    let plaintext = cipher
        .decrypt(nonce, ciphertext.as_ref())
        // aes-gcm's Error is opaque by design (it does not leak
        // which check failed). The user-facing error has to be
        // generic too — most likely "wrong passphrase", second
        // most likely "tampered file".
        .map_err(|_| eyre::eyre!("keystore decryption failed: wrong passphrase or tampered file"))?;

    if plaintext.len() != 32 {
        eyre::bail!(
            "decrypted payload is {} bytes; expected 32 (Ed25519 private key)",
            plaintext.len()
        );
    }
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(&plaintext);
    let private = PrivateKey::from(bytes);

    // Cross-check the recovered pubkey against the plaintext
    // field. Catches the "swap pubkey AND ciphertext together"
    // tamper that AEAD alone cannot detect.
    let expected_pubkey = hex::decode(&keystore.pubkey)
        .map_err(|e| eyre::eyre!("invalid pubkey hex: {e}"))?;
    let actual_pubkey = private.public_key();
    if expected_pubkey != actual_pubkey.as_bytes() {
        eyre::bail!(
            "decrypted private key does not match plaintext pubkey field — file may be corrupted or tampered with"
        );
    }

    Ok(private)
}

/// Persist a [`Keystore`] to `path` as pretty-printed JSON with
/// 0o600 perms on Unix. Refuses to overwrite an existing file
/// unless `force` is true — same "typo can't nuke validator
/// identity" guard as [`crate::validator_keygen::generate_fresh`].
pub(crate) fn write_to_path(keystore: &Keystore, path: &Path, force: bool) -> eyre::Result<()> {
    if path.exists() && !force {
        eyre::bail!(
            "keystore already exists at {} (pass --force to overwrite)",
            path.display()
        );
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create keystore parent dir {}", parent.display()))?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(keystore)?)
        .with_context(|| format!("write keystore to {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod 0600 {}", path.display()))?;
    }
    Ok(())
}

/// Read and JSON-parse a keystore file. Does not decrypt — that's
/// [`decrypt`]'s job.
#[allow(dead_code)] // load-side wiring lands in T3a-3
pub(crate) fn read_from_path(path: &Path) -> eyre::Result<Keystore> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("read keystore at {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .map_err(|e| eyre::eyre!("malformed keystore at {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::OsRng;

    fn fresh_private() -> PrivateKey {
        PrivateKey::generate(OsRng)
    }

    /// Round-trip: encrypt → decrypt with the right passphrase
    /// recovers the same private key bytes.
    #[test]
    fn encrypt_decrypt_round_trip() {
        let private = fresh_private();
        let passphrase = b"correct horse battery staple";

        let keystore = encrypt(&private, passphrase, ScryptParams::for_tests())
            .expect("encrypt");
        let recovered = decrypt(&keystore, passphrase).expect("decrypt");

        assert_eq!(
            private.inner().to_bytes(),
            recovered.inner().to_bytes(),
            "decrypted private bytes must match original"
        );
    }

    /// Wrong passphrase → clean error, not a panic or garbage
    /// plaintext. AEAD is doing its job.
    #[test]
    fn decrypt_with_wrong_passphrase_fails() {
        let keystore = encrypt(&fresh_private(), b"right", ScryptParams::for_tests())
            .expect("encrypt");
        let err = decrypt(&keystore, b"wrong").expect_err("must fail");
        assert!(
            err.to_string().contains("wrong passphrase or tampered file"),
            "error must blame passphrase/tamper: {err}"
        );
    }

    /// Flipping a single bit of the ciphertext → AEAD tag
    /// mismatch → clean error. Same surface as wrong-passphrase
    /// by design (aes-gcm doesn't distinguish which check failed,
    /// to avoid leaking signal).
    #[test]
    fn tampered_ciphertext_fails_to_decrypt() {
        let mut keystore = encrypt(&fresh_private(), b"pw", ScryptParams::for_tests())
            .expect("encrypt");
        let mut ct = hex::decode(&keystore.cipher.ciphertext).unwrap();
        ct[0] ^= 0x01;
        keystore.cipher.ciphertext = hex::encode(&ct);

        let err = decrypt(&keystore, b"pw").expect_err("must fail");
        assert!(err.to_string().contains("wrong passphrase or tampered file"));
    }

    /// Swap the plaintext pubkey to a different value, but leave
    /// the ciphertext alone. The recovered private's pubkey
    /// won't match the field, and the post-decrypt cross-check
    /// catches it. AEAD alone could not — the ciphertext is
    /// still valid.
    #[test]
    fn tampered_pubkey_field_fails_post_decrypt_check() {
        let mut keystore = encrypt(&fresh_private(), b"pw", ScryptParams::for_tests())
            .expect("encrypt");
        // Replace pubkey with a different validly-sized hex blob.
        let bogus = fresh_private().public_key();
        keystore.pubkey = hex::encode(bogus.as_bytes());

        let err = decrypt(&keystore, b"pw").expect_err("must fail");
        assert!(
            err.to_string().contains("does not match plaintext pubkey field"),
            "expected pubkey-mismatch error, got: {err}"
        );
    }

    /// Empty passphrase rejected on the encrypt side — prevents
    /// the "I'll set a passphrase later" footgun.
    #[test]
    fn encrypt_rejects_empty_passphrase() {
        let err = encrypt(&fresh_private(), b"", ScryptParams::for_tests())
            .expect_err("must fail");
        assert!(err.to_string().contains("passphrase must not be empty"));
    }

    /// Empty passphrase rejected on decrypt too — same footgun
    /// from the other direction.
    #[test]
    fn decrypt_rejects_empty_passphrase() {
        let keystore = encrypt(&fresh_private(), b"pw", ScryptParams::for_tests())
            .expect("encrypt");
        let err = decrypt(&keystore, b"").expect_err("must fail");
        assert!(err.to_string().contains("passphrase must not be empty"));
    }

    /// Version mismatch fails fast with a recognizable error.
    /// Future v2 readers must reject v1 files cleanly.
    #[test]
    fn decrypt_rejects_unknown_version() {
        let mut keystore = encrypt(&fresh_private(), b"pw", ScryptParams::for_tests())
            .expect("encrypt");
        keystore.version = 999;
        let err = decrypt(&keystore, b"pw").expect_err("must fail");
        assert!(err.to_string().contains("unsupported keystore version 999"));
    }

    /// Wrong-kind discriminator: an oracle keystore (hypothetical)
    /// loaded as a validator keystore must fail.
    #[test]
    fn decrypt_rejects_wrong_kind() {
        let mut keystore = encrypt(&fresh_private(), b"pw", ScryptParams::for_tests())
            .expect("encrypt");
        keystore.kind = "princeps-oracle-keystore".into();
        let err = decrypt(&keystore, b"pw").expect_err("must fail");
        assert!(err.to_string().contains("wrong keystore kind"));
    }

    /// JSON envelope round-trips via serde_json without losing
    /// any field — the on-disk format is what we think it is.
    #[test]
    fn json_round_trip_preserves_fields() {
        let keystore = encrypt(&fresh_private(), b"pw", ScryptParams::for_tests())
            .expect("encrypt");
        let json = serde_json::to_string_pretty(&keystore).unwrap();
        let parsed: Keystore = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.version, keystore.version);
        assert_eq!(parsed.kind, keystore.kind);
        assert_eq!(parsed.pubkey, keystore.pubkey);
        assert_eq!(parsed.kdf.function, keystore.kdf.function);
        assert_eq!(parsed.kdf.n, keystore.kdf.n);
        assert_eq!(parsed.kdf.r, keystore.kdf.r);
        assert_eq!(parsed.kdf.p, keystore.kdf.p);
        assert_eq!(parsed.kdf.salt, keystore.kdf.salt);
        assert_eq!(parsed.cipher.function, keystore.cipher.function);
        assert_eq!(parsed.cipher.nonce, keystore.cipher.nonce);
        assert_eq!(parsed.cipher.ciphertext, keystore.cipher.ciphertext);
    }

    /// Same passphrase + same key encrypted twice produces
    /// DIFFERENT ciphertext (fresh salt + fresh nonce per call).
    /// Catches any future regression where salt/nonce becomes
    /// deterministic.
    #[test]
    fn two_encryptions_use_distinct_salt_and_nonce() {
        let private = fresh_private();
        let a = encrypt(&private, b"pw", ScryptParams::for_tests()).unwrap();
        let b = encrypt(&private, b"pw", ScryptParams::for_tests()).unwrap();
        assert_ne!(a.kdf.salt, b.kdf.salt);
        assert_ne!(a.cipher.nonce, b.cipher.nonce);
        assert_ne!(a.cipher.ciphertext, b.cipher.ciphertext);
        // Both still decrypt to the same key.
        let r_a = decrypt(&a, b"pw").unwrap();
        let r_b = decrypt(&b, b"pw").unwrap();
        assert_eq!(r_a.inner().to_bytes(), r_b.inner().to_bytes());
    }

    /// write_to_path / read_from_path round-trip via disk; the
    /// file lands with 0o600 perms on Unix.
    #[test]
    fn write_then_read_round_trip() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("validator-keystore.json");

        let keystore = encrypt(&fresh_private(), b"pw", ScryptParams::for_tests())
            .expect("encrypt");
        write_to_path(&keystore, &path, false).expect("write");
        let loaded = read_from_path(&path).expect("read");

        // Decrypt round-trip via the loaded copy.
        let recovered = decrypt(&loaded, b"pw").expect("decrypt");
        let original = decrypt(&keystore, b"pw").expect("decrypt original");
        assert_eq!(
            recovered.inner().to_bytes(),
            original.inner().to_bytes()
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    /// `write_to_path` refuses to clobber an existing file unless
    /// `--force` is set — mirrors the gen-keys guard.
    #[test]
    fn write_to_path_refuses_overwrite_without_force() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("validator-keystore.json");

        let keystore = encrypt(&fresh_private(), b"pw", ScryptParams::for_tests()).unwrap();
        write_to_path(&keystore, &path, false).expect("first write");
        let err = write_to_path(&keystore, &path, false).expect_err("second write must fail");
        assert!(err.to_string().contains("already exists"));
        assert!(err.to_string().contains("--force"));

        // With --force, replaces.
        write_to_path(&keystore, &path, true).expect("force write");
    }
}

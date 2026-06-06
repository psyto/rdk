//! Stage T3a-1 of `docs/plans/v0-testnet-deploy.md` — validator
//! key generation lifted out of the inline `run_reth_devnet` boot
//! path into a dedicated module + standalone subcommand.
//!
//! What changes for the operator: keys can now be minted on an
//! air-gapped host via `princeps validator gen-keys` without
//! booting the Reth + Malachite stack. The on-disk format is
//! unchanged from Stage 13h — same `PrincepsPrivateKeyFile` JSON,
//! same `validator-pubkey.hex` sidecar, same 0o600 perms on Unix.
//! T3a-2 will swap the plaintext JSON for an encrypted keystore
//! with a stdin passphrase.
//!
//! The reth-devnet boot path still calls into this module so the
//! generate-on-first-boot ergonomic is preserved.

use std::path::{Path, PathBuf};

use eyre::Context as _;
use informalsystems_malachitebft_signing_ed25519::{PrivateKey, PublicKey};
use princeps_consensus::PrincepsPrivateKeyFile;
use rand::rngs::OsRng;

/// Standard filename for the validator key file under `--data-dir`.
/// Held as a constant so the subcommand and the boot path can't
/// drift on the basename.
pub(crate) const VALIDATOR_KEY_FILENAME: &str = "validator-key.json";

/// Standard filename for the public-key sidecar under `--data-dir`.
pub(crate) const VALIDATOR_PUBKEY_SIDECAR_FILENAME: &str = "validator-pubkey.hex";

/// Outcome of [`gen_or_load`] — distinguishes a freshly minted key
/// from one read off disk. Used by the boot path to print a status
/// line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyStatus {
    Loaded,
    Generated,
}

impl std::fmt::Display for KeyStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Loaded => f.write_str("loaded"),
            Self::Generated => f.write_str("generated"),
        }
    }
}

/// Load the validator key at `key_path` if it exists, otherwise
/// generate a fresh one, persist it, and apply 0o600 perms on Unix.
/// This is the `run_reth_devnet` entry point — preserves the
/// Stage 13h "consecutive runs use the same identity" property
/// that Malachite WAL reuse depends on.
///
/// Errors:
/// - the file exists but is malformed JSON
/// - the parent directory cannot be created
/// - the file cannot be written
pub(crate) fn gen_or_load(key_path: &Path) -> eyre::Result<(PrivateKey, KeyStatus)> {
    if key_path.exists() {
        let bytes = std::fs::read(key_path)
            .with_context(|| format!("read validator key at {}", key_path.display()))?;
        let file: PrincepsPrivateKeyFile = serde_json::from_slice(&bytes)
            .map_err(|e| eyre::eyre!("malformed validator key at {}: {e}", key_path.display()))?;
        Ok((file.into_private_key(), KeyStatus::Loaded))
    } else {
        let fresh = mint_and_write(key_path)?;
        Ok((fresh, KeyStatus::Generated))
    }
}

/// Generate a fresh key and persist it to `key_path`. If
/// `key_path` already exists and `force` is false, this errors
/// without touching the existing file — protects an operator
/// from a single mistyped command nuking an established
/// validator identity. With `force = true` the existing file is
/// overwritten.
///
/// Returned tuple is `(private, public)` so the caller can echo
/// the pubkey to stdout for the operator to paste into the
/// validator-set JSON.
pub(crate) fn generate_fresh(
    key_path: &Path,
    force: bool,
) -> eyre::Result<(PrivateKey, PublicKey)> {
    if key_path.exists() && !force {
        eyre::bail!(
            "validator key already exists at {} (pass --force to overwrite)",
            key_path.display()
        );
    }
    let private = mint_and_write(key_path)?;
    let public = private.public_key();
    Ok((private, public))
}

/// Write the `validator-pubkey.hex` sidecar next to the key file.
/// Idempotent — overwrites every call so a stale sidecar from a
/// rotated key can't drift. Returns the sidecar's full path so
/// the caller can print it.
pub(crate) fn write_pubkey_sidecar(parent_dir: &Path, public: &PublicKey) -> eyre::Result<PathBuf> {
    std::fs::create_dir_all(parent_dir).with_context(|| {
        format!("create sidecar parent dir {}", parent_dir.display())
    })?;
    let sidecar = parent_dir.join(VALIDATOR_PUBKEY_SIDECAR_FILENAME);
    let pubkey_hex = hex::encode(public.as_bytes());
    std::fs::write(&sidecar, format!("{pubkey_hex}\n"))
        .with_context(|| format!("write sidecar {}", sidecar.display()))?;
    Ok(sidecar)
}

/// Mint a fresh key with `OsRng`, persist it as
/// `PrincepsPrivateKeyFile` JSON at `key_path`, and chmod 0o600
/// on Unix. Shared between [`gen_or_load`] (called on first boot)
/// and [`generate_fresh`] (called from the subcommand).
fn mint_and_write(key_path: &Path) -> eyre::Result<PrivateKey> {
    let fresh = PrivateKey::generate(OsRng);
    let file = PrincepsPrivateKeyFile::from_private_key(&fresh);
    if let Some(parent) = key_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create key parent dir {}", parent.display()))?;
    }
    std::fs::write(key_path, serde_json::to_vec_pretty(&file)?)
        .with_context(|| format!("write validator key to {}", key_path.display()))?;
    // Owner-readable only — minor hardening so a shared-filesystem
    // mishap doesn't surface the secret to other users on the host.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(key_path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod 0600 {}", key_path.display()))?;
    }
    Ok(fresh)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn temp_key_path() -> (TempDir, PathBuf) {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("validator-key.json");
        (dir, path)
    }

    /// Fresh-gen path on a clean dir produces a `Generated` status
    /// and a file that round-trips back through `gen_or_load` as
    /// the same private key — the property Malachite WAL reuse
    /// depends on (Stage 13h).
    #[test]
    fn gen_or_load_is_idempotent_across_calls() {
        let (_dir, path) = temp_key_path();
        let (k1, s1) = gen_or_load(&path).expect("first call");
        assert_eq!(s1, KeyStatus::Generated);

        let (k2, s2) = gen_or_load(&path).expect("second call");
        assert_eq!(s2, KeyStatus::Loaded);

        // Private-key bytes round-trip via the on-disk file.
        assert_eq!(
            PrincepsPrivateKeyFile::from_private_key(&k1).bytes,
            PrincepsPrivateKeyFile::from_private_key(&k2).bytes,
        );
    }

    /// Malformed JSON at `key_path` surfaces a clear error rather
    /// than panicking or silently regenerating — silently
    /// regenerating would mean a corrupted file resets a
    /// validator's identity, which is worse than a hard failure.
    #[test]
    fn gen_or_load_rejects_malformed_json() {
        let (_dir, path) = temp_key_path();
        std::fs::write(&path, b"not json").unwrap();

        let err = gen_or_load(&path).expect_err("malformed JSON should error");
        assert!(
            err.to_string().contains("malformed validator key"),
            "error should mention malformed: {err}"
        );
    }

    /// `generate_fresh` refuses to clobber an existing file when
    /// `force` is false — protects against the typo-nukes-validator
    /// failure mode.
    #[test]
    fn generate_fresh_refuses_overwrite_without_force() {
        let (_dir, path) = temp_key_path();
        let (_k1, _pk1) = generate_fresh(&path, /* force = */ false).expect("first call");

        let err = generate_fresh(&path, false).expect_err("second call should error");
        assert!(
            err.to_string().contains("already exists"),
            "error should mention existing file: {err}"
        );
        assert!(
            err.to_string().contains("--force"),
            "error should hint at --force: {err}"
        );
    }

    /// With `force = true` the second call replaces the file and
    /// returns a different key — confirms force actually rotates
    /// rather than silently no-op'ing.
    #[test]
    fn generate_fresh_with_force_rotates_key() {
        let (_dir, path) = temp_key_path();
        let (k1, _) = generate_fresh(&path, false).expect("first call");
        let (k2, _) = generate_fresh(&path, true).expect("force call");

        // OsRng makes a collision astronomically unlikely; treat
        // identical bytes as a real failure.
        assert_ne!(
            PrincepsPrivateKeyFile::from_private_key(&k1).bytes,
            PrincepsPrivateKeyFile::from_private_key(&k2).bytes,
            "force should mint a new key, not reuse the old one"
        );
    }

    /// On Unix the key file lands with 0o600 perms — the
    /// `mint_and_write` hardening from Stage 13h, now exercised
    /// by an explicit test.
    #[cfg(unix)]
    #[test]
    fn fresh_key_file_has_owner_only_perms() {
        use std::os::unix::fs::PermissionsExt as _;
        let (_dir, path) = temp_key_path();
        let _ = gen_or_load(&path).expect("gen");

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        // Mask off file-type bits; only the bottom 9 perm bits matter.
        assert_eq!(mode & 0o777, 0o600, "expected 0o600, got {:o}", mode & 0o777);
    }

    /// Sidecar lands at the conventional filename with the hex
    /// pubkey + trailing newline. Catches any future regression
    /// where the format drifts (downstream tools grep this file).
    #[test]
    fn write_pubkey_sidecar_emits_hex_newline() {
        let dir = TempDir::new().unwrap();
        let (priv_key, _) = gen_or_load(&dir.path().join("validator-key.json")).unwrap();
        let public = priv_key.public_key();

        let sidecar = write_pubkey_sidecar(dir.path(), &public).expect("write sidecar");
        assert_eq!(
            sidecar.file_name().and_then(|s| s.to_str()),
            Some(VALIDATOR_PUBKEY_SIDECAR_FILENAME),
        );

        let body = std::fs::read_to_string(&sidecar).unwrap();
        assert!(body.ends_with('\n'), "sidecar must end with newline");
        let hex_only = body.trim_end();
        assert_eq!(hex_only.len(), 64, "Ed25519 pubkey hex is 64 chars");
        assert!(
            hex_only.chars().all(|c| c.is_ascii_hexdigit()),
            "sidecar body must be hex: {hex_only:?}"
        );
        // And it must match the actual pubkey bytes.
        assert_eq!(hex_only, hex::encode(public.as_bytes()));
    }
}

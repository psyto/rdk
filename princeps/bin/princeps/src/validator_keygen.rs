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

/// Outcome of [`gen_or_load`] / [`load_or_generate_validator_identity`].
/// `LoadedPlaintext` and `LoadedKeystore` carry the on-disk format
/// distinction so the boot-line print can flag operators still on
/// the un-hardened plaintext path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyStatus {
    LoadedPlaintext,
    LoadedKeystore,
    Generated,
}

impl std::fmt::Display for KeyStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LoadedPlaintext => f.write_str("loaded (plaintext)"),
            Self::LoadedKeystore => f.write_str("loaded (keystore)"),
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
        Ok((file.into_private_key(), KeyStatus::LoadedPlaintext))
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

// === T3a-3: load-side resolver ===============================================
//
// Bundle the boot-time "which on-disk format does this host have"
// decision behind a single resolver call. Today `run_reth_devnet`
// asks for plaintext only; with T3a-2's encrypted keystore on disk
// the same boot path needs to prefer the keystore + prompt for a
// passphrase, fail closed on parse/decrypt error, but stay
// compatible with the plaintext-only devnet flow when neither
// keystore-related flag is given.

/// Where the boot-time passphrase comes from. The TTY variant is
/// the default for an interactive operator; the other two cover
/// systemd / CI / scripted provisioning.
#[derive(Debug, Clone)]
pub(crate) enum PassphraseSource {
    /// Prompt on /dev/tty via rpassword. Errors if stdin is not a
    /// TTY rather than blocking on a read that no one will satisfy.
    Tty,
    /// Read from `path`. Single line; trailing `\n` or `\r\n` is
    /// stripped. Empty content rejected.
    File(PathBuf),
    /// Read one line from stdin. Same trimming/empty rules as File.
    Stdin,
}

/// Inputs to [`load_or_generate_validator_identity`].
#[derive(Debug)]
pub(crate) struct LoadOrGenerateOpts<'a> {
    /// The reth-devnet data directory. Resolved keystore + plaintext
    /// paths sit underneath here unless `keystore_path_override` says
    /// otherwise.
    pub data_dir: &'a Path,
    /// Explicit `--validator-keystore` override. When `Some(p)` and
    /// the path does not exist, the resolver bails — the operator
    /// asked for a specific file and we won't silently fall through
    /// to plaintext or generate-fresh.
    pub keystore_path_override: Option<&'a Path>,
    /// How to source the passphrase when a keystore is being decrypted.
    pub passphrase: PassphraseSource,
}

/// Resolver output. `warnings` is a list of operator-facing strings
/// the caller should surface (typically via `eprintln!`) — the
/// resolver itself doesn't print so test code can assert against
/// the contents.
#[derive(Debug)]
pub(crate) struct ValidatorIdentity {
    pub private: PrivateKey,
    pub status: KeyStatus,
    pub warnings: Vec<String>,
}

/// Boot-time resolver. Precedence:
/// 1. `--validator-keystore <path>` explicit → must exist; load + decrypt.
/// 2. `<data_dir>/validator-keystore.json` exists → load + decrypt; warn
///    if a plaintext file is also present.
/// 3. `<data_dir>/validator-key.json` exists → load plaintext.
/// 4. Neither file present → generate fresh plaintext.
///
/// Fails closed on any keystore parse or decrypt error — silently
/// falling through to plaintext or generate-fresh would drop the
/// validator's intended identity.
pub(crate) fn load_or_generate_validator_identity(
    opts: &LoadOrGenerateOpts<'_>,
) -> eyre::Result<ValidatorIdentity> {
    let plaintext_path = opts.data_dir.join(VALIDATOR_KEY_FILENAME);
    let (keystore_path, keystore_is_explicit) = match opts.keystore_path_override {
        Some(p) => (p.to_path_buf(), true),
        None => (
            opts.data_dir.join(crate::keystore::VALIDATOR_KEYSTORE_FILENAME),
            false,
        ),
    };

    if keystore_is_explicit && !keystore_path.exists() {
        eyre::bail!(
            "--validator-keystore {} does not exist",
            keystore_path.display()
        );
    }

    if keystore_path.exists() {
        let mut warnings = Vec::new();
        if plaintext_path.exists() {
            warnings.push(format!(
                "both {} and {} exist; using the encrypted keystore. \
                 The plaintext file is being ignored — consider removing \
                 or archiving it to avoid ambiguity on the next rotation.",
                keystore_path.display(),
                plaintext_path.display(),
            ));
        }
        let doc = crate::keystore::read_from_path(&keystore_path)?;
        let passphrase = read_passphrase_from_source(&opts.passphrase)?;
        let private = crate::keystore::decrypt(&doc, passphrase.as_bytes())?;
        Ok(ValidatorIdentity {
            private,
            status: KeyStatus::LoadedKeystore,
            warnings,
        })
    } else {
        let (private, status) = gen_or_load(&plaintext_path)?;
        Ok(ValidatorIdentity {
            private,
            status,
            warnings: Vec::new(),
        })
    }
}

/// Pull a passphrase from one of the three supported sources. Empty
/// passphrases are rejected at this layer so the caller doesn't have
/// to re-check; an empty value means the source is misconfigured
/// rather than a deliberate choice.
fn read_passphrase_from_source(source: &PassphraseSource) -> eyre::Result<String> {
    match source {
        PassphraseSource::Tty => {
            // Explicit IsTerminal check before calling rpassword so
            // the no-TTY case produces a deterministic error instead
            // of rpassword's fallback behavior (which can vary by
            // platform and may try to read from stdin silently).
            use std::io::IsTerminal as _;
            if !std::io::stdin().is_terminal() {
                eyre::bail!(
                    "validator keystore requires a passphrase but stdin is not a TTY; \
                     pass --validator-keystore-passphrase-file <path> or \
                     --validator-keystore-passphrase-stdin"
                );
            }
            let pw = rpassword::prompt_password("validator keystore passphrase: ")
                .map_err(|e| eyre::eyre!("read passphrase from tty: {e}"))?;
            if pw.is_empty() {
                eyre::bail!("passphrase must not be empty");
            }
            Ok(pw)
        }
        PassphraseSource::File(path) => {
            let bytes = std::fs::read(path).with_context(|| {
                format!("read passphrase file {}", path.display())
            })?;
            let text = String::from_utf8(bytes)
                .map_err(|e| eyre::eyre!("passphrase file must be valid UTF-8: {e}"))?;
            let trimmed = text.trim_end_matches(['\n', '\r']).to_string();
            if trimmed.is_empty() {
                eyre::bail!("passphrase file {} is empty", path.display());
            }
            Ok(trimmed)
        }
        PassphraseSource::Stdin => {
            let mut buf = String::new();
            std::io::stdin()
                .read_line(&mut buf)
                .map_err(|e| eyre::eyre!("read passphrase from stdin: {e}"))?;
            let trimmed = buf.trim_end_matches(['\n', '\r']).to_string();
            if trimmed.is_empty() {
                eyre::bail!("passphrase read from stdin was empty");
            }
            Ok(trimmed)
        }
    }
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
        assert_eq!(s2, KeyStatus::LoadedPlaintext);

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

    // === T3a-3 resolver tests ===============================================

    /// Helper: write a freshly-encrypted keystore at `<dir>/validator-keystore.json`
    /// using test-cost scrypt params (so the suite stays sub-second).
    fn write_test_keystore(dir: &Path, passphrase: &[u8]) -> PrivateKey {
        let private = PrivateKey::generate(OsRng);
        let doc = crate::keystore::encrypt(
            &private,
            passphrase,
            crate::keystore::ScryptParams::for_tests(),
        )
        .expect("encrypt");
        let path = dir.join(crate::keystore::VALIDATOR_KEYSTORE_FILENAME);
        crate::keystore::write_to_path(&doc, &path, false).expect("write");
        private
    }

    /// Helper: drop a passphrase file at `<dir>/pw.txt`.
    fn write_passphrase_file(dir: &Path, passphrase: &str) -> PathBuf {
        let path = dir.join("pw.txt");
        std::fs::write(&path, passphrase).unwrap();
        path
    }

    /// Keystore-only present → resolver returns LoadedKeystore and
    /// the decrypted bytes match what we encrypted.
    #[test]
    fn resolver_keystore_only_loads_via_decrypt() {
        let dir = TempDir::new().unwrap();
        let pw = "correct horse";
        let original = write_test_keystore(dir.path(), pw.as_bytes());
        let pw_path = write_passphrase_file(dir.path(), pw);

        let identity = load_or_generate_validator_identity(&LoadOrGenerateOpts {
            data_dir: dir.path(),
            keystore_path_override: None,
            passphrase: PassphraseSource::File(pw_path),
        })
        .expect("resolve");

        assert_eq!(identity.status, KeyStatus::LoadedKeystore);
        assert!(identity.warnings.is_empty(), "no warnings expected when only keystore present");
        assert_eq!(
            original.inner().to_bytes(),
            identity.private.inner().to_bytes(),
        );
    }

    /// Plaintext-only present → returns LoadedPlaintext, no passphrase
    /// read (so any passphrase source is fine; we pass File pointing at
    /// nothing to prove it's never read).
    #[test]
    fn resolver_plaintext_only_returns_existing_plaintext() {
        let dir = TempDir::new().unwrap();
        let plaintext_path = dir.path().join(VALIDATOR_KEY_FILENAME);
        let (first, _) = gen_or_load(&plaintext_path).expect("seed plaintext");

        let identity = load_or_generate_validator_identity(&LoadOrGenerateOpts {
            data_dir: dir.path(),
            keystore_path_override: None,
            passphrase: PassphraseSource::File(PathBuf::from("/this/file/does/not/exist")),
        })
        .expect("resolve");

        assert_eq!(identity.status, KeyStatus::LoadedPlaintext);
        assert!(identity.warnings.is_empty());
        assert_eq!(
            first.inner().to_bytes(),
            identity.private.inner().to_bytes(),
        );
    }

    /// Neither file present → resolver generates fresh plaintext, no
    /// passphrase prompt.
    #[test]
    fn resolver_neither_file_generates_fresh_plaintext() {
        let dir = TempDir::new().unwrap();

        let identity = load_or_generate_validator_identity(&LoadOrGenerateOpts {
            data_dir: dir.path(),
            keystore_path_override: None,
            // Even an invalid path is OK — never consulted.
            passphrase: PassphraseSource::File(PathBuf::from("/nope")),
        })
        .expect("resolve");

        assert_eq!(identity.status, KeyStatus::Generated);
        assert!(identity.warnings.is_empty());
        // And the plaintext file was created.
        assert!(dir.path().join(VALIDATOR_KEY_FILENAME).exists());
    }

    /// Both files present → keystore wins, plaintext is left
    /// untouched on disk, and a warning is surfaced naming both paths.
    #[test]
    fn resolver_both_files_present_prefers_keystore_with_warning() {
        let dir = TempDir::new().unwrap();
        let pw = "pw";
        let _keystore_priv = write_test_keystore(dir.path(), pw.as_bytes());
        let pw_path = write_passphrase_file(dir.path(), pw);
        // Also drop a plaintext file in the same dir.
        let plaintext_path = dir.path().join(VALIDATOR_KEY_FILENAME);
        let (_plain_priv, _) = gen_or_load(&plaintext_path).expect("seed plaintext");

        let identity = load_or_generate_validator_identity(&LoadOrGenerateOpts {
            data_dir: dir.path(),
            keystore_path_override: None,
            passphrase: PassphraseSource::File(pw_path),
        })
        .expect("resolve");

        assert_eq!(identity.status, KeyStatus::LoadedKeystore);
        assert_eq!(identity.warnings.len(), 1);
        let w = &identity.warnings[0];
        assert!(w.contains("validator-keystore.json"), "warning: {w}");
        assert!(w.contains("validator-key.json"), "warning: {w}");
        // Plaintext file remains on disk — we did NOT delete it.
        assert!(plaintext_path.exists());
    }

    /// Explicit --validator-keystore <path> that doesn't exist → bail.
    /// Refusing to silently fall through preserves the operator's intent.
    #[test]
    fn resolver_explicit_keystore_missing_bails() {
        let dir = TempDir::new().unwrap();
        let bogus = dir.path().join("nope.json");

        let err = load_or_generate_validator_identity(&LoadOrGenerateOpts {
            data_dir: dir.path(),
            keystore_path_override: Some(&bogus),
            passphrase: PassphraseSource::File(PathBuf::from("/x")),
        })
        .expect_err("must bail");

        assert!(
            err.to_string().contains("does not exist"),
            "error: {err}"
        );
    }

    /// Keystore exists but the passphrase is wrong → resolver
    /// surfaces the decrypt error and does NOT fall through.
    #[test]
    fn resolver_wrong_passphrase_fails_closed() {
        let dir = TempDir::new().unwrap();
        let _ = write_test_keystore(dir.path(), b"right");
        let pw_path = write_passphrase_file(dir.path(), "wrong");

        let err = load_or_generate_validator_identity(&LoadOrGenerateOpts {
            data_dir: dir.path(),
            keystore_path_override: None,
            passphrase: PassphraseSource::File(pw_path),
        })
        .expect_err("must fail");

        assert!(
            err.to_string().contains("wrong passphrase or tampered file"),
            "error: {err}"
        );
    }

    /// Keystore exists but is malformed JSON → fail closed.
    #[test]
    fn resolver_malformed_keystore_fails_closed() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join(crate::keystore::VALIDATOR_KEYSTORE_FILENAME),
            b"not json",
        )
        .unwrap();
        let pw_path = write_passphrase_file(dir.path(), "anything");

        let err = load_or_generate_validator_identity(&LoadOrGenerateOpts {
            data_dir: dir.path(),
            keystore_path_override: None,
            passphrase: PassphraseSource::File(pw_path),
        })
        .expect_err("must fail");

        assert!(err.to_string().contains("malformed keystore"));
    }

    /// Empty passphrase file → bail with a path-naming error. Catches
    /// the misconfigured-systemd-credential footgun.
    #[test]
    fn resolver_empty_passphrase_file_bails() {
        let dir = TempDir::new().unwrap();
        let _ = write_test_keystore(dir.path(), b"pw");
        let pw_path = write_passphrase_file(dir.path(), "");

        let err = load_or_generate_validator_identity(&LoadOrGenerateOpts {
            data_dir: dir.path(),
            keystore_path_override: None,
            passphrase: PassphraseSource::File(pw_path.clone()),
        })
        .expect_err("must fail");

        let s = err.to_string();
        assert!(s.contains("is empty"), "error: {err}");
        assert!(s.contains(&pw_path.display().to_string()), "error names path: {err}");
    }

    /// Tty source under cargo test (no TTY) → bail with the
    /// "pass --passphrase-file or --passphrase-stdin" hint, so an
    /// operator running under systemd without an explicit source
    /// gets a clear pointer.
    #[test]
    fn resolver_tty_without_terminal_bails_with_hint() {
        let dir = TempDir::new().unwrap();
        let _ = write_test_keystore(dir.path(), b"pw");

        let err = load_or_generate_validator_identity(&LoadOrGenerateOpts {
            data_dir: dir.path(),
            keystore_path_override: None,
            passphrase: PassphraseSource::Tty,
        })
        .expect_err("must fail");

        let s = err.to_string();
        assert!(s.contains("not a TTY"), "error: {err}");
        assert!(s.contains("--validator-keystore-passphrase-file"), "error: {err}");
        assert!(s.contains("--validator-keystore-passphrase-stdin"), "error: {err}");
    }

    /// Passphrase file with trailing \n round-trips (systemd-credential
    /// files end in newline by default).
    #[test]
    fn resolver_passphrase_file_strips_trailing_newline() {
        let dir = TempDir::new().unwrap();
        let _ = write_test_keystore(dir.path(), b"pw");
        let pw_path = write_passphrase_file(dir.path(), "pw\n");

        let identity = load_or_generate_validator_identity(&LoadOrGenerateOpts {
            data_dir: dir.path(),
            keystore_path_override: None,
            passphrase: PassphraseSource::File(pw_path),
        })
        .expect("resolve");

        assert_eq!(identity.status, KeyStatus::LoadedKeystore);
    }
}

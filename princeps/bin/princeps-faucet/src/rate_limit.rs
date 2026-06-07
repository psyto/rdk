//! Stage T4a-2 of `docs/plans/v0-testnet-deploy.md` — SQLite-backed
//! rate limiter for the faucet.
//!
//! Three-dimensional check on every `POST /drip` (the route lands
//! in T4a-3):
//!
//! - **Per-IP** — at most `per_ip_max_drips` drips from a single IP
//!   address inside a `per_ip_window_secs` window. Catches the
//!   casual abuse case where one operator hammers from one host.
//! - **Per-recipient** — same shape against the destination address.
//!   Catches the rotating-IP variant of the same attack.
//! - **Global** — at most `global_max_drips` total drips inside a
//!   `global_window_secs` window. The faucet-wallet drain
//!   protection: even if every check above somehow passes, the
//!   global cap bounds the worst-case daily payout. Open question
//!   #5 of the testnet plan explicitly relies on this.
//!
//! ## Schema + atomicity
//!
//! Single table `drips(id, ip, recipient, ts_unix)` with per-column
//! indexes. Each check is one SELECT-COUNT-then-INSERT pair inside
//! a transaction. The `Connection` is wrapped in `Mutex<...>` (the
//! `rusqlite::Connection` type is `Send` but `!Sync`) which already
//! serializes concurrent callers, so the default DEFERRED
//! transaction mode is sufficient — no TOCTOU between the count
//! and the insert.
//!
//! ## "Drip is consumed even if the tx fails" semantics
//!
//! `check_and_record` is atomic: the row is recorded immediately
//! on success. If a downstream chain submission (T4a-5) fails
//! afterward, the drip is "spent" against the rate-limit budget
//! but never delivered. Considered acceptable for testnet because:
//! (1) the alternative is a two-phase commit that opens a TOCTOU
//! window between check and chain-submit; (2) tokens have no value
//! at testnet; (3) operator support can compensate via a
//! force-drip path if needed.
//!
//! ## Eviction
//!
//! [`SqliteRateLimiter::prune`] deletes rows older than `now -
//! max(all_windows)`. Not wired into a background task at T4a-2 —
//! tasks are integration concerns and land naturally when the
//! `POST /drip` route exists. At v0 testnet volume (≤ a few drips
//! per minute) the table stays small even without pruning;
//! pruning is hygiene, not load.

use std::path::Path;
use std::sync::{Arc, Mutex};

use eyre::Context as _;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

/// The three rate-limit dimensions, configured per
/// [`crate::config::FaucetConfig::rate_limit`]. Every field is
/// required — pretending a dimension doesn't exist by setting its
/// max to `u32::MAX` is a worse failure mode than declaring the
/// intended limit explicitly.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub(crate) struct RateLimitConfig {
    pub per_ip_window_secs: u64,
    pub per_ip_max_drips: u32,
    pub per_recipient_window_secs: u64,
    pub per_recipient_max_drips: u32,
    pub global_window_secs: u64,
    pub global_max_drips: u32,
}

impl RateLimitConfig {
    /// Suggested defaults for the v0 testnet: 1 drip per address +
    /// per IP per 24h, global cap of 100 drips / 1h. Used by the
    /// example config + as the test default; production deployments
    /// should override based on observed traffic.
    #[allow(dead_code)] // referenced by docs + future tests; kept on the type
    pub(crate) const fn testnet_defaults() -> Self {
        Self {
            per_ip_window_secs: 86_400,
            per_ip_max_drips: 1,
            per_recipient_window_secs: 86_400,
            per_recipient_max_drips: 1,
            global_window_secs: 3_600,
            global_max_drips: 100,
        }
    }

    /// Largest configured window across all three dimensions.
    /// Drives [`SqliteRateLimiter::prune`]'s cutoff — rows older
    /// than this can never affect a future check regardless of
    /// which dimension is being evaluated.
    #[allow(dead_code)] // first production caller (prune via background task) lands later
    pub(crate) fn longest_window_secs(&self) -> u64 {
        self.per_ip_window_secs
            .max(self.per_recipient_window_secs)
            .max(self.global_window_secs)
    }
}

/// Reason for rejecting a drip. Carries enough detail for the
/// HTTP layer to format a useful 429 message
/// ("Per-IP limit of N per S seconds exceeded") without having
/// to re-read the config.
#[allow(dead_code)] // first production return path lands in T4a-3 (POST /drip handler)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RateLimitRejection {
    PerIp { max: u32, window_secs: u64 },
    PerRecipient { max: u32, window_secs: u64 },
    Global { max: u32, window_secs: u64 },
}

impl std::fmt::Display for RateLimitRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PerIp { max, window_secs } => write!(
                f,
                "per-IP rate limit exceeded ({max} drips per {window_secs}s window)"
            ),
            Self::PerRecipient { max, window_secs } => write!(
                f,
                "per-recipient rate limit exceeded ({max} drips per {window_secs}s window)"
            ),
            Self::Global { max, window_secs } => write!(
                f,
                "global rate limit exceeded ({max} drips per {window_secs}s window)"
            ),
        }
    }
}

/// SQLite-backed rate limiter. Cheap to clone — the inner
/// connection is `Arc<Mutex<...>>`.
#[derive(Clone)]
pub(crate) struct SqliteRateLimiter {
    inner: Arc<Mutex<Connection>>,
    config: RateLimitConfig,
}

// `check_and_record` and `prune` are unused by the production
// binary today — server.rs wires the limiter into AppState at
// T4a-2 but the first caller (POST /drip handler) lands in
// T4a-3. Tests exercise both methods, so suppress the binary-
// build dead-code warning at the impl block; the warning will
// fall away as soon as T4a-3 calls into either method.
#[allow(dead_code)]

impl SqliteRateLimiter {
    /// Open (or create) the SQLite database at `path`, run the
    /// schema if needed, return a limiter ready to use. The
    /// parent directory is created if it doesn't exist — match
    /// the validator-key path's auto-mkdir behavior.
    pub(crate) fn open(path: &Path, config: RateLimitConfig) -> eyre::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("create rate-limit db parent dir {}", parent.display())
            })?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("open rate-limit db at {}", path.display()))?;
        Self::from_connection(conn, config)
    }

    /// Construct against an already-opened connection. The
    /// in-memory entrypoint for tests goes through here too.
    pub(crate) fn from_connection(
        conn: Connection,
        config: RateLimitConfig,
    ) -> eyre::Result<Self> {
        init_schema(&conn)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(conn)),
            config,
        })
    }

    /// In-memory limiter — sole purpose is testability. Real
    /// deployments always go through [`Self::open`].
    #[cfg(test)]
    pub(crate) fn open_in_memory(config: RateLimitConfig) -> eyre::Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::from_connection(conn, config)
    }

    #[allow(dead_code)] // first caller lands in T4a-3 (handler reads to format the 429 message)
    pub(crate) fn config(&self) -> &RateLimitConfig {
        &self.config
    }

    /// Atomic check + record. Returns `Ok(())` if all three
    /// limits pass and the drip has been recorded; returns
    /// `Err(rejection)` if any dimension fails — in which case
    /// NO row is recorded (verified by
    /// `rejection_does_not_record_a_row`).
    ///
    /// `now_unix_secs` is the caller's clock value. Production
    /// uses `SystemTime::now()`; tests pass a controlled value
    /// so window boundaries are deterministic. Negative `now`
    /// is a misuse (no historical drips have a meaningful
    /// timestamp) — accepted by the query layer but produces
    /// undefined behavior at the consumer; callers MUST pass a
    /// non-negative value.
    pub(crate) fn check_and_record(
        &self,
        ip: &str,
        recipient: &str,
        now_unix_secs: i64,
    ) -> eyre::Result<Result<(), RateLimitRejection>> {
        let conn = self.inner.lock().expect("rate-limit db mutex poisoned");
        // Single transaction: the count + insert pair must observe
        // a consistent table state. The Mutex above already
        // serializes callers; the transaction adds defense-in-depth
        // in case someone later removes the Mutex.
        let tx = conn.unchecked_transaction()?;

        // Per-IP.
        let cfg = &self.config;
        let cutoff_ip = now_unix_secs.saturating_sub(i64::try_from(cfg.per_ip_window_secs).unwrap_or(i64::MAX));
        let recent_ip: u32 = tx
            .query_row(
                "SELECT COUNT(*) FROM drips WHERE ip = ?1 AND ts_unix > ?2",
                params![ip, cutoff_ip],
                |r| r.get(0),
            )?;
        if recent_ip >= cfg.per_ip_max_drips {
            return Ok(Err(RateLimitRejection::PerIp {
                max: cfg.per_ip_max_drips,
                window_secs: cfg.per_ip_window_secs,
            }));
        }

        // Per-recipient.
        let cutoff_recipient = now_unix_secs.saturating_sub(
            i64::try_from(cfg.per_recipient_window_secs).unwrap_or(i64::MAX),
        );
        let recent_recipient: u32 = tx
            .query_row(
                "SELECT COUNT(*) FROM drips WHERE recipient = ?1 AND ts_unix > ?2",
                params![recipient, cutoff_recipient],
                |r| r.get(0),
            )?;
        if recent_recipient >= cfg.per_recipient_max_drips {
            return Ok(Err(RateLimitRejection::PerRecipient {
                max: cfg.per_recipient_max_drips,
                window_secs: cfg.per_recipient_window_secs,
            }));
        }

        // Global.
        let cutoff_global =
            now_unix_secs.saturating_sub(i64::try_from(cfg.global_window_secs).unwrap_or(i64::MAX));
        let recent_global: u32 = tx
            .query_row(
                "SELECT COUNT(*) FROM drips WHERE ts_unix > ?1",
                params![cutoff_global],
                |r| r.get(0),
            )?;
        if recent_global >= cfg.global_max_drips {
            return Ok(Err(RateLimitRejection::Global {
                max: cfg.global_max_drips,
                window_secs: cfg.global_window_secs,
            }));
        }

        // All limits clear — record the drip.
        tx.execute(
            "INSERT INTO drips (ip, recipient, ts_unix) VALUES (?1, ?2, ?3)",
            params![ip, recipient, now_unix_secs],
        )?;
        tx.commit()?;
        Ok(Ok(()))
    }

    /// Delete rows older than the longest configured window
    /// before `now_unix_secs`. Returns the number of rows
    /// deleted so a periodic-prune task can log progress.
    pub(crate) fn prune(&self, now_unix_secs: i64) -> eyre::Result<usize> {
        let conn = self.inner.lock().expect("rate-limit db mutex poisoned");
        let cutoff = now_unix_secs.saturating_sub(
            i64::try_from(self.config.longest_window_secs()).unwrap_or(i64::MAX),
        );
        let n = conn.execute("DELETE FROM drips WHERE ts_unix <= ?1", params![cutoff])?;
        Ok(n)
    }

    /// Count rows in the table. Test-only helper — production
    /// callers should not depend on the row count.
    #[cfg(test)]
    fn row_count(&self) -> usize {
        let conn = self.inner.lock().unwrap();
        conn.query_row("SELECT COUNT(*) FROM drips", [], |r| r.get::<_, i64>(0))
            .unwrap() as usize
    }
}

/// Idempotent schema bootstrap. Called from every constructor;
/// re-running against an existing file is a no-op because every
/// statement uses `IF NOT EXISTS`.
fn init_schema(conn: &Connection) -> eyre::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS drips (
            id        INTEGER PRIMARY KEY AUTOINCREMENT,
            ip        TEXT NOT NULL,
            recipient TEXT NOT NULL,
            ts_unix   INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS drips_ip_ts        ON drips(ip, ts_unix);
        CREATE INDEX IF NOT EXISTS drips_recipient_ts ON drips(recipient, ts_unix);
        CREATE INDEX IF NOT EXISTS drips_ts           ON drips(ts_unix);
        ",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a limiter with the testnet defaults and an
    /// in-memory database. Per-IP and per-recipient max = 1,
    /// global max = 100 — high enough that single-dimension
    /// tests don't accidentally trip the global cap.
    fn fresh_limiter() -> SqliteRateLimiter {
        SqliteRateLimiter::open_in_memory(RateLimitConfig::testnet_defaults())
            .expect("open in-memory")
    }

    /// Limiter with a tight global cap so the global-dimension
    /// test isn't masked by the higher per-IP/recipient ceilings.
    fn limiter_with_global_cap(max: u32) -> SqliteRateLimiter {
        let cfg = RateLimitConfig {
            // Make per-* cheap to hit the global path without
            // tripping the dimensional limits.
            per_ip_window_secs: 1,
            per_ip_max_drips: u32::MAX,
            per_recipient_window_secs: 1,
            per_recipient_max_drips: u32::MAX,
            global_window_secs: 3_600,
            global_max_drips: max,
        };
        SqliteRateLimiter::open_in_memory(cfg).expect("open in-memory")
    }

    /// First drip succeeds and lands as a row.
    #[test]
    fn first_drip_allowed_records_row() {
        let lim = fresh_limiter();
        assert_eq!(
            lim.check_and_record("1.2.3.4", "0xabc", 1_700_000_000).unwrap(),
            Ok(())
        );
        assert_eq!(lim.row_count(), 1);
    }

    /// Second drip from the same IP (with a fresh recipient)
    /// inside the per-IP window is rejected with PerIp.
    #[test]
    fn second_drip_same_ip_rejected_per_ip() {
        let lim = fresh_limiter();
        lim.check_and_record("1.2.3.4", "0xabc", 1_700_000_000).unwrap().unwrap();
        let res = lim
            .check_and_record("1.2.3.4", "0xdef", 1_700_000_001)
            .unwrap();
        assert!(matches!(res, Err(RateLimitRejection::PerIp { .. })));
    }

    /// Second drip to the same recipient (from a fresh IP) inside
    /// the per-recipient window is rejected with PerRecipient.
    #[test]
    fn second_drip_same_recipient_rejected_per_recipient() {
        let lim = fresh_limiter();
        lim.check_and_record("1.2.3.4", "0xabc", 1_700_000_000).unwrap().unwrap();
        let res = lim
            .check_and_record("5.6.7.8", "0xabc", 1_700_000_001)
            .unwrap();
        assert!(matches!(res, Err(RateLimitRejection::PerRecipient { .. })));
    }

    /// Once global_max_drips drips have landed inside the global
    /// window, additional drips are rejected with Global even if
    /// IP and recipient are fresh.
    #[test]
    fn global_limit_rejects_after_max_dripped() {
        let lim = limiter_with_global_cap(3);
        // Three allowed drips, each with a unique IP + recipient.
        for i in 0..3 {
            let ip = format!("10.0.0.{i}");
            let rcp = format!("0x{i}");
            lim.check_and_record(&ip, &rcp, 1_700_000_000).unwrap().unwrap();
        }
        // Fourth attempt hits the global ceiling.
        let res = lim
            .check_and_record("10.0.0.99", "0xff", 1_700_000_001)
            .unwrap();
        assert!(matches!(res, Err(RateLimitRejection::Global { .. })));
    }

    /// After the per-IP window has elapsed (with a fresh
    /// recipient to dodge that dimension), the same IP can drip
    /// again. Pins the window's strict `ts > cutoff` boundary.
    #[test]
    fn per_ip_drip_after_window_succeeds() {
        let lim = fresh_limiter();
        lim.check_and_record("1.2.3.4", "0xabc", 1_700_000_000).unwrap().unwrap();
        // 24h + 1s later, still same IP but a fresh recipient.
        let later = 1_700_000_000 + 86_401;
        let res = lim
            .check_and_record("1.2.3.4", "0xdef", later)
            .unwrap();
        assert_eq!(res, Ok(()));
        assert_eq!(lim.row_count(), 2);
    }

    /// A rejected attempt must NOT insert a row — otherwise a
    /// throttled attacker could still consume the global budget
    /// without ever getting a payout, draining the cap for
    /// honest users.
    #[test]
    fn rejection_does_not_record_a_row() {
        let lim = fresh_limiter();
        lim.check_and_record("1.2.3.4", "0xabc", 1_700_000_000).unwrap().unwrap();
        let before = lim.row_count();
        let res = lim
            .check_and_record("1.2.3.4", "0xdef", 1_700_000_001)
            .unwrap();
        assert!(matches!(res, Err(RateLimitRejection::PerIp { .. })));
        assert_eq!(lim.row_count(), before, "rejection must not add a row");
    }

    /// `prune` deletes rows older than the longest configured
    /// window, leaves newer rows alone, and reports the count
    /// deleted.
    #[test]
    fn prune_removes_old_rows_only() {
        let lim = fresh_limiter();
        // longest window is per_ip / per_recipient = 86_400s.
        // Two rows old enough to prune (>86_400 before "now"),
        // one fresh row.
        let now = 1_700_100_000_i64;
        let old1 = now - 90_000;
        let old2 = now - 100_000;
        let fresh = now - 100;
        {
            let conn = lim.inner.lock().unwrap();
            conn.execute(
                "INSERT INTO drips (ip, recipient, ts_unix) VALUES ('1', 'a', ?1),
                                                                     ('2', 'b', ?2),
                                                                     ('3', 'c', ?3)",
                params![old1, old2, fresh],
            )
            .unwrap();
        }
        assert_eq!(lim.row_count(), 3);
        let deleted = lim.prune(now).unwrap();
        assert_eq!(deleted, 2);
        assert_eq!(lim.row_count(), 1);
    }

    /// Boundary: a row with `ts_unix == now - window` is OUTSIDE
    /// the window (strict `>` in the cutoff query). One second
    /// inside, the row is considered recent. Pins this so a
    /// future change to `>=` surfaces here.
    #[test]
    fn window_boundary_is_strict_greater_than() {
        let cfg = RateLimitConfig {
            per_ip_window_secs: 100,
            per_ip_max_drips: 1,
            per_recipient_window_secs: 100,
            per_recipient_max_drips: 1,
            global_window_secs: 1_000,
            global_max_drips: 100,
        };
        let lim = SqliteRateLimiter::open_in_memory(cfg).unwrap();

        // First drip at t=1000. Cutoff for a second drip at t=1100
        // is 1100 - 100 = 1000; original row has ts_unix=1000
        // which is NOT > 1000, so it's outside the window: the
        // second drip is allowed.
        lim.check_and_record("ip", "rcp1", 1000).unwrap().unwrap();
        let on_boundary = lim.check_and_record("ip", "rcp2", 1100).unwrap();
        assert_eq!(on_boundary, Ok(()), "row at exact window edge should not count");

        // But one second inside the window, the just-recorded
        // row at t=1100 IS within range from t=1199.
        let inside = lim.check_and_record("ip", "rcp3", 1199).unwrap();
        assert!(matches!(inside, Err(RateLimitRejection::PerIp { .. })));
    }

    /// Schema bootstrap is idempotent: opening the same file
    /// twice doesn't error and doesn't lose rows.
    #[test]
    fn open_then_open_again_preserves_rows() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("state.db");
        let cfg = RateLimitConfig::testnet_defaults();
        {
            let lim = SqliteRateLimiter::open(&db, cfg).unwrap();
            lim.check_and_record("1.2.3.4", "0xabc", 1_700_000_000).unwrap().unwrap();
        }
        let lim2 = SqliteRateLimiter::open(&db, cfg).unwrap();
        assert_eq!(lim2.row_count(), 1, "rows must survive reopen");
    }

    /// open() creates the parent directory if missing — matches
    /// the validator-key path's auto-mkdir convention.
    #[test]
    fn open_creates_parent_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("nested").join("state.db");
        let lim =
            SqliteRateLimiter::open(&db, RateLimitConfig::testnet_defaults()).unwrap();
        assert!(db.exists());
        assert_eq!(lim.row_count(), 0);
    }
}

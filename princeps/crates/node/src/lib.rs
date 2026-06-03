//! `princeps-node` — integration coordinator for the princeps L1 (Stage 13).
//!
//! No new state machines, no new pure-compute primitives. This crate
//! is the **composition layer**: it owns one [`OracleState`], one
//! [`LiquidationScanner`] (with its [`InsuranceFund`]), and one
//! [`VaultState`], and runs them through the per-block tick that
//! `crates/liquidation/src/lib.rs` documents as "the bridge's
//! per-block flow." Stage 13 lifts that comment into actual code.
//!
//! ### What `tick` does
//!
//! Each block the bridge calls [`PrincepsNode::tick`] with:
//!   - `block_time` / `block_height` — current chain time/height.
//!   - `mark` — the current top-of-book mark price (from the CLOB).
//!   - `account_snapshots` — every non-flat account in the market
//!     (the bridge assembles these from its position table).
//!   - `vault_total_assets` — the vault's current asset value
//!     (collateral + marked `PnL`), computed off-tick by the bridge
//!     from the vault's own positions.
//!
//! The tick then:
//!   1. **Refreshes the oracle** (if the configured interval has
//!      elapsed since the last refresh). Stale-feed filter + median
//!      + deviation guard from `rdk_oracle`.
//!   2. **Scans for liquidations** using [`LiquidationScanner::scan`].
//!      Liquidatable / Underwater accounts produce close orders and
//!      mutate the insurance fund.
//!   3. **Runs ADL** if `ScanReport::unfilled_deficit > 0` and the
//!      config opted in. Profitable counter-positions are ranked and
//!      haircut via [`execute_adl`].
//!   4. **Marks the vault to market** by pushing the bridge-computed
//!      `vault_total_assets` into [`VaultState::mark_to_market`]. No
//!      shares are minted or burned — only NAV per share moves.
//!
//! Funding settlement is **not** part of `tick` — it's per-position
//! and happens on the funding clock's own cadence, called by the
//! bridge separately. The bridge layer composes both as it sees fit.
//!
//! ### What `tick` does NOT do
//!
//! - **Submit close orders to the CLOB.** `tick` produces a
//!   `ScanReport` whose `records` carry close-order specs; the bridge
//!   submits them to the matching engine. Keeping the coordinator
//!   side-effect-free against the CLOB lets it stay a pure
//!   state-machine driver.
//! - **Apply ADL bookkeeping mutations.** Same reason — `tick`
//!   produces an `AdlReport` whose records the bridge applies to its
//!   own position/balance tables.
//! - **Halt the chain on unresolvable deficit.** If `tick` returns
//!   `adl.deficit_remaining > 0`, the bridge decides whether to halt
//!   or accept protocol loss per deployment policy. Stage 13 doesn't
//!   make that policy call.
//!
//! ### Why no Reth boot here
//!
//! Booting Reth + the consensus bridge is `crates/evm`'s
//! `LiveRethEvmBridge` (in production-shape since Stage 9d).
//! `princeps-node` is one level above that: the per-block state-machine
//! driver that the bridge calls into. Splitting the Reth-side
//! composition (in `evm`) from the princeps-side composition (here)
//! keeps each layer independently testable. The `bin/princeps` binary
//! will own wiring of these two layers together.

use rdk_funding::{FundingClock, FundingParams, FundingTick, IndexPrice, MarkPrice, Position};
use rdk_liquidation::{
    execute_adl, AccountSnapshot, AdlReport, InsuranceFund, LiquidationParams,
    LiquidationScanner, ScanReport, WithdrawOutcome,
};
use rdk_oracle::{
    AggregatedPrice, AggregationError, FeedId, ObservationError, OracleParams, OracleState,
    PriceObservation, PublisherKey,
};
use rdk_vault::{VaultParams, VaultState};
use serde::{Deserialize, Serialize};

/// Oracle circuit-breaker parameters (threat-model row O-3).
///
/// Per-block deviation guard on the aggregated oracle price. When a
/// refresh produces a price that differs from the prior cached price by
/// more than `max_per_block_deviation_bps`, the breaker trips for
/// `halt_duration_blocks` blocks. While tripped, the per-block lending
/// scan / bad-debt absorption loop in `bin/princeps` skips its run —
/// liquidations on a suspect oracle would be the very thing the attacker
/// is trying to trigger. Repayments are not gated (always safe to reduce
/// debt) and existing perp settlement continues to use the last cached
/// good price.
///
/// At v0 the breaker only short-circuits the coordinator-driven loop;
/// precompile-level enforcement (revert on borrow / withdraw during
/// halt) is v1 work because it requires the precompiles to read
/// coordinator state, which they currently don't. The threat model
/// (`docs/threat-model.md`, row O-3) flags this scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CircuitBreakerParams {
    /// Maximum allowed per-block move of the aggregated oracle price,
    /// expressed in basis points of the prior cached price. A value of
    /// `0` disables the guard entirely (any move accepted); `2000` =
    /// 20%, a reasonable v0 default for ETH-scale volatility.
    pub max_per_block_deviation_bps: u32,
    /// Number of blocks the breaker stays tripped after firing. Each
    /// new anomalous observation re-arms `oracle_halt_until` to the new
    /// maximum, so a sustained spike keeps the halt extended rather
    /// than letting it expire mid-attack.
    pub halt_duration_blocks: u64,
}

impl CircuitBreakerParams {
    /// v0 default: 20% per-block move trips the breaker for 50 blocks
    /// (≈ 50 seconds at 1s block time). Both values are open questions
    /// for tuning during testnet — too tight and benign volatility
    /// halts the protocol; too loose and oracle attacks proceed.
    #[must_use]
    pub const fn v0_default() -> Self {
        Self {
            max_per_block_deviation_bps: 2_000,
            halt_duration_blocks: 50,
        }
    }

    /// Disabled — the guard accepts any move and never trips. Useful
    /// for unit tests of unrelated tick paths.
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            max_per_block_deviation_bps: 0,
            halt_duration_blocks: 0,
        }
    }
}

/// Static configuration for the node. Set once at chain genesis;
/// changing values mid-chain would fork the network.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PrincepsNodeConfig {
    /// Seconds between automatic oracle refreshes. The tick triggers
    /// a refresh when `block_time >= last_refresh + interval`.
    pub oracle_refresh_interval_secs: u64,
    /// Liquidation engine parameters (initial / maintenance margin,
    /// liquidation fee).
    pub liquidation_params: LiquidationParams,
    /// Oracle aggregator parameters (staleness window, min feeds,
    /// deviation cap).
    pub oracle_params: OracleParams,
    /// Vault parameters (deposit floor).
    pub vault_params: VaultParams,
    /// Funding clock parameters (interval, rate cap, divisor).
    pub funding_params: FundingParams,
    /// Oracle circuit-breaker thresholds (per-block deviation guard +
    /// halt duration). See [`CircuitBreakerParams`].
    pub circuit_breaker_params: CircuitBreakerParams,
    /// When `true`, the tick auto-runs ADL on any
    /// `ScanReport::unfilled_deficit > 0`. When `false`, the bridge
    /// inspects the scan report itself and decides what to do.
    pub run_adl_on_unfilled_deficit: bool,
}

impl PrincepsNodeConfig {
    /// Hyperliquid-shape defaults that match the worked examples in
    /// the rethlab Perp Primer course. Real deployments override.
    #[must_use]
    pub const fn hyperliquid_default() -> Self {
        Self {
            oracle_refresh_interval_secs: 12,
            liquidation_params: LiquidationParams::hyperliquid_default(),
            oracle_params: OracleParams::hyperliquid_default(),
            vault_params: VaultParams::production_default(),
            funding_params: FundingParams::hyperliquid_default(),
            circuit_breaker_params: CircuitBreakerParams::v0_default(),
            run_adl_on_unfilled_deficit: true,
        }
    }
}

/// Per-tick input the bridge hands the coordinator.
#[derive(Debug, Clone, Copy)]
pub struct TickInput<'a> {
    pub block_height: u64,
    pub block_time: u64,
    /// Current top-of-book mark from the CLOB. The coordinator does
    /// not read the CLOB itself — the bridge supplies it.
    pub mark: MarkPrice,
    /// Snapshots of every non-flat account in the market. The bridge
    /// is responsible for deterministic ordering (typically
    /// `account_id`-sorted).
    pub account_snapshots: &'a [AccountSnapshot],
    /// Vault's current total assets (collateral + marked `PnL`)
    /// computed off-tick by the bridge from the vault's own perp
    /// positions.
    pub vault_total_assets: i64,
}

/// Snapshot of the [`PrincepsNode`]'s runtime state, for restart-time
/// resume (Stage 14e). Saved to disk by the binary alongside the
/// bridge's chain snapshot; restored via [`PrincepsNode::load_snapshot`]
/// on the next boot before the engine app loop starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoordinatorSnapshot {
    /// Insurance fund balance carried over from the last run. The
    /// scanner re-instantiates with this on load.
    pub insurance_fund_balance: i64,
    /// Total vault shares outstanding.
    pub vault_total_shares: u64,
    /// Total vault assets under management. May be negative if the
    /// vault is insolvent — see [`VaultState::is_insolvent`].
    pub vault_total_assets: i64,
    /// Last block_time at which the oracle successfully refreshed.
    /// Restoring this prevents an unnecessary refresh on the first
    /// tick after restart when the interval hasn't elapsed.
    pub last_oracle_refresh_at: Option<u64>,
    /// Last block_time at which the funding clock successfully
    /// settled (Stage 15a). Restoring this prevents an unintended
    /// extra settlement on the first tick after restart when the
    /// interval hasn't elapsed.
    pub funding_last_settled_at: u64,
    /// Cached oracle aggregate from the last successful refresh
    /// (Stage 16d). Persisting it means the funding clock and any
    /// other consumer of `oracle.current_price()` keep working
    /// across restart instead of silently pausing until the next
    /// refresh interval elapses. `#[serde(default)]` so older
    /// on-disk coordinator snapshots deserialize as `None`.
    #[serde(default)]
    pub cached_oracle_price: Option<AggregatedPrice>,
    /// Active oracle circuit-breaker halt, if any. Restoring this
    /// prevents an attacker from re-running an oracle attack across
    /// a node restart that would otherwise clear `oracle_halt_until`.
    /// `#[serde(default)]` so older snapshots deserialize as `None`.
    #[serde(default)]
    pub oracle_halt_until: Option<u64>,
}

/// Per-tick output — aggregated reports plus a snapshot of post-tick
/// vault state for telemetry. Every field is structured so the bridge
/// can pick the parts it needs without re-reading the coordinator's
/// internal state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickReport {
    pub block_height: u64,
    pub block_time: u64,
    /// `Some(Ok(price))` if the oracle refreshed this tick and
    /// succeeded; `Some(Err(...))` if it tried and failed (insufficient
    /// fresh feeds, quorum failed after deviation filter); `None` if
    /// the refresh interval hadn't elapsed.
    pub oracle: Option<Result<AggregatedPrice, AggregationError>>,
    /// The liquidation scan report.
    pub liquidation: ScanReport,
    /// `Some(report)` when the scan surfaced an `unfilled_deficit > 0`
    /// AND the config opted into auto-ADL; `None` otherwise.
    pub adl: Option<AdlReport>,
    /// Vault state after `mark_to_market`. Bridge uses this for
    /// telemetry / accounting reconciliation.
    pub vault_total_shares: u64,
    pub vault_total_assets: i64,
    pub vault_share_price_bps: Option<i64>,
    /// `Some(tick)` when the funding clock fired this block — i.e.,
    /// the oracle had a cached index price AND the interval since the
    /// last settlement had elapsed. `None` otherwise.
    ///
    /// Stage 15a only surfaces the rate / settlements as telemetry;
    /// applying the settlements to account balances is the clearing
    /// layer's job and lands in a later stage.
    pub funding: Option<FundingTick>,
    /// `Some(until)` if the per-block oracle deviation guard tripped
    /// this tick. The breaker stays armed through `block_height ≤ until`;
    /// downstream lending bad-debt absorption should skip its run while
    /// any halt is active. `None` if no trip occurred this tick.
    ///
    /// Threat-model row O-3.
    pub circuit_breaker_tripped_until: Option<u64>,
}

/// The integration coordinator. One [`PrincepsNode`] per deployed
/// market — multi-market deployments instantiate one per market.
#[derive(Debug, Clone)]
pub struct PrincepsNode {
    config: PrincepsNodeConfig,
    oracle: OracleState,
    scanner: LiquidationScanner,
    vault: VaultState,
    funding_clock: FundingClock,
    last_oracle_refresh_at: Option<u64>,
    /// `Some(block)` if the oracle circuit breaker has tripped and the
    /// halt window has not yet elapsed. `None` if the breaker has
    /// never tripped or the window has fully expired. The lending
    /// bad-debt loop in `bin/princeps` consults this via
    /// [`PrincepsNode::is_oracle_halted`].
    oracle_halt_until: Option<u64>,
}

impl PrincepsNode {
    /// Construct a fresh node from config. The oracle, scanner, and
    /// vault all start in their empty states (no feeds, no insurance
    /// fund, no shares).
    #[must_use]
    pub fn new(config: PrincepsNodeConfig) -> Self {
        let oracle = OracleState::new(config.oracle_params);
        let scanner = LiquidationScanner::with_empty_fund(config.liquidation_params);
        let vault = VaultState::new(config.vault_params);
        // Genesis time 0: first tick at any block_time ≥ interval_secs fires.
        let funding_clock = FundingClock::new(config.funding_params, 0);
        Self {
            config,
            oracle,
            scanner,
            vault,
            funding_clock,
            last_oracle_refresh_at: None,
            oracle_halt_until: None,
        }
    }

    /// Construct a node from an existing insurance-fund balance —
    /// supports resuming from a snapshot or genesis-seeding the fund.
    #[must_use]
    pub fn with_insurance_fund(config: PrincepsNodeConfig, fund: InsuranceFund) -> Self {
        let oracle = OracleState::new(config.oracle_params);
        let scanner = LiquidationScanner::new(config.liquidation_params, fund);
        let vault = VaultState::new(config.vault_params);
        let funding_clock = FundingClock::new(config.funding_params, 0);
        Self {
            config,
            oracle,
            scanner,
            vault,
            funding_clock,
            last_oracle_refresh_at: None,
            oracle_halt_until: None,
        }
    }

    /// Borrow the config.
    #[must_use]
    pub const fn config(&self) -> &PrincepsNodeConfig {
        &self.config
    }

    /// Borrow the oracle (read-only).
    #[must_use]
    pub const fn oracle(&self) -> &OracleState {
        &self.oracle
    }

    /// Mutable access to the oracle. The bridge uses this to register
    /// publisher keys, ingest signed observations, etc. — operations
    /// that happen between ticks rather than inside one.
    pub const fn oracle_mut(&mut self) -> &mut OracleState {
        &mut self.oracle
    }

    /// Borrow the liquidation scanner (read-only).
    #[must_use]
    pub const fn scanner(&self) -> &LiquidationScanner {
        &self.scanner
    }

    /// Absorb a lending bad-debt shortfall from the integration coordinator's
    /// insurance fund. The complement to the bridge's
    /// `absorb_account_bad_debt` (which wipes the account's lending positions
    /// and returns the shortfall amount).
    ///
    /// Typical orchestration in a long-running node:
    /// ```ignore
    /// let bad_debt = bridge.absorb_account_bad_debt(account, mark, prices);
    /// let outcome = node.absorb_lending_bad_debt(bad_debt as i64);
    /// match outcome {
    ///     WithdrawOutcome::Covered { amount: _ } => { /* fund had enough */ }
    ///     WithdrawOutcome::PartiallyDrained { amount: _, unfilled }
    ///     | WithdrawOutcome::Depleted { unfilled } => {
    ///         // Insurance fund is out — Stage 10d ADL territory or
    ///         // protocol-level halt.
    ///     }
    /// }
    /// ```
    ///
    /// Non-positive `shortfall` is a no-op (returns `Covered { amount: 0 }`),
    /// matching `InsuranceFund::withdraw_shortfall` semantics.
    ///
    /// Stage 22c bridge-side + this node-side method together close the
    /// cross-layer bad-debt routing concern. Bridge-side accumulator that
    /// feeds this per-block tick (instead of a single call per account) is
    /// future work, gated on whether a long-running production loop emerges.
    pub fn absorb_lending_bad_debt(&mut self, shortfall: i64) -> WithdrawOutcome {
        self.scanner.fund_mut().withdraw_shortfall(shortfall)
    }

    /// Borrow the vault (read-only).
    #[must_use]
    pub const fn vault(&self) -> &VaultState {
        &self.vault
    }

    /// Mutable access to the vault. The bridge uses this for deposit
    /// / withdraw operations that happen between ticks.
    pub const fn vault_mut(&mut self) -> &mut VaultState {
        &mut self.vault
    }

    /// Register a publisher key, passthrough to the oracle. Stage 11b
    /// path; the bridge calls this once per publisher at chain
    /// configuration time (and again for each rotation).
    pub fn register_publisher(&mut self, feed: FeedId, key: PublisherKey) {
        self.oracle.register_publisher(feed, key);
    }

    /// Ingest one observation via the unsigned (trusted-bridge) path.
    /// Returns the same [`ObservationError`]s as the underlying
    /// [`OracleState::ingest`].
    pub fn ingest_observation(
        &mut self,
        obs: PriceObservation,
        now: u64,
    ) -> Result<(), ObservationError> {
        self.oracle.ingest(obs, now)
    }

    /// Ingest one signed observation. Verifies the ECDSA signature
    /// against the registered publisher key before storing.
    pub fn ingest_signed_observation(
        &mut self,
        obs: PriceObservation,
        now: u64,
    ) -> Result<(), ObservationError> {
        self.oracle.ingest_signed(obs, now)
    }

    /// Run one per-block tick.
    ///
    /// Order of operations is fixed (deterministic):
    ///   1. Oracle refresh (if interval elapsed).
    ///   2. Liquidation scan.
    ///   3. ADL (conditional on scan result + config).
    ///   4. Vault mark-to-market.
    ///
    /// The mark used for liquidation is always the bridge-supplied
    /// `input.mark`, **not** the oracle's freshly-aggregated price.
    /// They serve different purposes: the oracle's index price feeds
    /// funding (`premium = mark − index`), while the CLOB-derived
    /// mark drives margin classification (a contract's collateral is
    /// only stress-tested against the CLOB it can actually exit into).
    /// Conflating the two would let a stale oracle delay
    /// otherwise-required liquidations.
    pub fn tick(&mut self, input: TickInput<'_>) -> TickReport {
        // 1a. Capture the prior cached oracle price *before* refresh so
        //     the circuit-breaker comparison sees the pre-refresh value.
        let prior_index = self.oracle.current_price();

        // 1b. Oracle refresh — only if the interval has elapsed.
        let oracle_result = self.maybe_refresh_oracle(input.block_time);

        // 1c. Circuit-breaker check — fires when a successful refresh
        //     moves the aggregated price by more than the configured
        //     deviation cap relative to the prior cached price.
        let circuit_breaker_tripped_this_tick = self.evaluate_circuit_breaker(
            prior_index,
            oracle_result.as_ref().and_then(|r| r.as_ref().ok()).copied(),
            input.block_height,
        );

        // 2. Liquidation scan against the CLOB-derived mark.
        let scan = self.scanner.scan(input.account_snapshots, input.mark);

        // 3. ADL only if scan surfaced unfilled deficit AND config opts in.
        let adl_report = if self.config.run_adl_on_unfilled_deficit && scan.unfilled_deficit > 0 {
            Some(execute_adl(
                input.account_snapshots,
                input.mark,
                scan.unfilled_deficit,
            ))
        } else {
            None
        };

        // 4. Vault mark-to-market — no shares move, only NAV.
        self.vault.mark_to_market(input.vault_total_assets);

        // 5. Funding tick — only if the oracle has a cached current
        //    price AND the funding interval has elapsed. The clock's
        //    own gating decides whether a settlement actually fires;
        //    we just supply the inputs. Per the module's "no catch-up"
        //    invariant, a long gap still produces at most one tick.
        let funding = self.oracle.current_price().and_then(|index| {
            let positions: Vec<Position> = input
                .account_snapshots
                .iter()
                .map(|snap| Position {
                    account: snap.account,
                    size: snap.position_size,
                })
                .collect();
            self.funding_clock
                .tick(input.block_time, input.mark, index, &positions)
        });

        TickReport {
            block_height: input.block_height,
            block_time: input.block_time,
            oracle: oracle_result,
            liquidation: scan,
            adl: adl_report,
            vault_total_shares: self.vault.total_shares().0,
            vault_total_assets: self.vault.total_assets().0,
            vault_share_price_bps: self.vault.share_price_bps(),
            funding,
            circuit_breaker_tripped_until: if circuit_breaker_tripped_this_tick {
                self.oracle_halt_until
            } else {
                None
            },
        }
    }

    /// Returns `true` if the oracle circuit breaker is currently armed
    /// — i.e., it tripped at some prior tick and `block_height` is
    /// still within the halt window. Lending bad-debt absorption (and
    /// any other code path that should not act on a suspect oracle)
    /// consults this before running.
    #[must_use]
    pub fn is_oracle_halted(&self, block_height: u64) -> bool {
        self.oracle_halt_until
            .is_some_and(|until| block_height <= until)
    }

    /// Block height at which the current halt expires, if any.
    /// Useful for telemetry / RPC. Returns `None` when no halt is armed.
    #[must_use]
    pub const fn oracle_halt_until(&self) -> Option<u64> {
        self.oracle_halt_until
    }

    /// Internal: evaluate this tick's deviation between `prior_index`
    /// (pre-refresh cached) and the freshly-aggregated `new_price`. If
    /// the deviation exceeds `circuit_breaker_params.max_per_block_deviation_bps`,
    /// re-arm `oracle_halt_until` to `block_height + halt_duration_blocks`
    /// (taking max with any existing halt). Returns whether the breaker
    /// tripped on this tick.
    ///
    /// Returns `false` (no trip) when:
    /// - no refresh happened this tick (`new_price` is `None`)
    /// - no prior price exists to compare against (first observation)
    /// - the refresh produced an error (already a halt signal upstream)
    /// - the deviation guard is disabled (`max_per_block_deviation_bps == 0`)
    /// - the actual deviation is within the cap
    fn evaluate_circuit_breaker(
        &mut self,
        prior_index: Option<IndexPrice>,
        new_price: Option<AggregatedPrice>,
        block_height: u64,
    ) -> bool {
        let params = self.config.circuit_breaker_params;
        if params.max_per_block_deviation_bps == 0 {
            return false;
        }
        let (Some(prior), Some(fresh)) = (prior_index, new_price) else {
            return false;
        };
        if prior.0 == 0 {
            // No meaningful denominator; treat as first observation.
            return false;
        }
        let new_index = fresh.index.0;
        let prior_index_value = prior.0;
        let abs_delta = new_index.abs_diff(prior_index_value);
        // u128 intermediate to avoid overflow on large prices.
        let bps =
            u128::from(abs_delta).saturating_mul(10_000) / u128::from(prior_index_value);
        if bps <= u128::from(params.max_per_block_deviation_bps) {
            return false;
        }
        let new_until = block_height.saturating_add(params.halt_duration_blocks);
        self.oracle_halt_until = Some(match self.oracle_halt_until {
            Some(existing) => existing.max(new_until),
            None => new_until,
        });
        true
    }

    /// Capture the load-bearing fields for cross-restart resume
    /// (Stage 14e). Mirrors the bridge's [`BridgeSnapshot`] —
    /// deliberately small, covering only the fields that the next
    /// boot can't reconstruct from config + per-block inputs.
    ///
    /// Excluded by design:
    ///   - `config`: comes from the binary at boot.
    ///   - `oracle.feeds` / `oracle.publishers`: feeds are re-ingested
    ///     every block; publishers are re-registered by the binary at
    ///     boot (the bridge owns the registry).
    ///   - `scanner.params` / `vault.params`: come from config.
    #[must_use]
    pub fn snapshot(&self) -> CoordinatorSnapshot {
        CoordinatorSnapshot {
            insurance_fund_balance: self.scanner.fund_balance(),
            vault_total_shares: self.vault.total_shares().0,
            vault_total_assets: self.vault.total_assets().0,
            last_oracle_refresh_at: self.last_oracle_refresh_at,
            funding_last_settled_at: self.funding_clock.last_settled_at(),
            cached_oracle_price: self.oracle.current(),
            oracle_halt_until: self.oracle_halt_until,
        }
    }

    /// Apply a [`CoordinatorSnapshot`] to this node's runtime state.
    /// Used immediately after `PrincepsNode::new` to resume from a prior
    /// run's persisted snapshot. Publisher registrations and oracle
    /// feed observations are NOT restored — those flow back through
    /// `register_publisher` and `ingest_signed_observation` at boot.
    pub fn load_snapshot(&mut self, snap: CoordinatorSnapshot) {
        self.scanner = LiquidationScanner::new(
            self.config.liquidation_params,
            InsuranceFund::new(snap.insurance_fund_balance),
        );
        self.vault = VaultState::restore(
            self.config.vault_params,
            snap.vault_total_shares,
            snap.vault_total_assets,
        );
        self.funding_clock = FundingClock::new(
            self.config.funding_params,
            snap.funding_last_settled_at,
        );
        self.last_oracle_refresh_at = snap.last_oracle_refresh_at;
        self.oracle_halt_until = snap.oracle_halt_until;
        if let Some(price) = snap.cached_oracle_price {
            self.oracle.restore_current(price);
        }
    }

    fn maybe_refresh_oracle(
        &mut self,
        block_time: u64,
    ) -> Option<Result<AggregatedPrice, AggregationError>> {
        let should_refresh = match self.last_oracle_refresh_at {
            None => true,
            Some(last) => {
                block_time.saturating_sub(last) >= self.config.oracle_refresh_interval_secs
            }
        };
        if !should_refresh {
            return None;
        }
        let result = self.oracle.refresh(block_time);
        if result.is_ok() {
            self.last_oracle_refresh_at = Some(block_time);
        }
        Some(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rdk_funding::{IndexPrice, Notional, PositionSize};

    fn default_node() -> PrincepsNode {
        PrincepsNode::new(PrincepsNodeConfig::hyperliquid_default())
    }

    fn snapshot(account: u64, size: i64, entry: u64, collateral: i64) -> AccountSnapshot {
        AccountSnapshot {
            account: rdk_clob::AccountId(account),
            position_size: PositionSize(size),
            avg_entry: MarkPrice(entry),
            collateral: Notional(collateral),
        }
    }

    // ─── construction ──────────────────────────────────────────────

    #[test]
    fn new_node_is_empty() {
        let node = default_node();
        assert_eq!(node.oracle().feed_count(), 0);
        assert_eq!(node.scanner().fund_balance(), 0);
        assert_eq!(node.vault().total_shares().0, 0);
        assert_eq!(node.vault().total_assets().0, 0);
    }

    #[test]
    fn snapshot_round_trips_load_bearing_state() {
        // Build up some non-default state, snapshot it, restore on a
        // fresh node, and assert the load-bearing fields match.
        let mut node = PrincepsNode::with_insurance_fund(
            PrincepsNodeConfig::hyperliquid_default(),
            InsuranceFund::new(750),
        );
        assert_eq!(node.scanner().fund_balance(), 750);
        // Vault: pretend a depositor put 10_000 in (mints 10_000 shares at inception).
        let _ = node.vault_mut().deposit(10_000).expect("inception deposit");
        // last_oracle_refresh_at is private to this module — fine to
        // set directly inside `mod tests`.
        node.last_oracle_refresh_at = Some(12);

        // Pretend the funding clock has settled once at block_time 9.
        node.funding_clock = FundingClock::new(node.config.funding_params, 9);
        // Stage 16d: also pretend the oracle has a cached price.
        node.oracle.restore_current(AggregatedPrice {
            index: rdk_funding::IndexPrice(102),
            computed_at: 12,
            feeds_used: 3,
        });

        let snap = node.snapshot();
        assert_eq!(snap.insurance_fund_balance, 750);
        assert_eq!(snap.vault_total_shares, 10_000);
        assert_eq!(snap.vault_total_assets, 10_000);
        assert_eq!(snap.last_oracle_refresh_at, Some(12));
        assert_eq!(snap.funding_last_settled_at, 9);
        assert_eq!(
            snap.cached_oracle_price.map(|p| p.index.0),
            Some(102),
        );

        // Round-trip via serde to mirror the real on-disk path.
        let bytes = serde_json::to_vec(&snap).expect("serialize");
        let decoded: CoordinatorSnapshot = serde_json::from_slice(&bytes).expect("deserialize");
        assert_eq!(decoded, snap);

        let mut fresh = default_node();
        assert_eq!(fresh.scanner().fund_balance(), 0);
        assert_eq!(fresh.vault().total_shares().0, 0);
        assert_eq!(fresh.funding_clock.last_settled_at(), 0);
        assert_eq!(fresh.oracle().current_price(), None);
        fresh.load_snapshot(decoded);
        assert_eq!(fresh.scanner().fund_balance(), 750);
        assert_eq!(fresh.vault().total_shares().0, 10_000);
        assert_eq!(fresh.vault().total_assets().0, 10_000);
        assert_eq!(fresh.last_oracle_refresh_at, Some(12));
        assert_eq!(fresh.funding_clock.last_settled_at(), 9);
        assert_eq!(
            fresh.oracle().current_price(),
            Some(rdk_funding::IndexPrice(102)),
        );
    }

    #[test]
    fn with_insurance_fund_seeds_balance() {
        let node = PrincepsNode::with_insurance_fund(
            PrincepsNodeConfig::hyperliquid_default(),
            InsuranceFund::new(50_000),
        );
        assert_eq!(node.scanner().fund_balance(), 50_000);
    }

    // ─── tick: empty market ────────────────────────────────────────

    #[test]
    fn tick_on_empty_market_does_nothing_destructive() {
        let mut node = default_node();
        let report = node.tick(TickInput {
            block_height: 1,
            block_time: 100,
            mark: MarkPrice(1_000),
            account_snapshots: &[],
            vault_total_assets: 0,
        });
        // Oracle tries to refresh (first tick) but has no feeds → error.
        assert!(matches!(
            report.oracle,
            Some(Err(AggregationError::TooFewFreshFeeds { .. }))
        ));
        assert!(report.liquidation.records.is_empty());
        assert!(report.adl.is_none());
        assert_eq!(report.vault_total_assets, 0);
    }

    // ─── tick: oracle cadence ──────────────────────────────────────

    #[test]
    fn tick_refreshes_oracle_at_first_tick_then_waits_interval() {
        let mut node = default_node();
        node.ingest_observation(
            PriceObservation::unsigned(FeedId(1), IndexPrice(100), 100),
            100,
        )
        .unwrap();
        node.ingest_observation(
            PriceObservation::unsigned(FeedId(2), IndexPrice(101), 100),
            100,
        )
        .unwrap();
        // Tick at t=100: first refresh fires.
        let r1 = node.tick(TickInput {
            block_height: 1,
            block_time: 100,
            mark: MarkPrice(100),
            account_snapshots: &[],
            vault_total_assets: 0,
        });
        assert!(matches!(r1.oracle, Some(Ok(_))));
        // Tick at t=105 (< 12s interval): no refresh.
        let r2 = node.tick(TickInput {
            block_height: 2,
            block_time: 105,
            mark: MarkPrice(100),
            account_snapshots: &[],
            vault_total_assets: 0,
        });
        assert!(r2.oracle.is_none(), "expected no refresh inside interval");
        // Tick at t=112 (exactly at boundary): refresh fires again.
        // We need a fresh observation though — old ones are 12s stale
        // relative to t=112 with the 60s default staleness window, so
        // they're still in range. Refresh should succeed.
        let r3 = node.tick(TickInput {
            block_height: 3,
            block_time: 112,
            mark: MarkPrice(100),
            account_snapshots: &[],
            vault_total_assets: 0,
        });
        assert!(matches!(r3.oracle, Some(Ok(_))));
    }

    // ─── tick: liquidation + ADL composition ───────────────────────

    #[test]
    fn tick_runs_liquidation_then_adl_on_unfilled_deficit() {
        // Mark = 80; entry = 100.
        // Long 1, $10 coll → pnl = -20, equity = -10 → underwater.
        // Short -1, $50 coll → pnl = +20, equity = 70 → profitable ADL victim.
        let mut node = default_node();
        let accounts = vec![snapshot(1, 1, 100, 10), snapshot(2, -1, 100, 50)];
        let report = node.tick(TickInput {
            block_height: 1,
            block_time: 100,
            mark: MarkPrice(80),
            account_snapshots: &accounts,
            vault_total_assets: 0,
        });

        // Liquidation: underwater long force-closed; fund empty → deficit.
        assert!(report.liquidation.unfilled_deficit > 0);
        // ADL: ran on the deficit, ate into the winner.
        let adl = report.adl.as_ref().expect("ADL should have fired");
        assert!(!adl.records.is_empty(), "ADL should have records");
        assert_eq!(adl.records[0].account, rdk_clob::AccountId(2));
        // Conservation: absorbed + remaining = the original deficit.
        assert_eq!(
            adl.deficit_absorbed + adl.deficit_remaining,
            report.liquidation.unfilled_deficit
        );
    }

    #[test]
    fn tick_skips_adl_when_config_opts_out() {
        let mut config = PrincepsNodeConfig::hyperliquid_default();
        config.run_adl_on_unfilled_deficit = false;
        let mut node = PrincepsNode::new(config);
        let accounts = vec![snapshot(1, 1, 100, 10)]; // underwater
        let report = node.tick(TickInput {
            block_height: 1,
            block_time: 100,
            mark: MarkPrice(80),
            account_snapshots: &accounts,
            vault_total_assets: 0,
        });
        assert!(report.liquidation.unfilled_deficit > 0);
        assert!(report.adl.is_none());
    }

    // ─── tick: vault mark-to-market ────────────────────────────────

    #[test]
    fn tick_marks_vault_to_market() {
        let mut node = default_node();
        node.vault_mut().deposit(1_000).unwrap();
        let report = node.tick(TickInput {
            block_height: 1,
            block_time: 100,
            mark: MarkPrice(100),
            account_snapshots: &[],
            vault_total_assets: 1_200,
        });
        assert_eq!(report.vault_total_assets, 1_200);
        assert_eq!(report.vault_total_shares, 1_000, "shares unchanged");
        // 1_200 × 10_000 / 1_000 = 12_000 bps (1.2×)
        assert_eq!(report.vault_share_price_bps, Some(12_000));
    }

    #[test]
    fn tick_vault_insolvent_when_marked_negative() {
        let mut node = default_node();
        node.vault_mut().deposit(1_000).unwrap();
        let report = node.tick(TickInput {
            block_height: 1,
            block_time: 100,
            mark: MarkPrice(100),
            account_snapshots: &[],
            vault_total_assets: -50,
        });
        assert_eq!(report.vault_total_assets, -50);
        assert_eq!(report.vault_share_price_bps, None);
        assert!(node.vault().is_insolvent());
    }

    // ─── determinism ───────────────────────────────────────────────

    #[test]
    fn tick_is_deterministic() {
        let make = || {
            let mut n = PrincepsNode::with_insurance_fund(
                PrincepsNodeConfig::hyperliquid_default(),
                InsuranceFund::new(1_000),
            );
            n.vault_mut().deposit(500).unwrap();
            n
        };
        let mut node_a = make();
        let mut node_b = make();
        let accounts = vec![snapshot(1, 1, 100, 10), snapshot(2, -1, 100, 50)];
        let input = TickInput {
            block_height: 1,
            block_time: 100,
            mark: MarkPrice(80),
            account_snapshots: &accounts,
            vault_total_assets: 500,
        };
        let r_a = node_a.tick(input);
        let r_b = node_b.tick(input);
        assert_eq!(r_a, r_b);
    }

    // ===== Lending bad-debt routing =====

    #[test]
    fn absorb_lending_bad_debt_covered_when_fund_has_capacity() {
        let mut node = PrincepsNode::with_insurance_fund(
            PrincepsNodeConfig::hyperliquid_default(),
            InsuranceFund::new(1_000),
        );
        let outcome = node.absorb_lending_bad_debt(300);
        assert!(matches!(outcome, WithdrawOutcome::Covered { amount: 300 }));
        assert_eq!(node.scanner().fund_balance(), 700);
    }

    #[test]
    fn absorb_lending_bad_debt_partially_drained_at_boundary() {
        let mut node = PrincepsNode::with_insurance_fund(
            PrincepsNodeConfig::hyperliquid_default(),
            InsuranceFund::new(100),
        );
        let outcome = node.absorb_lending_bad_debt(300);
        assert!(matches!(
            outcome,
            WithdrawOutcome::PartiallyDrained {
                amount: 100,
                unfilled: 200,
            }
        ));
        assert_eq!(node.scanner().fund_balance(), 0);
    }

    #[test]
    fn absorb_lending_bad_debt_depleted_when_fund_empty() {
        let mut node = PrincepsNode::with_insurance_fund(
            PrincepsNodeConfig::hyperliquid_default(),
            InsuranceFund::new(0),
        );
        let outcome = node.absorb_lending_bad_debt(500);
        assert!(matches!(outcome, WithdrawOutcome::Depleted { unfilled: 500 }));
        assert_eq!(node.scanner().fund_balance(), 0);
    }

    #[test]
    fn absorb_lending_bad_debt_zero_shortfall_is_noop() {
        let mut node = PrincepsNode::with_insurance_fund(
            PrincepsNodeConfig::hyperliquid_default(),
            InsuranceFund::new(500),
        );
        let outcome = node.absorb_lending_bad_debt(0);
        assert!(matches!(outcome, WithdrawOutcome::Covered { amount: 0 }));
        assert_eq!(node.scanner().fund_balance(), 500);
    }

    // ─── circuit breaker: deviation guard ──────────────────────────

    fn agg(index: u64, computed_at: u64) -> AggregatedPrice {
        AggregatedPrice {
            index: IndexPrice(index),
            computed_at,
            feeds_used: 2,
        }
    }

    #[test]
    fn circuit_breaker_disabled_never_trips() {
        let mut node = default_node();
        let prior = Some(IndexPrice(100));
        let fresh = Some(agg(1_000_000, 0)); // 10000% move — would normally trip
        // Override config to disabled params.
        node.config.circuit_breaker_params = CircuitBreakerParams::disabled();
        let tripped = node.evaluate_circuit_breaker(prior, fresh, 100);
        assert!(!tripped);
        assert_eq!(node.oracle_halt_until, None);
        assert!(!node.is_oracle_halted(100));
    }

    #[test]
    fn circuit_breaker_no_prior_price_does_not_trip() {
        let mut node = default_node();
        let prior = None; // no cached price
        let fresh = Some(agg(100, 0));
        let tripped = node.evaluate_circuit_breaker(prior, fresh, 100);
        assert!(!tripped);
        assert_eq!(node.oracle_halt_until, None);
    }

    #[test]
    fn circuit_breaker_failed_refresh_does_not_trip() {
        let mut node = default_node();
        let prior = Some(IndexPrice(100));
        let fresh = None; // refresh did not produce a successful aggregation
        let tripped = node.evaluate_circuit_breaker(prior, fresh, 100);
        assert!(!tripped);
        assert_eq!(node.oracle_halt_until, None);
    }

    #[test]
    fn circuit_breaker_small_move_within_cap_does_not_trip() {
        let mut node = default_node();
        // hyperliquid_default cap is 2000 bps = 20%. 100 → 119 is 19%.
        let prior = Some(IndexPrice(100));
        let fresh = Some(agg(119, 0));
        let tripped = node.evaluate_circuit_breaker(prior, fresh, 100);
        assert!(!tripped);
        assert_eq!(node.oracle_halt_until, None);
    }

    #[test]
    fn circuit_breaker_large_move_trips_and_sets_halt() {
        let mut node = default_node();
        // 100 → 130 is 30%, above the default 20% cap.
        let prior = Some(IndexPrice(100));
        let fresh = Some(agg(130, 0));
        let tripped = node.evaluate_circuit_breaker(prior, fresh, 1_000);
        assert!(tripped);
        assert_eq!(
            node.oracle_halt_until,
            Some(1_000 + CircuitBreakerParams::v0_default().halt_duration_blocks),
        );
        assert!(node.is_oracle_halted(1_000));
        assert!(node.is_oracle_halted(1_050));
        assert!(!node.is_oracle_halted(1_051));
    }

    #[test]
    fn circuit_breaker_sustained_spike_extends_halt() {
        let mut node = default_node();
        let prior = Some(IndexPrice(100));
        // First trip at block 1000.
        node.evaluate_circuit_breaker(prior, Some(agg(130, 0)), 1_000);
        let first_halt = node.oracle_halt_until.expect("first trip");
        // Second trip at block 1010 (still within first halt window).
        // Halt extends rather than expires mid-attack.
        node.evaluate_circuit_breaker(Some(IndexPrice(130)), Some(agg(170, 0)), 1_010);
        let second_halt = node.oracle_halt_until.expect("second trip");
        assert!(second_halt > first_halt);
    }

    #[test]
    fn circuit_breaker_downward_move_also_trips() {
        let mut node = default_node();
        // 100 → 70 is 30% down — must trip just like 30% up.
        let prior = Some(IndexPrice(100));
        let fresh = Some(agg(70, 0));
        let tripped = node.evaluate_circuit_breaker(prior, fresh, 500);
        assert!(tripped);
        assert!(node.is_oracle_halted(500));
    }

    #[test]
    fn circuit_breaker_zero_prior_does_not_trip() {
        let mut node = default_node();
        // Pathological prior=0 must not divide-by-zero / panic.
        let prior = Some(IndexPrice(0));
        let fresh = Some(agg(100, 0));
        let tripped = node.evaluate_circuit_breaker(prior, fresh, 100);
        assert!(!tripped);
    }

    #[test]
    fn circuit_breaker_persists_across_snapshot_round_trip() {
        let mut node = default_node();
        node.evaluate_circuit_breaker(Some(IndexPrice(100)), Some(agg(130, 0)), 1_000);
        let snap = node.snapshot();
        assert_eq!(snap.oracle_halt_until, Some(1_050));

        let bytes = serde_json::to_vec(&snap).expect("serialize");
        let decoded: CoordinatorSnapshot =
            serde_json::from_slice(&bytes).expect("deserialize");
        assert_eq!(decoded.oracle_halt_until, Some(1_050));

        let mut fresh = default_node();
        assert!(!fresh.is_oracle_halted(1_000));
        fresh.load_snapshot(decoded);
        assert!(fresh.is_oracle_halted(1_000));
        assert!(fresh.is_oracle_halted(1_050));
        assert!(!fresh.is_oracle_halted(1_051));
    }
}

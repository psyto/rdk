// Bin-crate `unreachable_pub` triggers on every public item here since
// `openhl` has no library surface. Silence at the module level — same
// pattern as `rpc.rs` / `seed_fixture.rs` / `chain_history.rs`.
#![allow(unreachable_pub)]

//! Sandbox scenario surface.
//!
//! A *scenario* is a chain-history fixture wrapped with metadata so a
//! non-engineer can discover, inspect, and run it from the CLI. This
//! module is the entry point for the `scenario` subcommand family
//! described in `fabrknt/website/SANDBOX-PATTERN.md` (the five-element
//! spec: pre-baked scenarios / business-readable output / parameter
//! dial / replay / CTA).
//!
//! v1: in-process execution via [`run_embedded`] — constructs a
//! `LiveRethEvmBridge<()>` (the unit-provider variant used in
//! `chain_history` tests), applies the scenario's chain-history
//! events block by block, ticks the [`OpenHlNode`] coordinator
//! between events, and renders a headline + per-block timeline +
//! before/after account delta.
//!
//! v0 (legacy [`render_run_v0`]): printed the equivalent
//! `reth-devnet --chain-history …` invocation. Still exported for
//! callers that want the dry-run flavour without running.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rdk_clearing::Account;
use rdk_funding::MarkPrice;
use rdk_liquidation::AccountSnapshot;
use openhl_evm::LiveRethEvmBridge;
use openhl_node::{OpenHlNode, OpenHlNodeConfig, TickInput};
use reth_chainspec::ChainSpec;
use serde::{Deserialize, Serialize};

use crate::chain_history::{ChainHistory, ChainHistoryApplier};

/// Wire shape of a scenario file. Reads as standard JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scenario {
    /// Slug (kebab-case, no spaces). Should match the file stem.
    pub name: String,
    /// One-word category for grouping in `scenario list`.
    pub category: String,
    /// One-paragraph business-language description. Read by
    /// `scenario show` and echoed in the `run` banner.
    pub description: String,
    /// One-line outcome statement. Echoed by `run` as the headline.
    pub headline: String,
    /// Default execution parameters. CLI flags can override.
    #[serde(default)]
    pub params: ScenarioParams,
    /// Per-block events. Same shape as `--chain-history`.
    pub history: ChainHistory,
    /// v2 Phase 3: optional declarative outcome checks. When present,
    /// each check is evaluated against the captured execution result
    /// and rendered as `✓` / `✗` in the OUTCOMES section. When all
    /// checks pass, the HEADLINE gets a ✓ badge; when any fail, ⚠.
    /// When no checks are declared the HEADLINE is "(unverified)".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expected_outcomes: Vec<ExpectedOutcome>,
}

/// v2 Phase 3 — one declarative outcome a scenario claims to demonstrate.
/// `check` is engine-specific; for openhl it is an [`OpenHlCheck`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpectedOutcome {
    pub name: String,
    pub description: String,
    pub check: OpenHlCheck,
}

/// Engine-specific check schema for openhl. JSON-serialized as
/// externally-tagged so authors write `{"liquidations_min": 4}` rather
/// than `{"kind": "liquidations_min", "value": 4}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenHlCheck {
    /// Assert total liquidation scan-hits across all blocks is at least N.
    LiquidationsMin(usize),
    /// Assert total liquidation scan-hits is at most N.
    LiquidationsMax(usize),
    /// Assert total fills produced across all blocks is at least N.
    FillsMin(usize),
    /// Assert total fills produced is at most N.
    FillsMax(usize),
    /// Assert exact final account count (after all blocks).
    FinalAccountCount(usize),
    /// Assert the final-block mark is at most N.
    FinalMarkMax(u64),
    /// Assert the final-block mark is at least N.
    FinalMarkMin(u64),
    /// Assert a specific account's final collateral equals `expected`.
    AccountCollateral { account: u64, expected: i64 },
    /// Assert a specific account's final position size equals `expected`.
    AccountPosition { account: u64, expected: i64 },
    /// Assert a specific account's final avg_entry equals `expected`.
    /// Useful for verifying the clearing layer's VWAP computation
    /// across multiple fills.
    AccountAvgEntry { account: u64, expected: u64 },
}

/// Per-outcome evaluation status used in the OUTCOMES section.
#[derive(Debug, Clone)]
pub enum OutcomeStatus {
    Pass,
    Fail(String),
}

/// Default parameters baked into the scenario file. Each is optional;
/// missing fields fall through to the binary's compiled defaults
/// (`OpenHlNodeConfig::hyperliquid_default` and
/// `LiquidationParams::hyperliquid_default`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScenarioParams {
    /// Suggested `--rounds` for this scenario.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rounds: Option<u64>,
    /// Initial margin in basis points. Default 1000 (10%).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_margin_bps: Option<u32>,
    /// Maintenance margin in basis points. Default 200 (2%).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maintenance_margin_bps: Option<u32>,
    /// Liquidation fee in basis points. Default 150 (1.5%).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub liquidation_fee_bps: Option<u32>,
}

/// Parse a scenario JSON file.
pub fn load_from_path(path: &Path) -> eyre::Result<Scenario> {
    let bytes = fs::read(path)
        .map_err(|e| eyre::eyre!("scenario {}: {e}", path.display()))?;
    let scenario: Scenario = serde_json::from_slice(&bytes)
        .map_err(|e| eyre::eyre!("scenario {} parse: {e}", path.display()))?;
    Ok(scenario)
}

/// Enumerate every `*.json` in `dir`. Sorted by filename.
pub fn list_in(dir: &Path) -> eyre::Result<Vec<PathBuf>> {
    if !dir.exists() {
        return Err(eyre::eyre!(
            "scenarios directory not found: {}\n\
            run `openhl scenario list` from the openhl repo root, or pass --dir explicitly.",
            dir.display()
        ));
    }
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    paths.sort();
    Ok(paths)
}

/// Render `scenario list` output. ASCII-formatted table.
pub fn render_list(scenarios: &[(PathBuf, Scenario)]) -> String {
    if scenarios.is_empty() {
        return "No scenarios found.\n".to_string();
    }
    let name_w = scenarios.iter().map(|(_, s)| s.name.len()).max().unwrap_or(8).max(4);
    let cat_w = scenarios
        .iter()
        .map(|(_, s)| s.category.len())
        .max()
        .unwrap_or(8)
        .max(8);

    let mut out = String::new();
    out.push_str(&format!(
        "{:<name_w$}  {:<cat_w$}  Headline\n",
        "Name",
        "Category",
        name_w = name_w,
        cat_w = cat_w,
    ));
    out.push_str(&format!(
        "{:-<name_w$}  {:-<cat_w$}  {:-<60}\n",
        "",
        "",
        "",
        name_w = name_w,
        cat_w = cat_w,
    ));
    for (_, s) in scenarios {
        out.push_str(&format!(
            "{:<name_w$}  {:<cat_w$}  {}\n",
            s.name,
            s.category,
            s.headline,
            name_w = name_w,
            cat_w = cat_w,
        ));
    }
    out.push_str(&cta_footer());
    out
}

/// Render `scenario show <name>` output.
pub fn render_show(scenario: &Scenario, path: &Path) -> String {
    let mut out = String::new();
    out.push_str(&format!("─── {} ────────────────────────────────────\n", scenario.headline));
    out.push_str(&format!("name        : {}\n", scenario.name));
    out.push_str(&format!("category    : {}\n", scenario.category));
    out.push_str(&format!("source      : {}\n\n", path.display()));
    out.push_str("description :\n");
    for line in scenario.description.lines() {
        out.push_str(&format!("  {line}\n"));
    }
    out.push_str("\nparams:\n");
    out.push_str(&format!(
        "  rounds                 : {}\n",
        scenario.params.rounds.map_or("(default)".to_string(), |v| v.to_string())
    ));
    out.push_str(&format!(
        "  initial_margin_bps     : {}\n",
        scenario.params.initial_margin_bps.map_or("1000 (default)".to_string(), |v| v.to_string())
    ));
    out.push_str(&format!(
        "  maintenance_margin_bps : {}\n",
        scenario.params.maintenance_margin_bps.map_or("200 (default)".to_string(), |v| v.to_string())
    ));
    out.push_str(&format!(
        "  liquidation_fee_bps    : {}\n",
        scenario.params.liquidation_fee_bps.map_or("150 (default)".to_string(), |v| v.to_string())
    ));
    out.push_str(&format!(
        "\nhistory: {} block(s), heights {}\n",
        scenario.history.blocks.len(),
        scenario
            .history
            .blocks
            .iter()
            .map(|b| b.height.to_string())
            .collect::<Vec<_>>()
            .join(", "),
    ));
    out.push_str(&cta_footer());
    out
}

/// Per-run dial overrides supplied by the CLI. Each field is optional;
/// `None` means "use the value baked into the scenario JSON's `params`
/// block".
#[derive(Debug, Clone, Copy, Default)]
pub struct DialOverrides {
    pub rounds: Option<u64>,
    pub initial_margin_bps: Option<u32>,
    pub maintenance_margin_bps: Option<u32>,
    pub liquidation_fee_bps: Option<u32>,
}

/// v1 embedded execution: actually run the scenario in-process and
/// render the observed state. CLI [`DialOverrides`] take precedence
/// over scenario JSON `params`, which in turn take precedence over
/// the engine's compiled defaults.
///
/// Constructs a `LiveRethEvmBridge<()>` (no Reth boot), applies the
/// scenario's per-block trades and deposits via [`ChainHistoryApplier`],
/// ticks [`OpenHlNode`] between events, and produces a headline +
/// per-block timeline + before/after account delta.
///
/// Liquidation cascades require oracle observations (driven from
/// outside the chain-history format); v1 surfaces account snapshots
/// into the tick, but oracle ingest is not yet driven from the
/// scenario JSON, so headline outcomes that depend on oracle (e.g.
/// "Bob liquidated when oracle drops to 102") are reported as
/// "curator claim" alongside the observed state. Oracle drive in the
/// JSON format lands in v2.
pub fn run_embedded(scenario: &Scenario, dials: &DialOverrides) -> eyre::Result<String> {
    // Build coordinator config: CLI dials take precedence over JSON
    // params over compiled defaults.
    let mut config = OpenHlNodeConfig::hyperliquid_default();
    if let Some(im) = dials.initial_margin_bps.or(scenario.params.initial_margin_bps) {
        config.liquidation_params.initial_margin_bps = im;
    }
    if let Some(mm) = dials.maintenance_margin_bps.or(scenario.params.maintenance_margin_bps) {
        config.liquidation_params.maintenance_margin_bps = mm;
    }
    if let Some(lf) = dials.liquidation_fee_bps.or(scenario.params.liquidation_fee_bps) {
        config.liquidation_params.liquidation_fee_bps = lf;
    }
    let mut coordinator = OpenHlNode::new(config);

    // Build the bridge with matching margin params so on-chain and
    // off-chain reads agree on the threshold.
    let chain_spec = Arc::new(ChainSpec::default());
    let bridge = LiveRethEvmBridge::new((), chain_spec)
        .with_liquidation_params(config.liquidation_params);

    let applier = ChainHistoryApplier::new(scenario.history.clone())?;

    // Initial state — should be empty before any block applies.
    let initial = bridge.accounts_snapshot();

    let max_height = applier.heights().iter().max().copied().unwrap_or(0);
    let rounds = dials
        .rounds
        .or(scenario.params.rounds)
        .unwrap_or(max_height.saturating_add(2))
        .max(max_height);

    let mut timeline: Vec<BlockSummary> = Vec::with_capacity(rounds as usize);
    for height in 1..=rounds {
        // Apply this block's events (if any) BEFORE the tick — same
        // ordering as `bin/openhl reth-devnet`'s commit hook.
        let counts = applier.apply_for_height(&bridge, height)?;

        // Snapshot for tick. Conversion is field-by-field (Stage 16c
        // comment in main.rs).
        let snapshots: Vec<AccountSnapshot> = bridge
            .accounts_snapshot()
            .into_iter()
            .map(|a| AccountSnapshot {
                account: a.account,
                position_size: a.position_size,
                avg_entry: a.avg_entry,
                collateral: a.collateral,
            })
            .collect();

        let (mark, mark_source) = match bridge.current_mark() {
            Some(m) => (m, "clob"),
            None => (MarkPrice(100), "stub-empty-book"),
        };

        let report = coordinator.tick(TickInput {
            block_height: height,
            block_time: 1000_u64.saturating_mul(height),
            mark,
            account_snapshots: &snapshots,
            vault_total_assets: coordinator.vault().total_assets().0,
        });

        timeline.push(BlockSummary {
            height,
            trades_applied: counts.map_or(0, |c| c.0),
            fills_produced: counts.map_or(0, |c| c.1),
            deposits_applied: counts.map_or(0, |c| c.2),
            mark: mark.0,
            mark_source,
            liquidations: report.liquidation.records.len(),
            adl_fired: report.adl.is_some(),
            funding_fired: report.funding.is_some(),
        });
    }

    let final_accounts = bridge.accounts_snapshot();

    Ok(render_embedded_output(scenario, &initial, &final_accounts, &timeline))
}

/// Per-block summary captured during embedded execution.
pub struct BlockSummary {
    height: u64,
    trades_applied: usize,
    fills_produced: usize,
    deposits_applied: usize,
    mark: u64,
    mark_source: &'static str,
    liquidations: usize,
    adl_fired: bool,
    funding_fired: bool,
}

/// Evaluate one [`OpenHlCheck`] against the captured run state.
pub fn evaluate_openhl_check(
    check: &OpenHlCheck,
    final_accounts: &[Account],
    timeline: &[BlockSummary],
) -> OutcomeStatus {
    let total_liquidations: usize = timeline.iter().map(|b| b.liquidations).sum();
    let total_fills: usize = timeline.iter().map(|b| b.fills_produced).sum();
    let final_mark = timeline.last().map(|b| b.mark).unwrap_or(0);

    match check {
        OpenHlCheck::LiquidationsMin(min) => {
            if total_liquidations >= *min {
                OutcomeStatus::Pass
            } else {
                OutcomeStatus::Fail(format!(
                    "observed total liquidation scan-hits = {total_liquidations} (expected ≥ {min})"
                ))
            }
        }
        OpenHlCheck::LiquidationsMax(max) => {
            if total_liquidations <= *max {
                OutcomeStatus::Pass
            } else {
                OutcomeStatus::Fail(format!(
                    "observed total liquidation scan-hits = {total_liquidations} (expected ≤ {max})"
                ))
            }
        }
        OpenHlCheck::FillsMin(min) => {
            if total_fills >= *min {
                OutcomeStatus::Pass
            } else {
                OutcomeStatus::Fail(format!(
                    "observed total fills = {total_fills} (expected ≥ {min})"
                ))
            }
        }
        OpenHlCheck::FillsMax(max) => {
            if total_fills <= *max {
                OutcomeStatus::Pass
            } else {
                OutcomeStatus::Fail(format!(
                    "observed total fills = {total_fills} (expected ≤ {max})"
                ))
            }
        }
        OpenHlCheck::FinalAccountCount(expected) => {
            if final_accounts.len() == *expected {
                OutcomeStatus::Pass
            } else {
                OutcomeStatus::Fail(format!(
                    "observed final account count = {}",
                    final_accounts.len()
                ))
            }
        }
        OpenHlCheck::FinalMarkMax(max) => {
            if final_mark <= *max {
                OutcomeStatus::Pass
            } else {
                OutcomeStatus::Fail(format!(
                    "observed final mark = {final_mark} (expected ≤ {max})"
                ))
            }
        }
        OpenHlCheck::FinalMarkMin(min) => {
            if final_mark >= *min {
                OutcomeStatus::Pass
            } else {
                OutcomeStatus::Fail(format!(
                    "observed final mark = {final_mark} (expected ≥ {min})"
                ))
            }
        }
        OpenHlCheck::AccountCollateral { account, expected } => {
            match final_accounts.iter().find(|a| a.account.0 == *account) {
                Some(a) if a.collateral.0 == *expected => OutcomeStatus::Pass,
                Some(a) => OutcomeStatus::Fail(format!(
                    "account {account} collateral = {} (expected {expected})",
                    a.collateral.0
                )),
                None => OutcomeStatus::Fail(format!("account {account} not in final state")),
            }
        }
        OpenHlCheck::AccountPosition { account, expected } => {
            match final_accounts.iter().find(|a| a.account.0 == *account) {
                Some(a) if a.position_size.0 == *expected => OutcomeStatus::Pass,
                Some(a) => OutcomeStatus::Fail(format!(
                    "account {account} position = {} (expected {expected})",
                    a.position_size.0
                )),
                None => OutcomeStatus::Fail(format!("account {account} not in final state")),
            }
        }
        OpenHlCheck::AccountAvgEntry { account, expected } => {
            match final_accounts.iter().find(|a| a.account.0 == *account) {
                Some(a) if a.avg_entry.0 == *expected => OutcomeStatus::Pass,
                Some(a) => OutcomeStatus::Fail(format!(
                    "account {account} avg_entry = {} (expected {expected})",
                    a.avg_entry.0
                )),
                None => OutcomeStatus::Fail(format!("account {account} not in final state")),
            }
        }
    }
}

fn render_embedded_output(
    scenario: &Scenario,
    initial: &[Account],
    final_accounts: &[Account],
    timeline: &[BlockSummary],
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "─── scenario: {} ────────────────────────────────────\n",
        scenario.name
    ));

    // Evaluate expected_outcomes for the HEADLINE badge.
    let evaluated: Vec<(&ExpectedOutcome, OutcomeStatus)> = scenario
        .expected_outcomes
        .iter()
        .map(|o| (o, evaluate_openhl_check(&o.check, final_accounts, timeline)))
        .collect();
    let any_failed = evaluated
        .iter()
        .any(|(_, s)| matches!(s, OutcomeStatus::Fail(_)));
    let has_outcomes = !evaluated.is_empty();

    if has_outcomes && !any_failed {
        out.push_str(&format!("HEADLINE ✓: {}\n\n", scenario.headline));
    } else if has_outcomes && any_failed {
        out.push_str(&format!("HEADLINE ⚠: {}\n\n", scenario.headline));
    } else {
        out.push_str(&format!("HEADLINE (unverified): {}\n\n", scenario.headline));
    }

    out.push_str("DESCRIPTION:\n");
    for line in scenario.description.lines() {
        out.push_str(&format!("  {line}\n"));
    }

    // Per-block timeline.
    out.push_str("\nTIMELINE (per-block):\n");
    out.push_str("  height  mark    src              trades  fills  deposits  liqs  adl  fund\n");
    out.push_str("  ------  ------  ---------------  ------  -----  --------  ----  ---  ----\n");
    for b in timeline {
        out.push_str(&format!(
            "  {:>6}  {:>6}  {:<15}  {:>6}  {:>5}  {:>8}  {:>4}  {:>3}  {:>4}\n",
            b.height,
            b.mark,
            b.mark_source,
            b.trades_applied,
            b.fills_produced,
            b.deposits_applied,
            b.liquidations,
            if b.adl_fired { "yes" } else { "—" },
            if b.funding_fired { "yes" } else { "—" },
        ));
    }

    // Account delta. Initial state is empty for fresh runs, so this is
    // effectively a "final state" table — but rendered as a delta so
    // re-runs against a non-empty bridge still display sanely.
    out.push_str("\nACCOUNT DELTA (final − initial):\n");
    if final_accounts.is_empty() {
        out.push_str("  (no accounts touched — chain-history had no deposits or matching trades)\n");
    } else {
        out.push_str("  account  collateral  position  avg_entry\n");
        out.push_str("  -------  ----------  --------  ---------\n");
        for acct in final_accounts {
            out.push_str(&format!(
                "  {:>7}  {:>10}  {:>8}  {:>9}\n",
                acct.account.0,
                acct.collateral.0,
                acct.position_size.0,
                acct.avg_entry.0,
            ));
        }
        // Echo initial count for completeness.
        out.push_str(&format!(
            "  (initial account count: {}, final account count: {})\n",
            initial.len(),
            final_accounts.len(),
        ));
    }

    // OUTCOMES section (replaces the prior "OBSERVED" line).
    out.push_str("\nOUTCOMES:\n");
    if evaluated.is_empty() {
        out.push_str("  (no expected_outcomes declared — HEADLINE shown as unverified)\n");
        // Keep the v1 observed-state summary inline so even unverified
        // runs surface the headline numbers.
        let total_liquidations: usize = timeline.iter().map(|b| b.liquidations).sum();
        let total_fills: usize = timeline.iter().map(|b| b.fills_produced).sum();
        out.push_str(&format!(
            "  Observed: {} fill(s) across {} block(s), {} liquidation scan-hit(s).\n",
            total_fills,
            timeline.len(),
            total_liquidations,
        ));
    } else {
        for (outcome, status) in &evaluated {
            match status {
                OutcomeStatus::Pass => out.push_str(&format!("  ✓ {}\n", outcome.description)),
                OutcomeStatus::Fail(why) => {
                    out.push_str(&format!("  ✗ {} ({why})\n", outcome.description));
                }
            }
        }
        let passed = evaluated
            .iter()
            .filter(|(_, s)| matches!(s, OutcomeStatus::Pass))
            .count();
        out.push_str(&format!("\n  {passed} of {} outcome(s) verified.\n", evaluated.len()));
    }

    out.push_str("\nNOTE: v1 runs the scenario in-process against a unit-provider\n");
    out.push_str("`LiveRethEvmBridge<()>` (no Reth boot). For the production-shape\n");
    out.push_str("run (real Reth + Malachite + JSON-RPC), use:\n");
    out.push_str(&format!(
        "  openhl reth-devnet --chain-history scenarios/{}.json --rounds {}\n",
        scenario.name,
        timeline.len(),
    ));

    out.push_str(&cta_footer());
    out
}

/// Render `scenario run <name>` output. v0 prints the equivalent
/// `reth-devnet` invocation; kept exported for the optional `--dry-run`
/// flag — the default `Run` action now goes through [`run_embedded`].
pub fn render_run_v0(
    scenario: &Scenario,
    path: &Path,
    rounds_override: Option<u64>,
) -> String {
    let rounds = rounds_override
        .or(scenario.params.rounds)
        .unwrap_or(5);

    let mut out = String::new();
    out.push_str(&format!(
        "─── scenario: {} ────────────────────────────────────\n",
        scenario.name
    ));
    out.push_str(&format!("HEADLINE: {}\n\n", scenario.headline));
    out.push_str("DESCRIPTION:\n");
    for line in scenario.description.lines() {
        out.push_str(&format!("  {line}\n"));
    }

    out.push_str("\nTO EXECUTE:\n");
    out.push_str(&format!("  openhl reth-devnet \\\n"));
    out.push_str(&format!("    --rounds {rounds} \\\n"));
    out.push_str(&format!("    --chain-history {} \\\n", path.display()));
    if let Some(im) = scenario.params.initial_margin_bps {
        // The bin currently consumes a composite --liquidation-params; once
        // surfaced piecewise (Task #8 dial element), individual flags land.
        out.push_str(&format!("    # initial_margin_bps={im} (set via --liquidation-params when surfaced)\n"));
    }
    if let Some(mm) = scenario.params.maintenance_margin_bps {
        out.push_str(&format!("    # maintenance_margin_bps={mm}\n"));
    }
    if let Some(lf) = scenario.params.liquidation_fee_bps {
        out.push_str(&format!("    # liquidation_fee_bps={lf}\n"));
    }

    out.push_str("\nINSPECTING THE RUN:\n");
    out.push_str("  # while devnet is running, query state via the openhl_* RPC namespace:\n");
    out.push_str("  curl -s -X POST -H 'Content-Type: application/json' \\\n");
    out.push_str("    --data '{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"openhl_accounts\",\"params\":[]}' \\\n");
    out.push_str("    http://127.0.0.1:8545\n");
    out.push_str("  # then drill into a specific account:\n");
    out.push_str("  curl -s -X POST -H 'Content-Type: application/json' \\\n");
    out.push_str("    --data '{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"openhl_marginHealth\",\"params\":[<account_id>]}' \\\n");
    out.push_str("    http://127.0.0.1:8545\n");

    out.push_str("\nNOTE: v0 of the scenario subcommand prints the equivalent reth-devnet\n");
    out.push_str("invocation rather than executing in-process. Embedded execution with\n");
    out.push_str("in-CLI headline + before/after rendering lands in v1.\n");

    out.push_str(&cta_footer());
    out
}

/// Three-option CTA block shown at the bottom of every scenario
/// output. Per `fabrknt/website/SANDBOX-PATTERN.md` element (5).
fn cta_footer() -> String {
    let mut out = String::new();
    out.push_str("\nNEXT:\n");
    out.push_str("  • Adopt this engine  : https://github.com/psyto/rdk\n");
    out.push_str("  • Custom build       : https://fabrknt.com/waitlist.html?product=evm-perp&intent=build\n");
    out.push_str("  • Hosted access      : https://fabrknt.com/waitlist.html?product=evm-perp&intent=hosted\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn minimal_scenario_json() -> &'static str {
        r#"{
            "name": "test-cascade",
            "category": "liquidation-cascade",
            "description": "A small cascade.",
            "headline": "Account 20 was liquidated when oracle dropped to 102.",
            "params": {
                "rounds": 5
            },
            "history": {
                "blocks": [
                    {
                        "height": 1,
                        "deposits": [{"account": 10, "amount": 1000}],
                        "trades": [
                            {"id": 1, "account": 10, "side": "Sell", "qty": 10, "kind": "Limit", "price": 110}
                        ]
                    }
                ]
            }
        }"#
    }

    #[test]
    fn scenario_round_trips() {
        let json = minimal_scenario_json();
        let s: Scenario = serde_json::from_str(json).expect("parse");
        assert_eq!(s.name, "test-cascade");
        assert_eq!(s.category, "liquidation-cascade");
        assert_eq!(s.params.rounds, Some(5));
        assert_eq!(s.history.blocks.len(), 1);
        assert_eq!(s.history.blocks[0].height, 1);
        assert_eq!(s.history.blocks[0].trades.len(), 1);
        assert_eq!(s.history.blocks[0].deposits.len(), 1);

        // Re-serialise and re-parse to confirm symmetry.
        let serialized = serde_json::to_string(&s).expect("serialize");
        let again: Scenario = serde_json::from_str(&serialized).expect("re-parse");
        assert_eq!(again.name, "test-cascade");
    }

    #[test]
    fn load_from_path_reads_real_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test-cascade.json");
        let mut f = std::fs::File::create(&path).expect("create");
        f.write_all(minimal_scenario_json().as_bytes()).expect("write");
        f.sync_all().expect("sync");

        let s = load_from_path(&path).expect("load");
        assert_eq!(s.name, "test-cascade");
    }

    #[test]
    fn list_in_returns_sorted() {
        let dir = tempfile::tempdir().expect("tempdir");
        for name in &["zulu.json", "alpha.json", "mike.json", "ignored.txt"] {
            std::fs::write(dir.path().join(name), "{}").expect("write");
        }
        let files = list_in(dir.path()).expect("list");
        let names: Vec<&str> = files
            .iter()
            .map(|p| p.file_name().and_then(|s| s.to_str()).unwrap_or(""))
            .collect();
        assert_eq!(names, vec!["alpha.json", "mike.json", "zulu.json"]);
    }

    #[test]
    fn render_list_shows_each_scenario_headline() {
        let s = serde_json::from_str::<Scenario>(minimal_scenario_json()).unwrap();
        let path = PathBuf::from("/tmp/test-cascade.json");
        let out = render_list(&[(path, s)]);
        assert!(out.contains("test-cascade"));
        assert!(out.contains("Account 20 was liquidated"));
        assert!(out.contains("NEXT:"));
        assert!(out.contains("fabrknt.com/waitlist"));
    }

    #[test]
    fn render_show_includes_params_and_cta() {
        let s = serde_json::from_str::<Scenario>(minimal_scenario_json()).unwrap();
        let path = PathBuf::from("/tmp/test-cascade.json");
        let out = render_show(&s, &path);
        assert!(out.contains("test-cascade"));
        assert!(out.contains("rounds                 : 5"));
        assert!(out.contains("initial_margin_bps     : 1000 (default)"));
        assert!(out.contains("NEXT:"));
    }

    #[test]
    fn render_run_v0_shows_invocation_and_cta() {
        let s = serde_json::from_str::<Scenario>(minimal_scenario_json()).unwrap();
        let path = PathBuf::from("scenarios/test-cascade.json");
        let out = render_run_v0(&s, &path, None);
        assert!(out.contains("HEADLINE:"));
        assert!(out.contains("openhl reth-devnet"));
        assert!(out.contains("--chain-history scenarios/test-cascade.json"));
        assert!(out.contains("NEXT:"));
    }

    #[test]
    fn render_run_v0_respects_rounds_override() {
        let s = serde_json::from_str::<Scenario>(minimal_scenario_json()).unwrap();
        let path = PathBuf::from("scenarios/test-cascade.json");
        let out = render_run_v0(&s, &path, Some(42));
        assert!(out.contains("--rounds 42"));
    }

    #[test]
    fn run_embedded_executes_and_renders_timeline() {
        let s: Scenario = serde_json::from_str(minimal_scenario_json()).unwrap();
        let out = run_embedded(&s, &DialOverrides::default()).expect("embedded run ok");
        // minimal fixture has no expected_outcomes → unverified badge
        assert!(out.contains("HEADLINE (unverified):"));
        assert!(out.contains("TIMELINE (per-block)"));
        assert!(out.contains("height  mark"));
        assert!(out.contains("ACCOUNT DELTA"));
        assert!(out.contains("OUTCOMES:"));
        assert!(out.contains("NEXT:"));
    }

    #[test]
    fn run_embedded_runs_at_least_max_height_blocks() {
        // The minimal fixture has one event at height 1; with no rounds
        // override and no rounds in params (the minimal fixture has
        // params.rounds = Some(5)), should run 5 blocks.
        let s: Scenario = serde_json::from_str(minimal_scenario_json()).unwrap();
        let out = run_embedded(&s, &DialOverrides::default()).expect("embedded run ok");
        // Five rows of timeline output (heights 1..=5).
        let timeline_rows = out.matches("       1 ").count()
            + out.matches("       2 ").count()
            + out.matches("       3 ").count()
            + out.matches("       4 ").count()
            + out.matches("       5 ").count();
        assert!(timeline_rows >= 5, "expected >=5 timeline rows, got:\n{out}");
    }

    #[test]
    fn run_embedded_creates_account_from_deposit() {
        let s: Scenario = serde_json::from_str(minimal_scenario_json()).unwrap();
        let out = run_embedded(&s, &DialOverrides::default()).expect("embedded run ok");
        // Minimal fixture deposits 1000 to account 10.
        assert!(out.contains("(initial account count: 0, final account count: 1)"));
        assert!(out.contains("      10"));
    }

    /// Phase 3: expected_outcomes parsing + evaluation.

    fn fake_timeline(liquidations: usize, fills: usize, mark: u64) -> Vec<BlockSummary> {
        vec![BlockSummary {
            height: 1,
            trades_applied: 0,
            fills_produced: fills,
            deposits_applied: 0,
            mark,
            mark_source: "clob",
            liquidations,
            adl_fired: false,
            funding_fired: false,
        }]
    }

    #[test]
    fn evaluate_openhl_check_liquidations_min() {
        let timeline = fake_timeline(4, 0, 96);
        assert!(matches!(
            evaluate_openhl_check(&OpenHlCheck::LiquidationsMin(4), &[], &timeline),
            OutcomeStatus::Pass
        ));
        assert!(matches!(
            evaluate_openhl_check(&OpenHlCheck::LiquidationsMin(5), &[], &timeline),
            OutcomeStatus::Fail(_)
        ));
    }

    #[test]
    fn evaluate_openhl_check_fills_bounds() {
        let timeline = fake_timeline(0, 4, 100);
        assert!(matches!(
            evaluate_openhl_check(&OpenHlCheck::FillsMin(4), &[], &timeline),
            OutcomeStatus::Pass
        ));
        assert!(matches!(
            evaluate_openhl_check(&OpenHlCheck::FillsMax(4), &[], &timeline),
            OutcomeStatus::Pass
        ));
        assert!(matches!(
            evaluate_openhl_check(&OpenHlCheck::FillsMax(3), &[], &timeline),
            OutcomeStatus::Fail(_)
        ));
    }

    #[test]
    fn evaluate_openhl_check_final_mark() {
        let timeline = fake_timeline(0, 0, 96);
        assert!(matches!(
            evaluate_openhl_check(&OpenHlCheck::FinalMarkMax(96), &[], &timeline),
            OutcomeStatus::Pass
        ));
        assert!(matches!(
            evaluate_openhl_check(&OpenHlCheck::FinalMarkMin(96), &[], &timeline),
            OutcomeStatus::Pass
        ));
        assert!(matches!(
            evaluate_openhl_check(&OpenHlCheck::FinalMarkMax(95), &[], &timeline),
            OutcomeStatus::Fail(_)
        ));
    }

    #[test]
    fn evaluate_openhl_check_account_position() {
        use rdk_clearing::Account;
        use rdk_clob::AccountId as ClobAccountId;
        use rdk_funding::{MarkPrice as MP, Notional, PositionSize as PS};

        let accounts = vec![Account {
            account: ClobAccountId(10),
            position_size: PS(10),
            avg_entry: MP(110),
            collateral: Notional(200),
        }];
        let timeline = fake_timeline(0, 0, 100);
        assert!(matches!(
            evaluate_openhl_check(
                &OpenHlCheck::AccountPosition { account: 10, expected: 10 },
                &accounts,
                &timeline
            ),
            OutcomeStatus::Pass
        ));
        assert!(matches!(
            evaluate_openhl_check(
                &OpenHlCheck::AccountPosition { account: 10, expected: 99 },
                &accounts,
                &timeline
            ),
            OutcomeStatus::Fail(_)
        ));
        assert!(matches!(
            evaluate_openhl_check(
                &OpenHlCheck::AccountPosition { account: 999, expected: 0 },
                &accounts,
                &timeline
            ),
            OutcomeStatus::Fail(_)
        ));
    }

    #[test]
    fn scenario_with_expected_outcomes_round_trips() {
        let json = r#"{
            "name": "test",
            "category": "stress",
            "description": "test",
            "headline": "test",
            "params": {"rounds": 3},
            "history": {"blocks": []},
            "expected_outcomes": [
                {
                    "name": "liq",
                    "description": "min 4 liquidations",
                    "check": {"liquidations_min": 4}
                },
                {
                    "name": "acct",
                    "description": "alice has +10",
                    "check": {"account_position": {"account": 10, "expected": 10}}
                }
            ]
        }"#;
        let s: Scenario = serde_json::from_str(json).expect("parse");
        assert_eq!(s.expected_outcomes.len(), 2);
        assert!(matches!(
            s.expected_outcomes[0].check,
            OpenHlCheck::LiquidationsMin(4)
        ));
    }

    #[test]
    fn scenario_without_expected_outcomes_still_parses() {
        let json = minimal_scenario_json();
        let s: Scenario = serde_json::from_str(json).expect("parse");
        assert!(s.expected_outcomes.is_empty());
    }

    #[test]
    fn run_embedded_renders_headline_badge_when_outcomes_pass() {
        let mut s: Scenario = serde_json::from_str(minimal_scenario_json()).unwrap();
        s.expected_outcomes = vec![ExpectedOutcome {
            name: "acct".to_string(),
            description: "account 10 exists with 1000 collateral".to_string(),
            check: OpenHlCheck::AccountCollateral {
                account: 10,
                expected: 1000,
            },
        }];
        let out = run_embedded(&s, &DialOverrides::default()).expect("ok");
        assert!(out.contains("HEADLINE ✓:"), "expected ✓ badge; got:\n{out}");
        assert!(out.contains("1 of 1 outcome(s) verified."));
    }

    #[test]
    fn run_embedded_renders_headline_warning_when_outcomes_fail() {
        let mut s: Scenario = serde_json::from_str(minimal_scenario_json()).unwrap();
        s.expected_outcomes = vec![ExpectedOutcome {
            name: "bad".to_string(),
            description: "deliberately fails".to_string(),
            check: OpenHlCheck::FillsMin(9999),
        }];
        let out = run_embedded(&s, &DialOverrides::default()).expect("ok");
        assert!(out.contains("HEADLINE ⚠:"), "expected ⚠ badge; got:\n{out}");
    }
}

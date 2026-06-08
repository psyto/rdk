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

/// v1 embedded execution: actually run the scenario in-process and
/// render the observed state.
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
pub fn run_embedded(scenario: &Scenario, rounds_override: Option<u64>) -> eyre::Result<String> {
    // Build coordinator config with scenario param overrides.
    let mut config = OpenHlNodeConfig::hyperliquid_default();
    if let Some(im) = scenario.params.initial_margin_bps {
        config.liquidation_params.initial_margin_bps = im;
    }
    if let Some(mm) = scenario.params.maintenance_margin_bps {
        config.liquidation_params.maintenance_margin_bps = mm;
    }
    if let Some(lf) = scenario.params.liquidation_fee_bps {
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
    let rounds = rounds_override
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
struct BlockSummary {
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
    out.push_str(&format!("HEADLINE (curator claim): {}\n\n", scenario.headline));

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

    let total_liquidations: usize = timeline.iter().map(|b| b.liquidations).sum();
    let total_fills: usize = timeline.iter().map(|b| b.fills_produced).sum();
    let observed_match = if total_liquidations > 0 {
        format!(
            "✓ liquidation scan flagged accounts ({} scan-hit(s); v1 does not \
             write the close back to the bridge, so the same accounts may be \
             re-flagged each tick — v2 will wire the write-back loop)",
            total_liquidations,
        )
    } else {
        "no liquidations flagged (this is the expected outcome when the chain-\
        history doesn't produce a mark-vs-entry gap large enough to cross the \
        maintenance margin)"
            .to_string()
    };
    out.push_str(&format!(
        "\nOBSERVED: {} fill(s) across {} block(s); {}.\n",
        total_fills,
        timeline.len(),
        observed_match,
    ));

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
        let out = run_embedded(&s, None).expect("embedded run ok");
        assert!(out.contains("HEADLINE (curator claim)"));
        assert!(out.contains("TIMELINE (per-block)"));
        assert!(out.contains("height  mark"));
        assert!(out.contains("ACCOUNT DELTA"));
        assert!(out.contains("OBSERVED"));
        assert!(out.contains("NEXT:"));
    }

    #[test]
    fn run_embedded_runs_at_least_max_height_blocks() {
        // The minimal fixture has one event at height 1; with no rounds
        // override and no rounds in params (the minimal fixture has
        // params.rounds = Some(5)), should run 5 blocks.
        let s: Scenario = serde_json::from_str(minimal_scenario_json()).unwrap();
        let out = run_embedded(&s, None).expect("embedded run ok");
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
        let out = run_embedded(&s, None).expect("embedded run ok");
        // Minimal fixture deposits 1000 to account 10.
        assert!(out.contains("(initial account count: 0, final account count: 1)"));
        assert!(out.contains("      10"));
    }
}

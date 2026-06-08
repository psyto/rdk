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
//! v0 (this commit): JSON wrapper + list / show / run renderers.
//! `run` prints the equivalent `reth-devnet --chain-history …`
//! invocation rather than executing in-process — embedded execution
//! lands in v1.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::chain_history::ChainHistory;

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
    pub initial_margin_bps: Option<u16>,
    /// Maintenance margin in basis points. Default 200 (2%).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maintenance_margin_bps: Option<u16>,
    /// Liquidation fee in basis points. Default 150 (1.5%).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub liquidation_fee_bps: Option<u16>,
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

/// Render `scenario run <name>` output. v0 prints the equivalent
/// `reth-devnet` invocation; embedded execution lands in v1.
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
}

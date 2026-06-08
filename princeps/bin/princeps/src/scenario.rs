// Bin-crate `unreachable_pub` would trigger on every public item here.
// Silence at the module level — same pattern as other bin modules.
#![allow(unreachable_pub)]

//! Sandbox scenario surface for princeps.
//!
//! A *scenario* is a metadata-wrapped recipe of CLI invocations that
//! demonstrate one cross-margin / lending behavior end-to-end. Unlike
//! `openhl`'s scenario format (which wraps a chain-history JSON because
//! openhl's matching engine is event-driven), princeps's value lives
//! in its CLI surface (`lending-demo`, `lending init/deposit/borrow/...`,
//! `socialize`), so scenarios are step-lists of CLI commands with
//! explanations.
//!
//! Surface contract per `fabrknt/website/SANDBOX-PATTERN.md`:
//! (1) pre-baked scenarios, (2) business-readable output, (3) parameter
//! dial (`--eth-crash-price` etc), (4) replay (CLI sequences are
//! reproducible), (5) CTA footer.
//!
//! v0 of `scenario run` prints the step list with explanations rather
//! than executing in-process. Embedded execution lands in v1.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scenario {
    pub name: String,
    pub category: String,
    pub description: String,
    pub headline: String,
    pub steps: Vec<ScenarioStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioStep {
    /// One-line explanation of what this step demonstrates.
    pub explanation: String,
    /// Exact CLI command to execute. Lines starting with `#` in the
    /// printed output are comments produced by the renderer.
    pub command: String,
    /// Optional substring the operator should look for in the step's
    /// output to confirm it ran correctly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<String>,
}

pub fn load_from_path(path: &Path) -> eyre::Result<Scenario> {
    let bytes = fs::read(path)
        .map_err(|e| eyre::eyre!("scenario {}: {e}", path.display()))?;
    let scenario: Scenario = serde_json::from_slice(&bytes)
        .map_err(|e| eyre::eyre!("scenario {} parse: {e}", path.display()))?;
    Ok(scenario)
}

pub fn list_in(dir: &Path) -> eyre::Result<Vec<PathBuf>> {
    if !dir.exists() {
        return Err(eyre::eyre!(
            "scenarios directory not found: {}\n\
            run `princeps scenario list` from the princeps repo root, or pass --dir explicitly.",
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
    out.push_str(&format!("\nsteps       : {} command(s)\n", scenario.steps.len()));
    for (i, step) in scenario.steps.iter().enumerate() {
        out.push_str(&format!("  [{}] {}\n", i + 1, step.explanation));
    }
    out.push_str(&cta_footer());
    out
}

pub fn render_run_v0(scenario: &Scenario, path: &Path) -> String {
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

    out.push_str(&format!("\nSTEPS ({} command(s)):\n", scenario.steps.len()));
    for (i, step) in scenario.steps.iter().enumerate() {
        out.push_str(&format!("\n  Step {} — {}\n", i + 1, step.explanation));
        out.push_str(&format!("    $ {}\n", step.command));
        if let Some(expect) = &step.expect {
            out.push_str(&format!("    # expect output to include: {expect}\n"));
        }
    }

    out.push_str(&format!("\nSOURCE: {}\n", path.display()));
    out.push_str("\nNOTE: v0 prints the step list rather than executing each command\n");
    out.push_str("in-process. Embedded execution with in-CLI headline + before/after\n");
    out.push_str("delta lands in v1.\n");

    out.push_str(&cta_footer());
    out
}

fn cta_footer() -> String {
    let mut out = String::new();
    out.push_str("\nNEXT:\n");
    out.push_str("  • Adopt this engine  : https://github.com/psyto/rdk\n");
    out.push_str("  • Custom build       : https://fabrknt.com/waitlist.html?product=evm-prime-broker&intent=build\n");
    out.push_str("  • Hosted access      : https://fabrknt.com/waitlist.html?product=evm-prime-broker&intent=hosted\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_scenario_json() -> &'static str {
        r#"{
            "name": "cross-margin-survival",
            "category": "lending-cross-margin",
            "description": "Alice deposits USDC and borrows ETH against it.",
            "headline": "Cross-margin saves Alice from a 10% ETH crash.",
            "steps": [
                {
                    "explanation": "Run the canonical cross-margin demo.",
                    "command": "princeps lending-demo --eth-crash-price 90",
                    "expect": "unified HEALTHY"
                }
            ]
        }"#
    }

    #[test]
    fn scenario_round_trips() {
        let s: Scenario = serde_json::from_str(minimal_scenario_json()).expect("parse");
        assert_eq!(s.name, "cross-margin-survival");
        assert_eq!(s.steps.len(), 1);
        assert_eq!(s.steps[0].expect.as_deref(), Some("unified HEALTHY"));

        let serialized = serde_json::to_string(&s).expect("serialize");
        let again: Scenario = serde_json::from_str(&serialized).expect("re-parse");
        assert_eq!(again.name, "cross-margin-survival");
    }

    #[test]
    fn render_list_contains_headline_and_cta() {
        let s: Scenario = serde_json::from_str(minimal_scenario_json()).unwrap();
        let path = PathBuf::from("/tmp/cm.json");
        let out = render_list(&[(path, s)]);
        assert!(out.contains("cross-margin-survival"));
        assert!(out.contains("10% ETH crash"));
        assert!(out.contains("NEXT:"));
        assert!(out.contains("evm-prime-broker"));
    }

    #[test]
    fn render_show_contains_step_summary() {
        let s: Scenario = serde_json::from_str(minimal_scenario_json()).unwrap();
        let path = PathBuf::from("/tmp/cm.json");
        let out = render_show(&s, &path);
        assert!(out.contains("steps       : 1 command(s)"));
        assert!(out.contains("Run the canonical cross-margin demo"));
        assert!(out.contains("NEXT:"));
    }

    #[test]
    fn render_run_v0_lists_each_step() {
        let s: Scenario = serde_json::from_str(minimal_scenario_json()).unwrap();
        let path = PathBuf::from("scenarios/cross-margin-survival.json");
        let out = render_run_v0(&s, &path);
        assert!(out.contains("HEADLINE:"));
        assert!(out.contains("Step 1 — Run the canonical cross-margin demo."));
        assert!(out.contains("$ princeps lending-demo --eth-crash-price 90"));
        assert!(out.contains("expect output to include: unified HEALTHY"));
        assert!(out.contains("NEXT:"));
    }
}

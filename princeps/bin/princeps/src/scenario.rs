// Bin-crate `unreachable_pub` would trigger on every public item here.
// Silence at the module level — same pattern as other bin modules.
#![allow(unreachable_pub)]

//! **Mirrored across sibling Fabrknt sandbox engines.** This module's
//! shape (Scenario JSON, list/show/run renderers, run_embedded with
//! sub-process spawn + stdio inherit, `has_shell_metacharacters`,
//! `EmbeddedReport`, CTA footer with `product=` waitlist enrichment)
//! is duplicated nearly verbatim in:
//!
//!   - psyto/openhl-solana → `scripts/scenario/src/main.rs`
//!   - psyto/ssr           → `cli/src/scenario.rs`
//!
//! When you change behavior shared across them (metachar detection
//! rules, headline rendering, JSON shape, CTA footer text), apply the
//! same change to all three. The decision to keep three copies rather
//! than extract to a shared crate (e.g. `fabrknt-scenario-runner`) is
//! deliberate: the engines live in different repos with no shared
//! workspace, so a crate would need crates.io publication + cross-repo
//! version coordination that doesn't yet pay for itself given the
//! small surface area. Revisit when adding the 4th subprocess-based
//! runner or when the shared surface grows.
//!
//! The 4th Fabrknt runner (`rdk/openhl`) is structurally different —
//! in-process execution via `LiveRethEvmBridge<()>` rather than
//! sub-process spawn — so it shares only the Scenario JSON shape and
//! the CTA footer with these three.
//!
//! ---
//!
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
//! v1 of `scenario run` spawns the current `princeps` binary as a
//! sub-process for each step (commands whose first token is
//! `princeps` are routed to `std::env::current_exe()`), inheriting
//! stdio so the step's own output streams live to the operator. The
//! scenario wrapper adds a headline header, per-step separators, and
//! a final verdict + CTA. Non-`princeps` steps (comments / off-CLI
//! hints) are skipped and printed as informational lines.
//!
//! v0 of `scenario run` printed only the step list — see
//! [`render_run_v0`], still exported for the optional `--dry-run`
//! flag.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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

/// Dispatch target for a step whose command can be served in-process
/// (no sub-process spawn). v2 walks every step trying to parse one of
/// these; if every step matches, the runner takes the v2 path and
/// renders the structured HEADLINE / TIMELINE / DELTA / OUTCOMES /
/// NEXT contract from `SANDBOX-PATTERN.md`. Otherwise the runner
/// falls back to v1 (sub-process spawn with stdio inherit).
#[derive(Debug, Clone, Copy)]
pub enum InProcessTarget {
    /// `princeps lending-demo --eth-crash-price <N>`
    LendingDemo { eth_crash_price: u128 },
}

/// Try to parse a step's command into an in-process target. Returns
/// `None` for commands that must still spawn a sub-process.
pub fn try_parse_in_process(command: &str) -> Option<InProcessTarget> {
    let trimmed = command.trim();
    let prefix = "princeps lending-demo --eth-crash-price ";
    if let Some(rest) = trimmed.strip_prefix(prefix) {
        let n: u128 = rest.trim().parse().ok()?;
        return Some(InProcessTarget::LendingDemo { eth_crash_price: n });
    }
    None
}

/// True iff every step in `scenario` can be served in-process. Drives
/// the v2 vs v1 path selection.
pub fn is_v2_eligible(scenario: &Scenario) -> bool {
    !scenario.steps.is_empty()
        && scenario
            .steps
            .iter()
            .all(|s| try_parse_in_process(&s.command).is_some())
}

/// Result of running a single in-process step. Aggregated across all
/// steps and consumed by the v2 renderer.
#[derive(Debug, Clone)]
enum StepResult {
    LendingDemo(crate::LendingDemoResult),
}

/// Embedded execution. For v2-eligible scenarios (every step matches
/// an [`InProcessTarget`]), dispatches in-process, captures structured
/// results, and emits the 5-section v2 output contract. For other
/// scenarios falls back to v1: spawn each step's command as a
/// sub-process with stdio inherited.
///
/// Returns the per-step status set. The v1 path streams step output
/// to the operator's terminal directly; the v2 path renders only the
/// 5 sections and suppresses per-step stdout.
pub fn run_embedded(scenario: &Scenario, path: &Path) -> eyre::Result<EmbeddedReport> {
    if is_v2_eligible(scenario) {
        return run_embedded_v2(scenario, path);
    }
    run_embedded_v1(scenario, path)
}

fn run_embedded_v2(scenario: &Scenario, path: &Path) -> eyre::Result<EmbeddedReport> {
    println!(
        "─── scenario: {} ────────────────────────────────────",
        scenario.name
    );
    println!();

    let mut results: Vec<StepResult> = Vec::with_capacity(scenario.steps.len());
    let mut failed = 0usize;

    for step in &scenario.steps {
        let target = try_parse_in_process(&step.command)
            .expect("v2-eligible scenario must have all-in-process steps");
        match target {
            InProcessTarget::LendingDemo { eth_crash_price } => {
                match crate::run_lending_demo_structured(eth_crash_price) {
                    Ok(r) => results.push(StepResult::LendingDemo(r)),
                    Err(e) => {
                        eprintln!("step '{}' failed: {e}", step.explanation);
                        failed += 1;
                    }
                }
            }
        }
    }

    let report = EmbeddedReport {
        total_steps: scenario.steps.len(),
        skipped: 0,
        passed: results.len(),
        failed,
        expectations_unverified: 0,
    };

    render_v2_sections(scenario, path, &results, &report);

    Ok(report)
}

fn render_v2_sections(
    scenario: &Scenario,
    path: &Path,
    results: &[StepResult],
    report: &EmbeddedReport,
) {
    // HEADLINE — for now echo scenario.headline (Phase 3 will verify it).
    println!("HEADLINE: {}", scenario.headline);
    println!();

    // TIMELINE — derive from each captured result. For LendingDemo,
    // each result contributes the 4-step demo sequence (deposit,
    // borrow, perp open, market shock).
    println!("TIMELINE:");
    for (i, r) in results.iter().enumerate() {
        match r {
            StepResult::LendingDemo(d) => {
                if results.len() > 1 {
                    println!("  [scenario step {}]", i + 1);
                }
                println!(
                    "    deposit  {} USDC as lending collateral",
                    d.initial_collateral_usdc
                );
                println!(
                    "    borrow   {} ETH at ETH={} USDC (debt = {} USDC)",
                    d.borrowed_eth_units,
                    d.perp_entry_mark,
                    d.borrowed_eth_units * u128::from(d.perp_entry_mark)
                );
                println!(
                    "    perp     long {} contracts @ entry {}, posts {} USDC margin",
                    d.perp_position_size, d.perp_entry_mark, d.perp_margin_usdc
                );
                println!(
                    "    shock    ETH price drops {} → {}",
                    d.perp_entry_mark, d.eth_crash_price
                );
            }
        }
    }
    println!();

    // DELTA — before vs after, for the entities the scenario cares about.
    println!("DELTA:");
    println!("  view                       free equity   verdict");
    println!("  -------------------------  -----------   ------------");
    for r in results {
        match r {
            StepResult::LendingDemo(d) => {
                println!(
                    "  siloed (perp only)         {:>11}   {}",
                    d.siloed_free_equity,
                    d.siloed_verdict()
                );
                println!(
                    "  unified (perp + lending)   {:>11}   {}",
                    d.unified_free_equity,
                    d.unified_verdict()
                );
            }
        }
    }
    println!();

    // OUTCOMES — v2 Phase 3 will parse expected_outcomes from JSON and
    // verify them. Until then, emit a placeholder so the contract
    // section is always present.
    println!("OUTCOMES:");
    println!("  (no expected_outcomes declared — Phase 3 will add JSON-driven verification)");
    println!();

    // Verdict (small, between OUTCOMES and NEXT) so failed steps are visible.
    if report.failed > 0 {
        println!(
            "({} of {} step(s) failed during execution)",
            report.failed, report.total_steps
        );
        println!();
    }
    println!("source: {}", path.display());

    print!("{}", cta_footer());
}

fn run_embedded_v1(scenario: &Scenario, path: &Path) -> eyre::Result<EmbeddedReport> {
    // Print scenario header up-front so the operator sees what they're
    // about to watch run.
    println!(
        "─── scenario: {} ────────────────────────────────────",
        scenario.name
    );
    println!("HEADLINE (curator claim): {}", scenario.headline);
    println!();
    println!("DESCRIPTION:");
    for line in scenario.description.lines() {
        println!("  {line}");
    }
    println!();

    let current_exe = std::env::current_exe()
        .map_err(|e| eyre::eyre!("current_exe() failed: {e}"))?;

    let mut report = EmbeddedReport {
        total_steps: scenario.steps.len(),
        skipped: 0,
        passed: 0,
        failed: 0,
        expectations_unverified: 0,
    };

    for (i, step) in scenario.steps.iter().enumerate() {
        println!("─── Step {} of {} ───────────────────────────", i + 1, scenario.steps.len());
        println!("  {}", step.explanation);
        println!("  $ {}", step.command);
        if let Some(expect) = &step.expect {
            println!("  # (looking for: {expect})");
        }
        println!();

        let trimmed = step.command.trim();
        // Skip comment / off-CLI hint lines.
        if trimmed.starts_with('#') || trimmed.is_empty() {
            println!("  (informational step — no command executed)");
            println!();
            report.skipped += 1;
            continue;
        }

        let has_shell_metas = has_shell_metacharacters(trimmed);

        let mut cmd = if has_shell_metas {
            let mut c = Command::new("sh");
            c.args(["-c", trimmed]);
            if let Some(exe_dir) = current_exe.parent() {
                let existing = std::env::var("PATH").unwrap_or_default();
                let new_path = format!("{}:{existing}", exe_dir.display());
                c.env("PATH", new_path);
            }
            c
        } else {
            let argv: Vec<&str> = trimmed.split_whitespace().collect();
            let (program, args) = match argv.split_first() {
                Some((p, a)) => (*p, a),
                None => continue,
            };
            let (cmd_name, cmd_args): (String, Vec<&str>) = if program == "princeps" {
                (current_exe.to_string_lossy().into_owned(), args.to_vec())
            } else {
                (program.to_string(), args.to_vec())
            };
            let mut c = Command::new(&cmd_name);
            c.args(&cmd_args);
            c
        };

        let status = cmd.status();

        match status {
            Ok(s) if s.success() => {
                println!();
                println!("  ✓ step {} succeeded (exit 0)", i + 1);
                report.passed += 1;
                // Expectation check — v1 cannot verify substrings
                // because we inherit stdio rather than capture it.
                // Track for the verdict; v2 will capture+tee.
                if step.expect.is_some() {
                    report.expectations_unverified += 1;
                }
            }
            Ok(s) => {
                println!();
                println!("  ✗ step {} exited {}", i + 1, s.code().unwrap_or(-1));
                report.failed += 1;
            }
            Err(e) => {
                println!();
                println!("  ✗ step {} failed to spawn: {e}", i + 1);
                if has_shell_metas {
                    println!("    (routed via `sh -c` because the command contains shell metacharacters; check that `sh` is available)");
                } else {
                    println!("    (princeps prefixes auto-route to current_exe; other CLIs must be in PATH)");
                }
                report.failed += 1;
            }
        }
        println!();
    }

    println!("─── verdict ───────────────────────────────────────────");
    println!(
        "{} step(s): {} passed / {} failed / {} skipped (informational)",
        report.total_steps, report.passed, report.failed, report.skipped
    );
    if report.expectations_unverified > 0 {
        println!(
            "{} step(s) declared expected-output substrings; v1 cannot verify these because it inherits stdio (v2 will tee).",
            report.expectations_unverified
        );
    }
    println!("source: {}", path.display());
    print!("{}", cta_footer());

    Ok(report)
}

/// Detect whether `command` contains shell metacharacters that mean it
/// can't be naïvely whitespace-split into argv. Returns true for
/// command chains (`&&`, `||`, `;`) and pipes (`|`).
///
/// **Intentionally excluded**: `<` and `>`. Curated scenarios use
/// `<PLACEHOLDER>` syntax for operator-substituted values, and
/// treating those as shell redirects breaks every placeholder step.
/// Real shell redirects are not used in any shipped scenario.
pub fn has_shell_metacharacters(command: &str) -> bool {
    command.contains("&&")
        || command.contains("||")
        || command.contains(';')
        || command.contains('|')
}

/// Summary of an embedded scenario run. Returned so callers (e.g.,
/// CI smoke tests) can fail when steps failed without re-parsing the
/// printed verdict.
#[derive(Debug, Clone, Copy)]
pub struct EmbeddedReport {
    pub total_steps: usize,
    pub skipped: usize,
    pub passed: usize,
    pub failed: usize,
    pub expectations_unverified: usize,
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

    /// Regression tests for shell-metachar detection. The runner
    /// branches on this; both branches have failed in distinct ways
    /// in this session's history, so the detection is locked in here.

    #[test]
    fn metachar_routes_chains_through_sh() {
        assert!(has_shell_metacharacters("princeps lending init && princeps lending deposit 1 100"));
        assert!(has_shell_metacharacters("a || b"));
        assert!(has_shell_metacharacters("a; b"));
        assert!(has_shell_metacharacters("a | grep b"));
    }

    /// Regression: scenarios use `<PLACEHOLDER>` for operator-
    /// substituted values. Treating `<` / `>` as metachars would
    /// route every placeholder-bearing step through `sh -c`, where
    /// the placeholder is parsed as a stdin redirect.
    #[test]
    fn metachar_does_not_match_angle_bracket_placeholders() {
        assert!(!has_shell_metacharacters(
            "princeps lending deposit <ACCOUNT> 1000"
        ));
        assert!(!has_shell_metacharacters("cmd <input> output"));
    }

    #[test]
    fn metachar_does_not_match_plain_commands() {
        assert!(!has_shell_metacharacters("princeps lending-demo --eth-crash-price 90"));
        assert!(!has_shell_metacharacters("princeps lending init"));
    }

    /// v2 in-process dispatch tests.

    #[test]
    fn try_parse_in_process_matches_lending_demo() {
        let t = try_parse_in_process("princeps lending-demo --eth-crash-price 90")
            .expect("should match");
        match t {
            InProcessTarget::LendingDemo { eth_crash_price } => {
                assert_eq!(eth_crash_price, 90);
            }
        }
    }

    #[test]
    fn try_parse_in_process_handles_whitespace() {
        let t = try_parse_in_process("  princeps lending-demo --eth-crash-price 42  ")
            .expect("should match after trim");
        match t {
            InProcessTarget::LendingDemo { eth_crash_price } => assert_eq!(eth_crash_price, 42),
        }
    }

    #[test]
    fn try_parse_in_process_returns_none_for_other_commands() {
        assert!(try_parse_in_process("princeps lending init").is_none());
        assert!(try_parse_in_process("princeps info").is_none());
        assert!(try_parse_in_process("princeps lending-demo").is_none()); // missing arg
        assert!(try_parse_in_process("princeps lending-demo --eth-crash-price").is_none()); // missing value
        assert!(try_parse_in_process("# comment").is_none());
    }

    fn make_scenario(commands: &[&str]) -> Scenario {
        Scenario {
            name: "test".to_string(),
            category: "stress".to_string(),
            description: "test".to_string(),
            headline: "test".to_string(),
            steps: commands
                .iter()
                .map(|c| ScenarioStep {
                    explanation: "step".to_string(),
                    command: (*c).to_string(),
                    expect: None,
                })
                .collect(),
        }
    }

    #[test]
    fn is_v2_eligible_true_when_all_steps_in_process() {
        let s = make_scenario(&[
            "princeps lending-demo --eth-crash-price 90",
            "princeps lending-demo --eth-crash-price 50",
        ]);
        assert!(is_v2_eligible(&s));
    }

    #[test]
    fn is_v2_eligible_false_when_any_step_is_subprocess() {
        let s = make_scenario(&[
            "princeps lending-demo --eth-crash-price 90",
            "princeps lending init", // not in-process-able
        ]);
        assert!(!is_v2_eligible(&s));
    }

    #[test]
    fn is_v2_eligible_false_for_empty_scenario() {
        let s = make_scenario(&[]);
        assert!(!is_v2_eligible(&s));
    }
}

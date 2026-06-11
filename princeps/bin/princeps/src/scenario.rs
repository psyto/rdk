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
    /// v2 Phase 3: optional declarative outcome checks. When present
    /// and the runner is v2-eligible, each check is evaluated against
    /// the captured execution result and rendered as `✓` / `✗` in the
    /// OUTCOMES section. When all checks pass, the HEADLINE drops the
    /// "(curator claim)" qualifier. When any fail or none are declared
    /// the qualifier remains.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expected_outcomes: Vec<ExpectedOutcome>,
}

/// One declarative outcome a scenario claims to demonstrate. `check`
/// is engine-specific; for princeps it is a [`LendingCheck`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpectedOutcome {
    /// Short identifier (kebab-case, no spaces). Echoed in OUTCOMES.
    pub name: String,
    /// One-line description in business language.
    pub description: String,
    pub check: LendingCheck,
}

/// Engine-specific check schema for princeps. JSON-serialized using
/// externally-tagged form so authors write `{"unified_verdict": "HEALTHY"}`
/// rather than `{"kind": "unified_verdict", "value": "HEALTHY"}`. Unit
/// variants serialize as bare strings, e.g. `"irm_base_rate_zero"`.
///
/// Checks dispatch on their own variant to the matching `StepResult`
/// type: lending-demo checks look at the last `LendingDemo` result;
/// IRM checks at the last `IrmCurve` result.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LendingCheck {
    // --- LendingDemo (cross-margin) checks ---
    /// Assert the siloed-perp verdict string. Accepts "LIQUIDATABLE" or "HEALTHY".
    SiloedVerdict(String),
    /// Assert the unified-portfolio verdict string.
    UnifiedVerdict(String),
    /// Assert the siloed free equity is at most this value.
    SiloedFreeMax(i128),
    /// Assert the siloed free equity is at least this value.
    SiloedFreeMin(i128),
    /// Assert the unified free equity is at most this value.
    UnifiedFreeMax(i128),
    /// Assert the unified free equity is at least this value.
    UnifiedFreeMin(i128),

    // --- IrmCurve checks ---
    /// Assert exact sample count on the IRM curve.
    IrmSampleCountExact(usize),
    /// Assert the IRM kink is at this exact utilization (bps).
    IrmKinkBpsExact(u16),
    /// Assert the base rate is exactly zero at zero utilization.
    IrmBaseRateZero,
    /// Assert the IRM curve never decreases as utilization rises.
    IrmCurveMonotonicNonDecreasing,
    /// Assert the slope-jump factor (rate@max / rate@kink) is at least
    /// this integer ratio. For the default params (slope_above = 10x
    /// slope_below) the actual factor is ~11; setting this to 5 gives
    /// a conservative floor.
    IrmSlopeJumpRatioMin(u32),

    // --- LendingStep (walkthrough) checks ---
    /// Assert the open-positions count at the end of the walkthrough.
    /// Evaluated against the LAST `LendingStep` result's
    /// `positions_after` snapshot.
    WalkPositionCount(usize),
    /// Assert a particular account's `collateral_amount` at the end of
    /// the walkthrough. Position must exist or check fails.
    WalkAccountCollateral { account: u64, amount: u128 },
    /// Assert a particular account has nonzero `scaled_debt` at the
    /// end of the walkthrough. Position must exist or check fails.
    WalkAccountHasDebt { account: u64 },
    /// Assert the verdict of the LAST `lending health` step.
    /// Accepts "HEALTHY" or "LIQUIDATABLE".
    WalkHealthVerdict(String),
    /// Assert the LAST `lending scan` step flagged exactly N accounts.
    WalkScanFlaggedExact(usize),
}

/// Per-outcome evaluation status used in the OUTCOMES section.
#[derive(Debug, Clone)]
pub enum OutcomeStatus {
    Pass,
    Fail(String),
}

/// Evaluate one LendingDemo-targeted check against a [`LendingDemoResult`].
pub fn evaluate_lending_check(
    check: &LendingCheck,
    result: &crate::LendingDemoResult,
) -> OutcomeStatus {
    match check {
        LendingCheck::SiloedVerdict(expected) => {
            let observed = result.siloed_verdict();
            if observed == expected.as_str() {
                OutcomeStatus::Pass
            } else {
                OutcomeStatus::Fail(format!("observed siloed verdict = {observed}"))
            }
        }
        LendingCheck::UnifiedVerdict(expected) => {
            let observed = result.unified_verdict();
            if observed == expected.as_str() {
                OutcomeStatus::Pass
            } else {
                OutcomeStatus::Fail(format!("observed unified verdict = {observed}"))
            }
        }
        LendingCheck::SiloedFreeMax(max) => {
            if result.siloed_free_equity <= *max {
                OutcomeStatus::Pass
            } else {
                OutcomeStatus::Fail(format!(
                    "observed siloed free = {} (expected ≤ {max})",
                    result.siloed_free_equity
                ))
            }
        }
        LendingCheck::SiloedFreeMin(min) => {
            if result.siloed_free_equity >= *min {
                OutcomeStatus::Pass
            } else {
                OutcomeStatus::Fail(format!(
                    "observed siloed free = {} (expected ≥ {min})",
                    result.siloed_free_equity
                ))
            }
        }
        LendingCheck::UnifiedFreeMax(max) => {
            if result.unified_free_equity <= *max {
                OutcomeStatus::Pass
            } else {
                OutcomeStatus::Fail(format!(
                    "observed unified free = {} (expected ≤ {max})",
                    result.unified_free_equity
                ))
            }
        }
        LendingCheck::UnifiedFreeMin(min) => {
            if result.unified_free_equity >= *min {
                OutcomeStatus::Pass
            } else {
                OutcomeStatus::Fail(format!(
                    "observed unified free = {} (expected ≥ {min})",
                    result.unified_free_equity
                ))
            }
        }
        // IRM checks are evaluated by `evaluate_irm_check` against an
        // IrmCurveDemoResult — they don't apply to LendingDemoResult.
        LendingCheck::IrmSampleCountExact(_)
        | LendingCheck::IrmKinkBpsExact(_)
        | LendingCheck::IrmBaseRateZero
        | LendingCheck::IrmCurveMonotonicNonDecreasing
        | LendingCheck::IrmSlopeJumpRatioMin(_) => OutcomeStatus::Fail(
            "this check targets the IRM curve, not the lending demo".to_string(),
        ),
        // Walkthrough checks are evaluated by `evaluate_walk_check`
        // against `LendingStepResult`s; they don't apply here.
        LendingCheck::WalkPositionCount(_)
        | LendingCheck::WalkAccountCollateral { .. }
        | LendingCheck::WalkAccountHasDebt { .. }
        | LendingCheck::WalkHealthVerdict(_)
        | LendingCheck::WalkScanFlaggedExact(_) => OutcomeStatus::Fail(
            "this check targets the lending walkthrough, not the lending demo".to_string(),
        ),
    }
}

/// Evaluate one IrmCurve-targeted check against an [`IrmCurveDemoResult`].
pub fn evaluate_irm_check(
    check: &LendingCheck,
    result: &crate::irm_curve_demo::IrmCurveDemoResult,
) -> OutcomeStatus {
    match check {
        LendingCheck::IrmSampleCountExact(expected) => {
            if result.sample_count() == *expected {
                OutcomeStatus::Pass
            } else {
                OutcomeStatus::Fail(format!(
                    "observed sample count = {}",
                    result.sample_count()
                ))
            }
        }
        LendingCheck::IrmKinkBpsExact(expected) => {
            if result.kink_bps == *expected {
                OutcomeStatus::Pass
            } else {
                OutcomeStatus::Fail(format!("observed kink_bps = {}", result.kink_bps))
            }
        }
        LendingCheck::IrmBaseRateZero => {
            if result.base_rate_per_block == 0 {
                OutcomeStatus::Pass
            } else {
                OutcomeStatus::Fail(format!(
                    "observed base_rate_per_block = {}",
                    result.base_rate_per_block
                ))
            }
        }
        LendingCheck::IrmCurveMonotonicNonDecreasing => {
            if result.is_monotonic_non_decreasing() {
                OutcomeStatus::Pass
            } else {
                OutcomeStatus::Fail("curve has a decreasing segment".to_string())
            }
        }
        LendingCheck::IrmSlopeJumpRatioMin(min) => {
            let jump = result.slope_jump_ratio().unwrap_or(0.0);
            if jump >= f64::from(*min) {
                OutcomeStatus::Pass
            } else {
                OutcomeStatus::Fail(format!("observed slope-jump factor = {jump:.2}x"))
            }
        }
        // LendingDemo checks don't apply here.
        LendingCheck::SiloedVerdict(_)
        | LendingCheck::UnifiedVerdict(_)
        | LendingCheck::SiloedFreeMax(_)
        | LendingCheck::SiloedFreeMin(_)
        | LendingCheck::UnifiedFreeMax(_)
        | LendingCheck::UnifiedFreeMin(_) => OutcomeStatus::Fail(
            "this check targets the lending demo, not the IRM curve".to_string(),
        ),
        // Walkthrough checks don't apply here.
        LendingCheck::WalkPositionCount(_)
        | LendingCheck::WalkAccountCollateral { .. }
        | LendingCheck::WalkAccountHasDebt { .. }
        | LendingCheck::WalkHealthVerdict(_)
        | LendingCheck::WalkScanFlaggedExact(_) => OutcomeStatus::Fail(
            "this check targets the lending walkthrough, not the IRM curve".to_string(),
        ),
    }
}

/// Evaluate one walkthrough-targeted check against a slice of
/// [`LendingStepResult`]s captured across the run. Each variant
/// resolves to the relevant "last" result:
/// * Position-related checks → last step's `positions_after`.
/// * `WalkHealthVerdict` → last step with a `health` snapshot.
/// * `WalkScanFlaggedExact` → last step with a `scan` snapshot.
fn evaluate_walk_check(check: &LendingCheck, results: &[LendingStepResult]) -> OutcomeStatus {
    let last = match results.last() {
        Some(r) => r,
        None => return OutcomeStatus::Fail("no lending walkthrough steps ran".to_string()),
    };
    match check {
        LendingCheck::WalkPositionCount(expected) => {
            let observed = last.positions_after.len();
            if observed == *expected {
                OutcomeStatus::Pass
            } else {
                OutcomeStatus::Fail(format!("observed position count = {observed}"))
            }
        }
        LendingCheck::WalkAccountCollateral { account, amount } => {
            match last
                .positions_after
                .iter()
                .find(|((acc, _), _)| *acc == *account)
            {
                Some((_, pos)) if pos.collateral_amount == *amount => OutcomeStatus::Pass,
                Some((_, pos)) => OutcomeStatus::Fail(format!(
                    "account {account} collateral = {} (expected {amount})",
                    pos.collateral_amount
                )),
                None => OutcomeStatus::Fail(format!("account {account} has no open position")),
            }
        }
        LendingCheck::WalkAccountHasDebt { account } => {
            match last
                .positions_after
                .iter()
                .find(|((acc, _), _)| *acc == *account)
            {
                Some((_, pos)) if pos.scaled_debt > 0 => OutcomeStatus::Pass,
                Some(_) => OutcomeStatus::Fail(format!(
                    "account {account} has zero scaled_debt"
                )),
                None => OutcomeStatus::Fail(format!("account {account} has no open position")),
            }
        }
        LendingCheck::WalkHealthVerdict(expected) => {
            match results.iter().rev().find_map(|r| r.health.as_ref()) {
                Some(h) => {
                    let observed = if h.healthy { "HEALTHY" } else { "LIQUIDATABLE" };
                    if observed == expected.as_str() {
                        OutcomeStatus::Pass
                    } else {
                        OutcomeStatus::Fail(format!("observed last-health verdict = {observed}"))
                    }
                }
                None => OutcomeStatus::Fail("no health step ran during the walkthrough".to_string()),
            }
        }
        LendingCheck::WalkScanFlaggedExact(expected) => {
            match results.iter().rev().find_map(|r| r.scan.as_ref()) {
                Some(s) => {
                    if s.flagged.len() == *expected {
                        OutcomeStatus::Pass
                    } else {
                        OutcomeStatus::Fail(format!(
                            "observed last-scan flagged = {} (expected {expected})",
                            s.flagged.len()
                        ))
                    }
                }
                None => OutcomeStatus::Fail("no scan step ran during the walkthrough".to_string()),
            }
        }
        // Non-walkthrough checks don't apply here.
        LendingCheck::SiloedVerdict(_)
        | LendingCheck::UnifiedVerdict(_)
        | LendingCheck::SiloedFreeMax(_)
        | LendingCheck::SiloedFreeMin(_)
        | LendingCheck::UnifiedFreeMax(_)
        | LendingCheck::UnifiedFreeMin(_) => OutcomeStatus::Fail(
            "this check targets the lending demo, not the walkthrough".to_string(),
        ),
        LendingCheck::IrmSampleCountExact(_)
        | LendingCheck::IrmKinkBpsExact(_)
        | LendingCheck::IrmBaseRateZero
        | LendingCheck::IrmCurveMonotonicNonDecreasing
        | LendingCheck::IrmSlopeJumpRatioMin(_) => OutcomeStatus::Fail(
            "this check targets the IRM curve, not the walkthrough".to_string(),
        ),
    }
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
#[derive(Debug, Clone)]
pub enum InProcessTarget {
    /// `princeps lending-demo --eth-crash-price <N>`
    LendingDemo { eth_crash_price: u128 },
    /// `princeps irm-curve-demo`
    IrmCurveDemo,
    /// `princeps lending <subcommand> ...` — per-step variant for the
    /// hands-on lending sandbox. Steps share a single in-memory bridge
    /// across the scenario run; `--state-file` flags are parsed but
    /// ignored (the v2 path never persists to disk).
    LendingStep(LendingStep),
}

/// One parsed `princeps lending` sub-step. Mirrors `LendingCommand` in
/// `main.rs` minus the variants the walkthrough doesn't exercise
/// (`repay`, `withdraw`, `supply`, `withdraw-supply`); add them when a
/// scenario needs them.
#[derive(Debug, Clone)]
pub enum LendingStep {
    /// `princeps lending init` — reset bridge to a fresh default market.
    Init,
    /// `princeps lending deposit <account> <amount>`.
    Deposit { account: u64, amount: u128 },
    /// `princeps lending borrow <account> <amount> --eth-price <P>`.
    Borrow { account: u64, amount: u128, eth_price: u64 },
    /// `princeps lending health <account> --eth-price <P>` (read-only).
    Health { account: u64, eth_price: u64 },
    /// `princeps lending scan --eth-price <P>` (read-only).
    Scan { eth_price: u64 },
    /// `princeps lending list` (read-only).
    List,
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
    if trimmed == "princeps irm-curve-demo" {
        return Some(InProcessTarget::IrmCurveDemo);
    }
    try_parse_lending_step(trimmed).map(InProcessTarget::LendingStep)
}

/// Parse a `princeps lending <subcommand> ...` command into a
/// [`LendingStep`]. Returns `None` if the command isn't a recognized
/// lending subcommand, or if required arguments are missing / malformed.
///
/// Recognized flags:
///   * `--state-file <path>` — parsed and discarded (in-process never
///     touches the file system).
///   * `--eth-price <N>` — borrow / health / scan price override.
///
/// Unknown flags or extra positional arguments cause this to return
/// `None` so the step falls back to v1 sub-process spawn rather than
/// silently dropping a flag the user supplied.
fn try_parse_lending_step(command: &str) -> Option<LendingStep> {
    let mut tokens = command.split_whitespace();
    if tokens.next()? != "princeps" {
        return None;
    }
    if tokens.next()? != "lending" {
        return None;
    }
    let sub = tokens.next()?;
    // Collect remaining tokens; we will pop positional args off the front
    // and pop named flags wherever they sit.
    let mut rest: Vec<&str> = tokens.collect();

    let eth_price = pop_named_u64(&mut rest, "--eth-price")?;
    // Discard --state-file (in-process never persists).
    let _ = pop_named_string(&mut rest, "--state-file");

    let step = match sub {
        "init" => {
            if !rest.is_empty() {
                return None;
            }
            LendingStep::Init
        }
        "deposit" => {
            let account = rest.first()?.parse::<u64>().ok()?;
            let amount = rest.get(1)?.parse::<u128>().ok()?;
            if rest.len() != 2 {
                return None;
            }
            LendingStep::Deposit { account, amount }
        }
        "borrow" => {
            let account = rest.first()?.parse::<u64>().ok()?;
            let amount = rest.get(1)?.parse::<u128>().ok()?;
            if rest.len() != 2 {
                return None;
            }
            LendingStep::Borrow {
                account,
                amount,
                eth_price: eth_price.unwrap_or(1),
            }
        }
        "health" => {
            let account = rest.first()?.parse::<u64>().ok()?;
            if rest.len() != 1 {
                return None;
            }
            LendingStep::Health {
                account,
                eth_price: eth_price.unwrap_or(1),
            }
        }
        "scan" => {
            if !rest.is_empty() {
                return None;
            }
            LendingStep::Scan {
                eth_price: eth_price.unwrap_or(1),
            }
        }
        "list" => {
            if !rest.is_empty() {
                return None;
            }
            LendingStep::List
        }
        _ => return None,
    };
    Some(step)
}

/// Pop a `--name <value>` pair out of the token list, returning the
/// parsed value. Returns:
///   * `Some(Some(v))` — flag present, value parsed.
///   * `Some(None)`    — flag absent (caller decides the default).
///   * `None`          — flag present but value missing or malformed
///                       (signals parse failure to the caller).
fn pop_named_u64(tokens: &mut Vec<&str>, name: &str) -> Option<Option<u64>> {
    match tokens.iter().position(|t| *t == name) {
        None => Some(None),
        Some(i) => {
            if i + 1 >= tokens.len() {
                return None;
            }
            let v = tokens[i + 1].parse::<u64>().ok()?;
            tokens.drain(i..=i + 1);
            Some(Some(v))
        }
    }
}

fn pop_named_string<'a>(tokens: &mut Vec<&'a str>, name: &str) -> Option<&'a str> {
    let i = tokens.iter().position(|t| *t == name)?;
    if i + 1 >= tokens.len() {
        return None;
    }
    let v = tokens[i + 1];
    tokens.drain(i..=i + 1);
    Some(v)
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
    IrmCurve(crate::irm_curve_demo::IrmCurveDemoResult),
    LendingStep(LendingStepResult),
}

/// Per-step record for `LendingStep` execution. Carries enough
/// information for the TIMELINE / DELTA renderers and the walkthrough
/// outcome checks. Held in a `Vec<LendingStepResult>` across the run
/// so checks can target "last health" or "last scan" cleanly.
#[derive(Debug, Clone)]
pub(crate) struct LendingStepResult {
    /// The step that produced this result (echoed for TIMELINE).
    pub step: LendingStep,
    /// All open positions after the step ran. Re-snapshotted every
    /// step so per-step DELTAs are honest even for read-only steps.
    pub positions_after: Vec<((u64, u32), princeps_lending::Position)>,
    /// Set for `Health` steps only.
    pub health: Option<LendingHealthSnapshot>,
    /// Set for `Scan` steps only.
    pub scan: Option<LendingScanSnapshot>,
}

#[derive(Debug, Clone)]
pub(crate) struct LendingHealthSnapshot {
    pub account: u64,
    pub eth_price: u64,
    pub adjusted_collateral_value: i128,
    pub debt_value: i128,
    pub free_equity: i128,
    pub healthy: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct LendingScanSnapshot {
    pub eth_price: u64,
    pub scanned: usize,
    pub flagged: Vec<(u64, i128)>,
}

/// Apply a single [`LendingStep`] against `bridge`. `Init` (re)builds
/// the bridge in place by replacing markets + clearing positions; all
/// other steps require the bridge to already have a market and return
/// an error if the underlying call fails. Always succeeds in producing
/// a `LendingStepResult` whose `positions_after` snapshot reflects the
/// post-step state (read-only steps still re-snapshot for honest DELTA).
fn run_lending_step(
    step: &LendingStep,
    bridge: &mut Option<princeps_evm::LiveRethEvmBridge<()>>,
) -> eyre::Result<LendingStepResult> {
    use princeps_evm::LiveRethEvmBridge;
    use princeps_lending::MarketId;
    use rdk_clob::AccountId;
    use rdk_funding::MarkPrice;
    use std::collections::BTreeMap;

    // `Init` resets the bridge whether or not one already existed.
    if matches!(step, LendingStep::Init) {
        let fresh = LiveRethEvmBridge::new((), crate::dev_chain_spec());
        let market = crate::make_default_market();
        fresh.with_markets_mut(|m| {
            m.insert(market.id, market.clone());
        });
        *bridge = Some(fresh);
        let positions_after = snapshot_positions(bridge.as_ref().unwrap());
        return Ok(LendingStepResult {
            step: step.clone(),
            positions_after,
            health: None,
            scan: None,
        });
    }

    // All non-init steps need a bridge.
    let bridge = bridge.as_ref().ok_or_else(|| {
        eyre::eyre!(
            "lending step ran before `lending init` in the same scenario — \
             the in-process walkthrough does not load state from disk."
        )
    })?;

    let (health, scan) = match step {
        LendingStep::Init => unreachable!(),
        LendingStep::Deposit { account, amount } => {
            bridge
                .lending_deposit_collateral(AccountId(*account), MarketId(0), *amount)
                .map_err(|e| eyre::eyre!("deposit failed: {e:?}"))?;
            (None, None)
        }
        LendingStep::Borrow { account, amount, eth_price } => {
            let mut prices = BTreeMap::new();
            prices.insert(MarketId(0), (1u128, u128::from(*eth_price)));
            bridge
                .lending_borrow_unified(
                    AccountId(*account),
                    MarketId(0),
                    *amount,
                    MarkPrice(0),
                    0,
                    &prices,
                )
                .map_err(|e| eyre::eyre!("borrow failed: {e:?}"))?;
            (None, None)
        }
        LendingStep::Health { account, eth_price } => {
            let mut prices = BTreeMap::new();
            prices.insert(MarketId(0), (1u128, u128::from(*eth_price)));
            let mark = MarkPrice(0);
            let inputs = bridge.compute_account_portfolio_inputs(
                AccountId(*account),
                mark,
                0,
                &prices,
            );
            let free = bridge.account_free_equity(AccountId(*account), mark, 0, &prices);
            let healthy = bridge.account_is_healthy_portfolio(AccountId(*account), mark, 0, &prices);
            (
                Some(LendingHealthSnapshot {
                    account: *account,
                    eth_price: *eth_price,
                    adjusted_collateral_value: inputs.lending_adjusted_collateral_value,
                    debt_value: inputs.lending_debt_value,
                    free_equity: free,
                    healthy,
                }),
                None,
            )
        }
        LendingStep::Scan { eth_price } => {
            let mut prices = BTreeMap::new();
            prices.insert(MarketId(0), (1u128, u128::from(*eth_price)));
            let report = bridge.scan_unified(MarkPrice(0), 0, &prices);
            (
                None,
                Some(LendingScanSnapshot {
                    eth_price: *eth_price,
                    scanned: report.scanned,
                    flagged: report
                        .flagged
                        .into_iter()
                        .map(|(acc, free)| (acc.0, free))
                        .collect(),
                }),
            )
        }
        LendingStep::List => (None, None),
    };

    Ok(LendingStepResult {
        step: step.clone(),
        positions_after: snapshot_positions(bridge),
        health,
        scan,
    })
}

fn snapshot_positions(
    bridge: &princeps_evm::LiveRethEvmBridge<()>,
) -> Vec<((u64, u32), princeps_lending::Position)> {
    bridge
        .positions_snapshot()
        .into_iter()
        .map(|((acc, mid), pos)| ((acc.0, mid.0), pos))
        .collect()
}

/// Per-run dial overrides supplied by the CLI. Each field is optional;
/// `None` means "use the value baked into the scenario JSON". Dials
/// only affect v2-eligible scenarios — v1 sub-process scenarios pass
/// their commands through unchanged and so cannot be retargeted from
/// here.
#[derive(Debug, Clone, Copy, Default)]
pub struct DialOverrides {
    /// Override the `--eth-crash-price` argument on lending-demo
    /// in-process targets.
    pub eth_crash_price: Option<u128>,
    /// Override the lending-market LT bps (default 9500 = 95% LT).
    pub ltv_bps: Option<u16>,
    /// Override the lending-market liquidation penalty bps (default
    /// 500 = 5% bonus).
    pub liquidation_penalty_bps: Option<u16>,
    /// Shock the lending-side ETH oracle ±bps relative to perp's view
    /// of the crash price. Default 0 = no divergence.
    pub oracle_shock_bps: Option<i16>,
}

/// Embedded execution. For v2-eligible scenarios (every step matches
/// an [`InProcessTarget`]), dispatches in-process with optional
/// [`DialOverrides`] applied, captures structured results, and emits
/// the 5-section v2 output contract. For other scenarios falls back
/// to v1: spawn each step's command as a sub-process with stdio
/// inherited.
///
/// Returns the per-step status set. The v1 path streams step output
/// to the operator's terminal directly; the v2 path renders only the
/// 5 sections and suppresses per-step stdout.
pub fn run_embedded(
    scenario: &Scenario,
    path: &Path,
    dials: &DialOverrides,
) -> eyre::Result<EmbeddedReport> {
    if is_v2_eligible(scenario) {
        return run_embedded_v2(scenario, path, dials);
    }
    run_embedded_v1(scenario, path)
}

fn run_embedded_v2(
    scenario: &Scenario,
    path: &Path,
    dials: &DialOverrides,
) -> eyre::Result<EmbeddedReport> {
    println!(
        "─── scenario: {} ────────────────────────────────────",
        scenario.name
    );
    let any_dial = dials.eth_crash_price.is_some()
        || dials.ltv_bps.is_some()
        || dials.liquidation_penalty_bps.is_some()
        || dials.oracle_shock_bps.is_some();
    if any_dial {
        println!();
        println!("DIAL OVERRIDES (from CLI flags):");
        if let Some(p) = dials.eth_crash_price {
            println!("    eth_crash_price     : {p}");
        }
        if let Some(l) = dials.ltv_bps {
            println!("    ltv_bps             : {l}");
        }
        if let Some(l) = dials.liquidation_penalty_bps {
            println!("    liquidation_penalty : {l} bps");
        }
        if let Some(s) = dials.oracle_shock_bps {
            println!("    oracle_shock_bps    : {s:+}");
        }
    }
    println!();

    let mut results: Vec<StepResult> = Vec::with_capacity(scenario.steps.len());
    let mut failed = 0usize;
    // Shared bridge for `LendingStep` walkthroughs. `Init` (re)builds
    // it; other steps require it to already exist. Stays `None` for
    // scenarios that don't use `LendingStep` at all.
    let mut lending_bridge: Option<princeps_evm::LiveRethEvmBridge<()>> = None;

    for step in &scenario.steps {
        let target = try_parse_in_process(&step.command)
            .expect("v2-eligible scenario must have all-in-process steps");
        match target {
            InProcessTarget::LendingDemo { eth_crash_price } => {
                // CLI dials override the JSON-baked values; unset dials
                // fall back to LendingDemoConfig::default().
                let defaults = crate::LendingDemoConfig::default();
                let cfg = crate::LendingDemoConfig {
                    eth_crash_price: dials.eth_crash_price.unwrap_or(eth_crash_price),
                    ltv_bps: dials.ltv_bps.unwrap_or(defaults.ltv_bps),
                    liquidation_penalty_bps: dials
                        .liquidation_penalty_bps
                        .unwrap_or(defaults.liquidation_penalty_bps),
                    oracle_shock_bps: dials
                        .oracle_shock_bps
                        .unwrap_or(defaults.oracle_shock_bps),
                };
                match crate::run_lending_demo_structured(cfg) {
                    Ok(r) => results.push(StepResult::LendingDemo(r)),
                    Err(e) => {
                        eprintln!("step '{}' failed: {e}", step.explanation);
                        failed += 1;
                    }
                }
            }
            InProcessTarget::IrmCurveDemo => {
                let r = crate::irm_curve_demo::run_irm_curve_demo_structured();
                results.push(StepResult::IrmCurve(r));
            }
            InProcessTarget::LendingStep(lstep) => {
                match run_lending_step(&lstep, &mut lending_bridge) {
                    Ok(r) => results.push(StepResult::LendingStep(r)),
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
    // Evaluate expected_outcomes up-front so HEADLINE can carry a
    // verification badge.
    let evaluated_outcomes = evaluate_all_outcomes(scenario, results);
    let any_failed = evaluated_outcomes
        .iter()
        .any(|(_, status)| matches!(status, OutcomeStatus::Fail(_)));
    let has_outcomes = !evaluated_outcomes.is_empty();

    // HEADLINE — verified ✓ when all outcomes pass; ⚠ when any fail;
    // plain (no badge) when no outcomes declared.
    if has_outcomes && !any_failed {
        println!("HEADLINE ✓: {}", scenario.headline);
    } else if has_outcomes && any_failed {
        println!("HEADLINE ⚠: {}", scenario.headline);
    } else {
        println!("HEADLINE (unverified): {}", scenario.headline);
    }
    println!();

    // TIMELINE — derive from each captured result.
    println!("TIMELINE:");
    for (i, r) in results.iter().enumerate() {
        if results.len() > 1 {
            println!("  [scenario step {}]", i + 1);
        }
        match r {
            StepResult::LendingDemo(d) => {
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
            StepResult::IrmCurve(d) => {
                println!(
                    "    sample     {} utilization points (0%, 10%, …, 100%)",
                    d.sample_count()
                );
                println!(
                    "    evaluate   compute_borrow_rate at each point with default IrmParams (kink {}%)",
                    d.kink_bps / 100
                );
                if let Some(jump) = d.slope_jump_ratio() {
                    println!(
                        "    summarize  base rate {}; slope-jump factor at max utilization = {jump:.2}x",
                        d.base_rate_per_block
                    );
                }
            }
            StepResult::LendingStep(r) => match &r.step {
                LendingStep::Init => {
                    println!("    init       fresh USDC/ETH market, no positions");
                }
                LendingStep::Deposit { account, amount } => {
                    println!("    deposit    account {account} → {amount} USDC collateral");
                }
                LendingStep::Borrow { account, amount, eth_price } => {
                    println!(
                        "    borrow     account {account} → {amount} ETH (oracle ETH={eth_price})"
                    );
                }
                LendingStep::Health { account: _, eth_price: _ } => {
                    let snap = r.health.as_ref().expect("health step must record health");
                    let verdict = if snap.healthy { "HEALTHY" } else { "LIQUIDATABLE" };
                    println!(
                        "    health     account {} at ETH={} → adj_coll={}, debt={}, free={}, verdict={verdict}",
                        snap.account,
                        snap.eth_price,
                        snap.adjusted_collateral_value,
                        snap.debt_value,
                        snap.free_equity,
                    );
                }
                LendingStep::Scan { eth_price: _ } => {
                    let snap = r.scan.as_ref().expect("scan step must record scan");
                    println!(
                        "    scan       at ETH={} → {} of {} flagged",
                        snap.eth_price,
                        snap.flagged.len(),
                        snap.scanned
                    );
                }
                LendingStep::List => {
                    println!(
                        "    list       {} open position(s)",
                        r.positions_after.len()
                    );
                }
            },
        }
    }
    println!();

    // DELTA — before vs after / spread.
    println!("DELTA:");
    for r in results {
        match r {
            StepResult::LendingDemo(d) => {
                println!("  view                       free equity   verdict");
                println!("  -------------------------  -----------   ------------");
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
            StepResult::IrmCurve(d) => {
                println!("  IRM curve (utilization → rate, as fraction of RAY):");
                println!("    {:<13}  {:<15}  Note", "Utilization", "Rate (×RAY)");
                println!("    {:-<13}  {:-<15}  {}", "", "", "----");
                for p in &d.points {
                    let note = if p.utilization_bps == d.kink_bps {
                        "kink"
                    } else if p.utilization_bps == 10_000 {
                        "max"
                    } else {
                        ""
                    };
                    println!(
                        "    {:>10}%   {:<15.8}  {}",
                        p.utilization_bps / 100,
                        p.rate_as_ray_fraction,
                        note
                    );
                }
            }
            StepResult::LendingStep(_) => {} // rendered once below, end-of-walkthrough state
        }
    }
    // For walkthroughs, render a single final positions table after all
    // per-step DELTAs. Cleaner than re-printing the (often-unchanged)
    // table after every read-only step.
    if let Some(last) = results.iter().rev().find_map(|r| match r {
        StepResult::LendingStep(s) => Some(s),
        _ => None,
    }) {
        println!("  Final positions after walkthrough:");
        if last.positions_after.is_empty() {
            println!("    (no open positions)");
        } else {
            println!(
                "    {:>8}  {:>6}  {:>12}  {:>12}  {:>12}",
                "Account", "Market", "Collateral", "ScaledDebt", "ScaledSupply"
            );
            println!(
                "    {:>8}  {:>6}  {:>12}  {:>12}  {:>12}",
                "-------", "------", "----------", "----------", "------------"
            );
            for ((acc, mid), pos) in &last.positions_after {
                println!(
                    "    {:>8}  {:>6}  {:>12}  {:>12}  {:>12}",
                    acc, mid, pos.collateral_amount, pos.scaled_debt, pos.scaled_supply
                );
            }
        }
    }
    println!();

    // OUTCOMES — render each evaluated outcome.
    println!("OUTCOMES:");
    if evaluated_outcomes.is_empty() {
        println!("  (no expected_outcomes declared — HEADLINE shown as unverified)");
    } else {
        for (outcome, status) in &evaluated_outcomes {
            match status {
                OutcomeStatus::Pass => println!("  ✓ {}", outcome.description),
                OutcomeStatus::Fail(why) => {
                    println!("  ✗ {} ({why})", outcome.description);
                }
            }
        }
        let passed = evaluated_outcomes
            .iter()
            .filter(|(_, s)| matches!(s, OutcomeStatus::Pass))
            .count();
        let total = evaluated_outcomes.len();
        println!();
        println!("  {passed} of {total} outcome(s) verified.");
    }
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

fn evaluate_all_outcomes<'a>(
    scenario: &'a Scenario,
    results: &[StepResult],
) -> Vec<(&'a ExpectedOutcome, OutcomeStatus)> {
    // Each check variant has an implied target result type:
    //   * LendingDemo checks (SiloedVerdict, UnifiedFreeMin, …) target
    //     the last LendingDemo result.
    //   * IRM checks (IrmSampleCountExact, IrmKinkBpsExact, …) target
    //     the last IrmCurve result.
    // The convention is "outcomes describe end state": when multiple
    // steps of the same target type exist, we evaluate against the LAST.
    let last_lending = results.iter().rev().find_map(|r| match r {
        StepResult::LendingDemo(d) => Some(d),
        _ => None,
    });
    let last_irm = results.iter().rev().find_map(|r| match r {
        StepResult::IrmCurve(d) => Some(d),
        _ => None,
    });
    let walk_results: Vec<LendingStepResult> = results
        .iter()
        .filter_map(|r| match r {
            StepResult::LendingStep(s) => Some(s.clone()),
            _ => None,
        })
        .collect();

    scenario
        .expected_outcomes
        .iter()
        .map(|outcome| {
            let status = match &outcome.check {
                LendingCheck::SiloedVerdict(_)
                | LendingCheck::UnifiedVerdict(_)
                | LendingCheck::SiloedFreeMax(_)
                | LendingCheck::SiloedFreeMin(_)
                | LendingCheck::UnifiedFreeMax(_)
                | LendingCheck::UnifiedFreeMin(_) => match last_lending {
                    Some(d) => evaluate_lending_check(&outcome.check, d),
                    None => OutcomeStatus::Fail(
                        "no lending-demo result available".to_string(),
                    ),
                },
                LendingCheck::IrmSampleCountExact(_)
                | LendingCheck::IrmKinkBpsExact(_)
                | LendingCheck::IrmBaseRateZero
                | LendingCheck::IrmCurveMonotonicNonDecreasing
                | LendingCheck::IrmSlopeJumpRatioMin(_) => match last_irm {
                    Some(d) => evaluate_irm_check(&outcome.check, d),
                    None => OutcomeStatus::Fail(
                        "no IRM curve result available".to_string(),
                    ),
                },
                LendingCheck::WalkPositionCount(_)
                | LendingCheck::WalkAccountCollateral { .. }
                | LendingCheck::WalkAccountHasDebt { .. }
                | LendingCheck::WalkHealthVerdict(_)
                | LendingCheck::WalkScanFlaggedExact(_) => {
                    evaluate_walk_check(&outcome.check, &walk_results)
                }
            };
            (outcome, status)
        })
        .collect()
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
            _ => panic!("expected LendingDemo, got {t:?}"),
        }
    }

    #[test]
    fn try_parse_in_process_handles_whitespace() {
        let t = try_parse_in_process("  princeps lending-demo --eth-crash-price 42  ")
            .expect("should match after trim");
        match t {
            InProcessTarget::LendingDemo { eth_crash_price } => assert_eq!(eth_crash_price, 42),
            _ => panic!("expected LendingDemo, got {t:?}"),
        }
    }

    #[test]
    fn try_parse_in_process_matches_irm_curve_demo() {
        assert!(matches!(
            try_parse_in_process("princeps irm-curve-demo"),
            Some(InProcessTarget::IrmCurveDemo)
        ));
        assert!(matches!(
            try_parse_in_process("  princeps irm-curve-demo  "),
            Some(InProcessTarget::IrmCurveDemo)
        ));
    }

    #[test]
    fn evaluate_irm_check_passes() {
        let r = crate::irm_curve_demo::run_irm_curve_demo_structured();
        assert!(matches!(
            evaluate_irm_check(&LendingCheck::IrmSampleCountExact(11), &r),
            OutcomeStatus::Pass
        ));
        assert!(matches!(
            evaluate_irm_check(&LendingCheck::IrmKinkBpsExact(8000), &r),
            OutcomeStatus::Pass
        ));
        assert!(matches!(
            evaluate_irm_check(&LendingCheck::IrmBaseRateZero, &r),
            OutcomeStatus::Pass
        ));
        assert!(matches!(
            evaluate_irm_check(&LendingCheck::IrmCurveMonotonicNonDecreasing, &r),
            OutcomeStatus::Pass
        ));
        assert!(matches!(
            evaluate_irm_check(&LendingCheck::IrmSlopeJumpRatioMin(5), &r),
            OutcomeStatus::Pass
        ));
        // Failure case:
        assert!(matches!(
            evaluate_irm_check(&LendingCheck::IrmKinkBpsExact(9999), &r),
            OutcomeStatus::Fail(_)
        ));
    }

    #[test]
    fn lending_check_against_irm_returns_fail_with_helpful_message() {
        let r = crate::LendingDemoResult {
            eth_crash_price: 90,
            ltv_bps: 9_500,
            liquidation_penalty_bps: 500,
            oracle_shock_bps: 0,
            lending_oracle_price: 90,
            initial_collateral_usdc: 1000,
            borrowed_eth_units: 5,
            perp_position_size: 10,
            perp_entry_mark: 100,
            perp_margin_usdc: 50,
            siloed_free_equity: -140,
            unified_free_equity: 360,
        };
        // An IRM check applied via the lending-check function should
        // return Fail with the type-mismatch hint.
        match evaluate_lending_check(&LendingCheck::IrmSampleCountExact(11), &r) {
            OutcomeStatus::Fail(why) => assert!(why.contains("IRM curve")),
            _ => panic!("expected Fail"),
        }
    }

    #[test]
    fn try_parse_in_process_returns_none_for_other_commands() {
        assert!(try_parse_in_process("princeps info").is_none());
        assert!(try_parse_in_process("princeps lending-demo").is_none()); // missing arg
        assert!(try_parse_in_process("princeps lending-demo --eth-crash-price").is_none()); // missing value
        assert!(try_parse_in_process("# comment").is_none());
        // `lending repay` is out of the v2 walkthrough surface — still
        // routes through the sub-process fallback.
        assert!(try_parse_in_process("princeps lending repay 1 50").is_none());
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
            expected_outcomes: Vec::new(),
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
            // `lending repay` is outside the v2 walkthrough surface, so
            // this still falls back to sub-process spawn.
            "princeps lending repay 1 50",
        ]);
        assert!(!is_v2_eligible(&s));
    }

    #[test]
    fn is_v2_eligible_false_for_empty_scenario() {
        let s = make_scenario(&[]);
        assert!(!is_v2_eligible(&s));
    }

    /// Phase 3: expected_outcomes parsing + evaluation.

    fn fake_result(siloed: i128, unified: i128) -> crate::LendingDemoResult {
        crate::LendingDemoResult {
            eth_crash_price: 90,
            ltv_bps: 9_500,
            liquidation_penalty_bps: 500,
            oracle_shock_bps: 0,
            lending_oracle_price: 90,
            initial_collateral_usdc: 1000,
            borrowed_eth_units: 5,
            perp_position_size: 10,
            perp_entry_mark: 100,
            perp_margin_usdc: 50,
            siloed_free_equity: siloed,
            unified_free_equity: unified,
        }
    }

    #[test]
    fn evaluate_lending_check_verdict_pass() {
        let r = fake_result(-140, 360);
        let s = evaluate_lending_check(
            &LendingCheck::SiloedVerdict("LIQUIDATABLE".to_string()),
            &r,
        );
        assert!(matches!(s, OutcomeStatus::Pass));
        let s = evaluate_lending_check(
            &LendingCheck::UnifiedVerdict("HEALTHY".to_string()),
            &r,
        );
        assert!(matches!(s, OutcomeStatus::Pass));
    }

    #[test]
    fn evaluate_lending_check_verdict_fail() {
        let r = fake_result(-140, 360);
        let s = evaluate_lending_check(
            &LendingCheck::UnifiedVerdict("LIQUIDATABLE".to_string()),
            &r,
        );
        match s {
            OutcomeStatus::Fail(why) => assert!(why.contains("HEALTHY")),
            _ => panic!("expected Fail"),
        }
    }

    #[test]
    fn evaluate_lending_check_bounds() {
        let r = fake_result(-140, 360);
        assert!(matches!(
            evaluate_lending_check(&LendingCheck::SiloedFreeMax(-1), &r),
            OutcomeStatus::Pass
        ));
        assert!(matches!(
            evaluate_lending_check(&LendingCheck::SiloedFreeMax(-200), &r),
            OutcomeStatus::Fail(_)
        ));
        assert!(matches!(
            evaluate_lending_check(&LendingCheck::UnifiedFreeMin(0), &r),
            OutcomeStatus::Pass
        ));
        assert!(matches!(
            evaluate_lending_check(&LendingCheck::UnifiedFreeMin(500), &r),
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
            "steps": [
                {"explanation": "step", "command": "princeps lending-demo --eth-crash-price 90"}
            ],
            "expected_outcomes": [
                {
                    "name": "u-healthy",
                    "description": "unified stays HEALTHY",
                    "check": {"unified_verdict": "HEALTHY"}
                },
                {
                    "name": "siloed-neg",
                    "description": "siloed free is negative",
                    "check": {"siloed_free_max": -1}
                }
            ]
        }"#;
        let s: Scenario = serde_json::from_str(json).expect("parse");
        assert_eq!(s.expected_outcomes.len(), 2);
        assert!(matches!(
            s.expected_outcomes[0].check,
            LendingCheck::UnifiedVerdict(_)
        ));
        assert!(matches!(
            s.expected_outcomes[1].check,
            LendingCheck::SiloedFreeMax(-1)
        ));
    }

    /// LendingStep parser tests — every walkthrough command shape.

    #[test]
    fn parse_lending_init_with_state_file() {
        let t = try_parse_in_process(
            "princeps lending init --state-file ./.princeps-walkthrough-state.json",
        )
        .expect("init parses");
        assert!(matches!(
            t,
            InProcessTarget::LendingStep(LendingStep::Init)
        ));
    }

    #[test]
    fn parse_lending_deposit() {
        let t = try_parse_in_process(
            "princeps lending deposit 1 1000 --state-file ./x.json",
        )
        .expect("deposit parses");
        match t {
            InProcessTarget::LendingStep(LendingStep::Deposit { account, amount }) => {
                assert_eq!(account, 1);
                assert_eq!(amount, 1000);
            }
            other => panic!("expected Deposit, got {other:?}"),
        }
    }

    #[test]
    fn parse_lending_borrow_flag_order_independent() {
        let a = try_parse_in_process(
            "princeps lending borrow 1 200 --eth-price 1 --state-file ./x.json",
        )
        .expect("borrow parses (flag order A)");
        let b = try_parse_in_process(
            "princeps lending borrow 1 200 --state-file ./x.json --eth-price 1",
        )
        .expect("borrow parses (flag order B)");
        for t in [a, b] {
            match t {
                InProcessTarget::LendingStep(LendingStep::Borrow {
                    account,
                    amount,
                    eth_price,
                }) => {
                    assert_eq!(account, 1);
                    assert_eq!(amount, 200);
                    assert_eq!(eth_price, 1);
                }
                other => panic!("expected Borrow, got {other:?}"),
            }
        }
    }

    #[test]
    fn parse_lending_health_default_eth_price() {
        // No --eth-price → default 1 (matches the CLI default).
        let t = try_parse_in_process("princeps lending health 1 --state-file ./x.json")
            .expect("health parses without --eth-price");
        match t {
            InProcessTarget::LendingStep(LendingStep::Health { account, eth_price }) => {
                assert_eq!(account, 1);
                assert_eq!(eth_price, 1);
            }
            other => panic!("expected Health, got {other:?}"),
        }
    }

    #[test]
    fn parse_lending_scan_and_list() {
        let s = try_parse_in_process(
            "princeps lending scan --eth-price 2 --state-file ./x.json",
        )
        .expect("scan parses");
        let l = try_parse_in_process("princeps lending list --state-file ./x.json")
            .expect("list parses");
        assert!(matches!(
            s,
            InProcessTarget::LendingStep(LendingStep::Scan { eth_price: 2 })
        ));
        assert!(matches!(
            l,
            InProcessTarget::LendingStep(LendingStep::List)
        ));
    }

    #[test]
    fn parse_lending_rejects_unknown_subcommand() {
        // `repay` isn't in the v2 surface yet — fall back to v1 sub-process.
        assert!(try_parse_in_process("princeps lending repay 1 50").is_none());
        // Garbage args.
        assert!(try_parse_in_process("princeps lending deposit one 1000").is_none());
        // Missing positional.
        assert!(try_parse_in_process("princeps lending deposit 1").is_none());
        // Stray extra positional.
        assert!(try_parse_in_process("princeps lending list 5").is_none());
    }

    /// End-to-end walkthrough execution: parser + runner + outcomes.

    #[test]
    fn walkthrough_runs_and_outcomes_pass() {
        let commands = [
            "princeps lending init --state-file ./.test-walk-state.json",
            "princeps lending deposit 1 1000 --state-file ./.test-walk-state.json",
            "princeps lending borrow 1 200 --eth-price 1 --state-file ./.test-walk-state.json",
            "princeps lending health 1 --eth-price 1 --state-file ./.test-walk-state.json",
            "princeps lending health 1 --eth-price 2 --state-file ./.test-walk-state.json",
            "princeps lending scan --eth-price 2 --state-file ./.test-walk-state.json",
            "princeps lending list --state-file ./.test-walk-state.json",
        ];

        let mut bridge: Option<princeps_evm::LiveRethEvmBridge<()>> = None;
        let mut step_results: Vec<LendingStepResult> = Vec::new();
        for cmd in commands {
            match try_parse_in_process(cmd).expect("walkthrough commands all parse") {
                InProcessTarget::LendingStep(s) => {
                    let r = run_lending_step(&s, &mut bridge).expect("step runs");
                    step_results.push(r);
                }
                other => panic!("unexpected target for {cmd}: {other:?}"),
            }
        }

        // Final state should have exactly one position (account 1) with
        // 1000 collateral + positive scaled_debt.
        let last = step_results.last().unwrap();
        assert_eq!(last.positions_after.len(), 1);
        let ((acc, _), pos) = &last.positions_after[0];
        assert_eq!(*acc, 1);
        assert_eq!(pos.collateral_amount, 1000);
        assert!(pos.scaled_debt > 0);

        // Outcome checks via the public evaluate_walk_check path.
        assert!(matches!(
            evaluate_walk_check(&LendingCheck::WalkPositionCount(1), &step_results),
            OutcomeStatus::Pass
        ));
        assert!(matches!(
            evaluate_walk_check(
                &LendingCheck::WalkAccountCollateral { account: 1, amount: 1000 },
                &step_results,
            ),
            OutcomeStatus::Pass
        ));
        assert!(matches!(
            evaluate_walk_check(
                &LendingCheck::WalkAccountHasDebt { account: 1 },
                &step_results,
            ),
            OutcomeStatus::Pass
        ));
        assert!(matches!(
            evaluate_walk_check(
                &LendingCheck::WalkHealthVerdict("HEALTHY".to_string()),
                &step_results,
            ),
            OutcomeStatus::Pass
        ));
        assert!(matches!(
            evaluate_walk_check(&LendingCheck::WalkScanFlaggedExact(0), &step_results),
            OutcomeStatus::Pass
        ));
    }

    #[test]
    fn walkthrough_non_init_first_fails_clearly() {
        let mut bridge: Option<princeps_evm::LiveRethEvmBridge<()>> = None;
        let err = run_lending_step(
            &LendingStep::Deposit { account: 1, amount: 100 },
            &mut bridge,
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("lending init"),
            "expected hint to mention init; got: {msg}"
        );
    }

    #[test]
    fn walkthrough_check_account_mismatch_fails_with_observed_value() {
        let mut bridge: Option<princeps_evm::LiveRethEvmBridge<()>> = None;
        let mut step_results: Vec<LendingStepResult> = Vec::new();
        for s in [
            LendingStep::Init,
            LendingStep::Deposit { account: 1, amount: 1000 },
        ] {
            step_results.push(run_lending_step(&s, &mut bridge).unwrap());
        }
        match evaluate_walk_check(
            &LendingCheck::WalkAccountCollateral { account: 1, amount: 9999 },
            &step_results,
        ) {
            OutcomeStatus::Fail(why) => assert!(why.contains("1000")),
            _ => panic!("expected Fail"),
        }
        // Missing-account case.
        match evaluate_walk_check(
            &LendingCheck::WalkAccountCollateral { account: 42, amount: 0 },
            &step_results,
        ) {
            OutcomeStatus::Fail(why) => assert!(why.contains("no open position")),
            _ => panic!("expected Fail"),
        }
    }

    #[test]
    fn walkthrough_check_with_no_scan_step_fails() {
        let mut bridge: Option<princeps_evm::LiveRethEvmBridge<()>> = None;
        let r = run_lending_step(&LendingStep::Init, &mut bridge).unwrap();
        let results = vec![r];
        match evaluate_walk_check(&LendingCheck::WalkScanFlaggedExact(0), &results) {
            OutcomeStatus::Fail(why) => assert!(why.contains("no scan step")),
            _ => panic!("expected Fail"),
        }
    }

    #[test]
    fn is_v2_eligible_true_for_walkthrough_shape() {
        let s = make_scenario(&[
            "princeps lending init --state-file ./x.json",
            "princeps lending deposit 1 1000 --state-file ./x.json",
            "princeps lending borrow 1 200 --eth-price 1 --state-file ./x.json",
            "princeps lending health 1 --eth-price 1 --state-file ./x.json",
            "princeps lending scan --eth-price 1 --state-file ./x.json",
            "princeps lending list --state-file ./x.json",
        ]);
        assert!(is_v2_eligible(&s));
    }

    #[test]
    fn is_v2_eligible_false_when_walkthrough_mixed_with_subprocess() {
        let s = make_scenario(&[
            "princeps lending init --state-file ./x.json",
            "princeps lending repay 1 50", // not v2-eligible
        ]);
        assert!(!is_v2_eligible(&s));
    }

    /// LendingStep walkthrough check serializes from JSON.
    #[test]
    fn walkthrough_check_round_trips() {
        let json = r#"{
            "name": "test",
            "category": "walkthrough",
            "description": "t",
            "headline": "t",
            "steps": [
                {"explanation": "init", "command": "princeps lending init"}
            ],
            "expected_outcomes": [
                {
                    "name": "one-position",
                    "description": "exactly one position lands",
                    "check": {"walk_position_count": 1}
                },
                {
                    "name": "alice-collat",
                    "description": "alice deposited 1000",
                    "check": {"walk_account_collateral": {"account": 1, "amount": 1000}}
                },
                {
                    "name": "health",
                    "description": "last health is HEALTHY",
                    "check": {"walk_health_verdict": "HEALTHY"}
                }
            ]
        }"#;
        let s: Scenario = serde_json::from_str(json).expect("parse");
        assert_eq!(s.expected_outcomes.len(), 3);
        assert!(matches!(
            s.expected_outcomes[0].check,
            LendingCheck::WalkPositionCount(1)
        ));
        assert!(matches!(
            s.expected_outcomes[1].check,
            LendingCheck::WalkAccountCollateral { account: 1, amount: 1000 }
        ));
        assert!(matches!(
            s.expected_outcomes[2].check,
            LendingCheck::WalkHealthVerdict(_)
        ));
    }

    #[test]
    fn scenario_without_expected_outcomes_still_parses() {
        let json = r#"{
            "name": "test",
            "category": "stress",
            "description": "test",
            "headline": "test",
            "steps": [
                {"explanation": "step", "command": "princeps lending-demo --eth-crash-price 90"}
            ]
        }"#;
        let s: Scenario = serde_json::from_str(json).expect("parse");
        assert!(s.expected_outcomes.is_empty());
    }
}

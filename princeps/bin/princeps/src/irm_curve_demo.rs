//! Standalone IRM curve demo for the princeps lending market.
//!
//! Samples the borrow-rate-vs-utilization curve at 11 evenly-spaced
//! utilization points (0%, 10%, 20%, …, 100%) using the default
//! princeps lending market's IrmParams. Each sample is computed via
//! `princeps_lending::compute_borrow_rate`, the same kernel function
//! the on-chain accrual loop uses every block.
//!
//! Pure Rust against `princeps_lending` primitives — no Reth boot,
//! no Malachite, no validator. The point is to surface the IRM
//! design tradeoff a buyer evaluating the engine actually has to
//! make: where do you put the kink, and how much steeper is the
//! slope above it? This demo shows the answer at-a-glance.
//!
//! Same shape pattern as `run_lending_demo_structured`: pure-compute
//! function returns a structured result; printed wrapper formats for
//! the CLI subcommand; the scenario runner consumes the structured
//! version directly to render the v2 5-section output contract.

use princeps_lending::{compute_borrow_rate, Bps, Index as LendingIndex, IrmParams};

/// One point on the borrow-rate curve.
#[derive(Debug, Clone)]
pub(crate) struct IrmCurvePoint {
    pub utilization_bps: u16,
    /// RAY-scaled per-block borrow rate.
    pub rate_per_block: u128,
    /// Rate expressed as a fraction of `Index::RAY` for human display.
    pub rate_as_ray_fraction: f64,
}

/// Result of one IRM curve sweep.
#[derive(Debug, Clone)]
pub(crate) struct IrmCurveDemoResult {
    /// The IrmParams the curve was sampled against (the default
    /// princeps lending market's).
    pub kink_bps: u16,
    pub base_rate_per_block: u128,
    pub slope_below_kink_per_block: u128,
    pub slope_above_kink_per_block: u128,
    /// Sampled (utilization, rate) points in order of ascending utilization.
    pub points: Vec<IrmCurvePoint>,
}

impl IrmCurveDemoResult {
    /// Convenience: total sample count.
    #[must_use]
    pub(crate) fn sample_count(&self) -> usize {
        self.points.len()
    }

    /// True iff rates never decrease as utilization rises.
    #[must_use]
    pub(crate) fn is_monotonic_non_decreasing(&self) -> bool {
        self.points.windows(2).all(|w| w[1].rate_per_block >= w[0].rate_per_block)
    }

    /// Rate immediately at the kink (last point with utilization ≤ kink).
    #[must_use]
    pub(crate) fn rate_at_kink(&self) -> Option<u128> {
        self.points
            .iter()
            .filter(|p| p.utilization_bps <= self.kink_bps)
            .last()
            .map(|p| p.rate_per_block)
    }

    /// Rate at maximum sampled utilization.
    #[must_use]
    pub(crate) fn rate_at_max(&self) -> Option<u128> {
        self.points.last().map(|p| p.rate_per_block)
    }

    /// Slope-jump factor: how much steeper the rate climbs above the
    /// kink than at the kink itself. For the default params this is
    /// ~10x (slope_above = 10 * slope_below).
    #[must_use]
    pub(crate) fn slope_jump_ratio(&self) -> Option<f64> {
        let kink_rate = self.rate_at_kink()? as f64;
        let max_rate = self.rate_at_max()? as f64;
        if kink_rate == 0.0 {
            return None;
        }
        Some(max_rate / kink_rate)
    }
}

/// The IrmParams used by the default princeps USDC/ETH lending market
/// (`make_default_market` in `main.rs`). Kept in sync deliberately so
/// the demo answers "what curve does the canonical market use?".
fn default_irm_params() -> IrmParams {
    IrmParams {
        base_rate_per_block: 0,
        slope_below_kink_per_block: LendingIndex::RAY / 10_000,
        slope_above_kink_per_block: LendingIndex::RAY / 1_000,
        kink_bps: Bps(8_000),
    }
}

/// Pure-compute version. Samples utilization at 11 points (0%, 10%,
/// …, 100%) and returns the curve.
#[must_use]
pub(crate) fn run_irm_curve_demo_structured() -> IrmCurveDemoResult {
    let params = default_irm_params();
    let ray = LendingIndex::RAY as f64;
    let mut points = Vec::with_capacity(11);
    for step in 0..=10 {
        let utilization_bps: u16 = (step * 1_000) as u16; // 0, 1000, 2000, ..., 10000
        let rate = compute_borrow_rate(Bps(utilization_bps), &params);
        points.push(IrmCurvePoint {
            utilization_bps,
            rate_per_block: rate,
            rate_as_ray_fraction: rate as f64 / ray,
        });
    }
    IrmCurveDemoResult {
        kink_bps: params.kink_bps.0,
        base_rate_per_block: params.base_rate_per_block,
        slope_below_kink_per_block: params.slope_below_kink_per_block,
        slope_above_kink_per_block: params.slope_above_kink_per_block,
        points,
    }
}

/// CLI subcommand body. Prints the curve in a human-readable form.
pub(crate) fn run_irm_curve_demo_cli() {
    let r = run_irm_curve_demo_structured();

    println!();
    println!("=== Princeps — IRM curve demo ===");
    println!();
    println!("    The kernel function `compute_borrow_rate(utilization, params)` is the");
    println!("    same call the on-chain lending accrual uses every block. This demo");
    println!("    samples it across the utilization spectrum so a buyer can SEE the");
    println!("    curve, the kink, and the slope-above-kink multiplier in one screen.");
    println!();
    println!("    Default princeps lending market parameters:");
    println!(
        "      base_rate_per_block        = {} (≈ {:.6} RAY)",
        r.base_rate_per_block,
        r.base_rate_per_block as f64 / LendingIndex::RAY as f64
    );
    println!(
        "      slope_below_kink_per_block = {} (≈ {:.6} RAY)",
        r.slope_below_kink_per_block,
        r.slope_below_kink_per_block as f64 / LendingIndex::RAY as f64
    );
    println!(
        "      slope_above_kink_per_block = {} (≈ {:.6} RAY)",
        r.slope_above_kink_per_block,
        r.slope_above_kink_per_block as f64 / LendingIndex::RAY as f64
    );
    println!("      kink_bps                   = {} ({}%)", r.kink_bps, r.kink_bps / 100);
    println!();

    println!("    Utilization → borrow rate (per-block, as fraction of RAY):");
    println!("      {:<13}  {:<15}  Note", "Utilization", "Rate (×RAY)");
    println!("      {:-<13}  {:-<15}  {}", "", "", "----");
    for p in &r.points {
        let note = if p.utilization_bps == r.kink_bps {
            "kink"
        } else if p.utilization_bps == 10_000 {
            "max"
        } else {
            ""
        };
        println!(
            "      {:<10}{}%  {:<15.8}  {}",
            p.utilization_bps / 100,
            "",
            p.rate_as_ray_fraction,
            note
        );
    }
    println!();

    if let Some(jump) = r.slope_jump_ratio() {
        println!("    Slope-jump factor (rate@max / rate@kink): {jump:.2}x");
        println!("    For the default params, slope_above is {}x slope_below.",
            r.slope_above_kink_per_block / r.slope_below_kink_per_block.max(1));
    }
    println!();
    println!("    The IRM is the engine's design knob: where you put the kink and");
    println!("    how steep the above-kink slope is shapes how aggressively the");
    println!("    market discourages high utilization.");
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_count_is_eleven() {
        let r = run_irm_curve_demo_structured();
        assert_eq!(r.sample_count(), 11, "0%, 10%, ..., 100% = 11 samples");
    }

    #[test]
    fn base_rate_is_zero_at_zero_utilization() {
        let r = run_irm_curve_demo_structured();
        let first = &r.points[0];
        assert_eq!(first.utilization_bps, 0);
        assert_eq!(first.rate_per_block, 0);
    }

    #[test]
    fn curve_is_monotonic_non_decreasing() {
        let r = run_irm_curve_demo_structured();
        assert!(r.is_monotonic_non_decreasing());
    }

    #[test]
    fn rate_at_kink_equals_slope_below_kink() {
        // At utilization = kink, rate = base + slope_below * u/kink
        //                              = 0 + slope_below * 1
        //                              = slope_below
        let r = run_irm_curve_demo_structured();
        assert_eq!(r.rate_at_kink(), Some(r.slope_below_kink_per_block));
    }

    #[test]
    fn slope_jump_factor_at_least_5x() {
        // Default params: slope_above is 10x slope_below, so at max
        // utilization the rate is 11x the rate at kink. The 5x bound
        // is conservative — what matters is that the multiplier is
        // structurally well above 1x.
        let r = run_irm_curve_demo_structured();
        let jump = r.slope_jump_ratio().expect("non-zero kink rate");
        assert!(jump >= 5.0, "slope-jump factor {jump} should be >= 5");
    }
}

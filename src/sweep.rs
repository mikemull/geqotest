use crate::bench::{explain_query, PlanStats};
use postgres::Client;
use rand::Rng;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;

pub fn default_pool_sizes() -> Vec<u32> {
    vec![10, 25, 50, 100, 250, 500, 1000]
}

pub fn default_generations() -> Vec<u32> {
    vec![10, 25, 50, 100, 250, 500, 1000]
}

pub fn default_efforts() -> Vec<u32> {
    (1..=10).collect()
}

pub struct SweepConfig {
    pub pool_sizes: Vec<u32>,
    pub generations: Vec<u32>,
    pub efforts: Vec<u32>,
    /// Samples per grid cell. GEQO is randomized, so one sample per cell is
    /// noise — this should generally be higher than `bench --repeat`, since
    /// variance *is* the thing being measured here.
    pub repeat: usize,
    pub pinned_seed: Option<f64>,
}

/// One EXPLAIN run under one sweep setting.
#[derive(Debug, Clone, Serialize)]
pub struct SweepRun {
    /// "geqo_off" | "auto" | "effort" | "pool_generations"
    pub dimension: String,
    pub pool_size: Option<u32>,
    pub generations: Option<u32>,
    pub effort: Option<u32>,
    pub repeat_index: usize,
    pub geqo_seed: Option<f64>,
    pub planning_time_ms: f64,
    pub execution_time_ms: Option<f64>,
    pub total_cost: f64,
    pub plan_signature: String,
}

type CellKey = (String, Option<u32>, Option<u32>, Option<u32>);

fn push_run(runs: &mut Vec<SweepRun>, dimension: &str, key: (Option<u32>, Option<u32>, Option<u32>), repeat_index: usize, seed: Option<f64>, stats: PlanStats) {
    runs.push(SweepRun {
        dimension: dimension.to_string(),
        pool_size: key.0,
        generations: key.1,
        effort: key.2,
        repeat_index,
        geqo_seed: seed,
        planning_time_ms: stats.planning_time_ms,
        execution_time_ms: stats.execution_time_ms,
        total_cost: stats.total_cost,
        plan_signature: stats.plan_signature,
    });
}

/// Draws (or reuses a pinned) random `geqo_seed` and applies it — GEQO's
/// search is otherwise deterministic per session, so without this every
/// repeat would just replay the same genetic search.
fn apply_seed(client: &mut Client, pinned: Option<f64>, rng: &mut impl Rng) -> anyhow::Result<f64> {
    let seed = pinned.unwrap_or_else(|| rng.random::<f64>());
    client.batch_execute(&format!("SET geqo_seed = {seed};"))?;
    Ok(seed)
}

/// Run every sweep dimension (exhaustive-search reference, GEQO-on-defaults
/// reference, a `geqo_effort` sweep, and a `geqo_pool_size` x
/// `geqo_generations` grid) against one fixed query, `repeat` times each.
pub fn run_sweep(
    client: &mut Client,
    pg_schema: &str,
    sql: &str,
    config: &SweepConfig,
    analyze: bool,
    collapse_limit: u32,
) -> anyhow::Result<Vec<SweepRun>> {
    client.batch_execute(&format!("SET search_path TO {pg_schema}, public;"))?;
    // See bench::run_bench: without this, Postgres' join_collapse_limit
    // default (8) silently caps how much of the join the optimizer (GEQO
    // included) will even try to reorder, independent of these GEQO knobs.
    client.batch_execute(&format!(
        "SET join_collapse_limit = {collapse_limit}; SET from_collapse_limit = {collapse_limit};"
    ))?;
    let mut rng = rand::rng();
    let mut runs = Vec::new();

    // Reference: exhaustive search (deterministic, no GEQO knobs apply).
    client.batch_execute("SET geqo = off;")?;
    let stats = explain_query(client, sql, analyze)?;
    push_run(&mut runs, "geqo_off", (None, None, None), 0, None, stats);
    client.batch_execute("RESET geqo;")?;

    // Reference: GEQO with everything left at its own defaults.
    for repeat_index in 0..config.repeat.max(1) {
        client.batch_execute("SET geqo = on; RESET geqo_pool_size; RESET geqo_generations; RESET geqo_effort;")?;
        let seed = apply_seed(client, config.pinned_seed, &mut rng)?;
        let stats = explain_query(client, sql, analyze)?;
        push_run(&mut runs, "auto", (None, None, None), repeat_index, Some(seed), stats);
    }

    // geqo_effort sweep: pool_size/generations stay at their auto default
    // (0), so effort is what drives them — this is how anyone would tune
    // GEQO in practice, before reaching for manual pool_size/generations.
    for &effort in &config.efforts {
        for repeat_index in 0..config.repeat.max(1) {
            client.batch_execute(&format!(
                "SET geqo = on; RESET geqo_pool_size; RESET geqo_generations; SET geqo_effort = {effort};"
            ))?;
            let seed = apply_seed(client, config.pinned_seed, &mut rng)?;
            let stats = explain_query(client, sql, analyze)?;
            push_run(&mut runs, "effort", (None, None, Some(effort)), repeat_index, Some(seed), stats);
        }
    }

    // geqo_pool_size x geqo_generations grid: explicit, nonzero values on
    // both override geqo_effort's influence entirely.
    for &pool_size in &config.pool_sizes {
        for &generations in &config.generations {
            for repeat_index in 0..config.repeat.max(1) {
                client.batch_execute(&format!(
                    "SET geqo = on; SET geqo_pool_size = {pool_size}; SET geqo_generations = {generations};"
                ))?;
                let seed = apply_seed(client, config.pinned_seed, &mut rng)?;
                let stats = explain_query(client, sql, analyze)?;
                push_run(
                    &mut runs,
                    "pool_generations",
                    (Some(pool_size), Some(generations), None),
                    repeat_index,
                    Some(seed),
                    stats,
                );
            }
        }
    }

    client.batch_execute(
        "RESET geqo; RESET geqo_pool_size; RESET geqo_generations; RESET geqo_effort; RESET geqo_seed;",
    )?;
    Ok(runs)
}

/// One (dimension, pool_size, generations, effort) setting, aggregated
/// across its repeats.
#[derive(Debug, Clone, Serialize)]
pub struct SweepCell {
    pub dimension: String,
    pub pool_size: Option<u32>,
    pub generations: Option<u32>,
    pub effort: Option<u32>,
    pub repeats: usize,
    pub cost_mean: f64,
    pub cost_min: f64,
    pub cost_max: f64,
    pub planning_time_mean_ms: f64,
    pub planning_time_min_ms: f64,
    pub planning_time_max_ms: f64,
    /// True if no other cell is at least as good on both planning time and
    /// cost (and strictly better on at least one) — the actually useful
    /// "given a time budget, what's the best cost achievable" answer.
    pub pareto_optimal: bool,
}

fn mean(v: &[f64]) -> f64 {
    v.iter().sum::<f64>() / v.len() as f64
}
fn min_of(v: &[f64]) -> f64 {
    v.iter().cloned().fold(f64::INFINITY, f64::min)
}
fn max_of(v: &[f64]) -> f64 {
    v.iter().cloned().fold(f64::NEG_INFINITY, f64::max)
}

pub fn aggregate_cells(runs: &[SweepRun]) -> Vec<SweepCell> {
    let mut groups: BTreeMap<CellKey, Vec<&SweepRun>> = BTreeMap::new();
    for r in runs {
        groups
            .entry((r.dimension.clone(), r.pool_size, r.generations, r.effort))
            .or_default()
            .push(r);
    }

    let mut cells: Vec<SweepCell> = groups
        .into_iter()
        .map(|((dimension, pool_size, generations, effort), rs)| {
            let costs: Vec<f64> = rs.iter().map(|r| r.total_cost).collect();
            let times: Vec<f64> = rs.iter().map(|r| r.planning_time_ms).collect();
            SweepCell {
                dimension,
                pool_size,
                generations,
                effort,
                repeats: rs.len(),
                cost_mean: mean(&costs),
                cost_min: min_of(&costs),
                cost_max: max_of(&costs),
                planning_time_mean_ms: mean(&times),
                planning_time_min_ms: min_of(&times),
                planning_time_max_ms: max_of(&times),
                pareto_optimal: false,
            }
        })
        .collect();

    mark_pareto_optimal(&mut cells);
    cells
}

fn mark_pareto_optimal(cells: &mut [SweepCell]) {
    let points: Vec<(f64, f64)> = cells.iter().map(|c| (c.planning_time_mean_ms, c.cost_mean)).collect();
    for i in 0..cells.len() {
        let dominated = (0..cells.len()).any(|j| {
            j != i
                && points[j].0 <= points[i].0
                && points[j].1 <= points[i].1
                && (points[j].0 < points[i].0 || points[j].1 < points[i].1)
        });
        cells[i].pareto_optimal = !dominated;
    }
}

pub fn write_runs(runs: &[SweepRun], path: &Path) -> anyhow::Result<()> {
    let mut writer = csv::Writer::from_path(path)?;
    for r in runs {
        writer.serialize(r)?;
    }
    writer.flush()?;
    Ok(())
}

pub fn write_cells(cells: &[SweepCell], path: &Path) -> anyhow::Result<()> {
    let mut writer = csv::Writer::from_path(path)?;
    for c in cells {
        writer.serialize(c)?;
    }
    writer.flush()?;
    Ok(())
}

fn setting_label(c: &SweepCell) -> String {
    match c.dimension.as_str() {
        "geqo_off" => "-".to_string(),
        "auto" => "defaults".to_string(),
        "effort" => format!("effort={}", c.effort.unwrap_or(0)),
        "pool_generations" => {
            format!("pool={} gens={}", c.pool_size.unwrap_or(0), c.generations.unwrap_or(0))
        }
        other => other.to_string(),
    }
}

pub fn print_summary(cells: &[SweepCell]) {
    println!(
        "{:<18} {:<24} {:>3} {:>12} {:>12} {:>12} {:>10} {:>10} {:>10}",
        "dimension", "setting", "n", "cost_mean", "cost_min", "cost_max", "plan_mean", "plan_min", "plan_max"
    );
    for c in cells {
        println!(
            "{:<18} {:<24} {:>3} {:>12.1} {:>12.1} {:>12.1} {:>10.3} {:>10.3} {:>10.3}{}",
            c.dimension,
            setting_label(c),
            c.repeats,
            c.cost_mean,
            c.cost_min,
            c.cost_max,
            c.planning_time_mean_ms,
            c.planning_time_min_ms,
            c.planning_time_max_ms,
            if c.pareto_optimal { "  *" } else { "" },
        );
    }

    let mut frontier: Vec<&SweepCell> = cells.iter().filter(|c| c.pareto_optimal).collect();
    frontier.sort_by(|a, b| a.planning_time_mean_ms.partial_cmp(&b.planning_time_mean_ms).unwrap());

    println!(
        "\nPareto frontier (* above): best cost achievable at each planning-time budget, cheapest-time first:"
    );
    for c in frontier {
        println!(
            "  {:<18} {:<24} plan~{:.3}ms -> cost~{:.1} (range {:.1}-{:.1})",
            c.dimension,
            setting_label(c),
            c.planning_time_mean_ms,
            c.cost_mean,
            c.cost_min,
            c.cost_max,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(dimension: &str, pool_size: Option<u32>, generations: Option<u32>, effort: Option<u32>, time: f64, cost: f64) -> SweepRun {
        SweepRun {
            dimension: dimension.to_string(),
            pool_size,
            generations,
            effort,
            repeat_index: 0,
            geqo_seed: None,
            planning_time_ms: time,
            execution_time_ms: None,
            total_cost: cost,
            plan_signature: "x".into(),
        }
    }

    #[test]
    fn aggregates_mean_min_max_per_distinct_setting() {
        let runs = vec![
            run("pool_generations", Some(10), Some(10), None, 5.0, 100.0),
            run("pool_generations", Some(10), Some(10), None, 7.0, 120.0),
            run("pool_generations", Some(20), Some(10), None, 9.0, 90.0),
        ];
        let cells = aggregate_cells(&runs);
        assert_eq!(cells.len(), 2, "two distinct (pool_size, generations) settings");

        let cell_10_10 = cells.iter().find(|c| c.pool_size == Some(10)).unwrap();
        assert_eq!(cell_10_10.repeats, 2);
        assert_eq!(cell_10_10.cost_mean, 110.0);
        assert_eq!(cell_10_10.cost_min, 100.0);
        assert_eq!(cell_10_10.cost_max, 120.0);
        assert_eq!(cell_10_10.planning_time_mean_ms, 6.0);
    }

    #[test]
    fn pareto_frontier_excludes_dominated_settings_and_keeps_tradeoffs() {
        // A: fast+cheap (dominates C). B: slower but even cheaper (a real
        // tradeoff vs A, so not dominated). C: slower AND more expensive
        // than A on both axes, so strictly dominated.
        let runs = vec![
            run("effort", None, None, Some(1), 10.0, 100.0), // A
            run("effort", None, None, Some(5), 20.0, 50.0),  // B
            run("effort", None, None, Some(9), 30.0, 150.0), // C, dominated by A
        ];
        let cells = aggregate_cells(&runs);

        let is_optimal = |effort: u32| cells.iter().find(|c| c.effort == Some(effort)).unwrap().pareto_optimal;
        assert!(is_optimal(1), "A: fastest and cheapest among non-dominating alternatives");
        assert!(is_optimal(5), "B: a genuine time/cost tradeoff against A");
        assert!(!is_optimal(9), "C: strictly worse than A on both axes");
    }
}

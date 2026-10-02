use crate::queries::Query;
use postgres::Client;
use rand::Rng;
use serde::Serialize;
use std::path::Path;

pub const GEQO_ON_LABEL: &str = "geqo_on";
pub const GEQO_OFF_LABEL: &str = "geqo_off";

#[derive(Debug, Clone)]
pub struct GeqoConfig {
    pub label: String,
    pub geqo: Option<bool>,
    pub geqo_threshold: Option<u32>,
    /// How many times to re-plan each query under this config. Only
    /// meaningful when `geqo == Some(true)`: GEQO's genetic search is
    /// randomized, so re-planning the identical query can pick a different
    /// join order/cost. `geqo_off` is deterministic, so it's always run once.
    pub repeat: usize,
    /// Pin `geqo_seed` to this value on every repeat instead of drawing a
    /// fresh random one each time (useful to reproduce one specific run).
    pub pinned_seed: Option<f64>,
}

/// Build the two configs we compare every qualifying (above-threshold) query
/// under: GEQO forced off (exhaustive search, the counterfactual) and GEQO
/// on (optionally with an overridden threshold), repeated `repeat` times to
/// surface GEQO's run-to-run variability.
pub fn build_configs(threshold_override: Option<u32>, repeat: usize, pinned_seed: Option<f64>) -> Vec<GeqoConfig> {
    vec![
        GeqoConfig {
            label: GEQO_OFF_LABEL.into(),
            geqo: Some(false),
            geqo_threshold: None,
            repeat: 1,
            pinned_seed: None,
        },
        GeqoConfig {
            label: GEQO_ON_LABEL.into(),
            geqo: Some(true),
            geqo_threshold: threshold_override,
            repeat: repeat.max(1),
            pinned_seed,
        },
    ]
}

#[derive(Debug, Serialize)]
pub struct BenchResult {
    pub query_id: String,
    pub label: String,
    pub num_tables: usize,
    pub config: String,
    pub repeat_index: usize,
    pub geqo_seed: Option<f64>,
    pub planning_time_ms: f64,
    pub execution_time_ms: Option<f64>,
    pub total_cost: f64,
    /// Canonical `NodeType(child,child,...)` string (relation names at the
    /// leaves) describing the concrete plan shape/join order chosen — the
    /// thing that actually varies between GEQO runs, not just the cost.
    pub plan_signature: String,
}

pub fn run_bench(
    client: &mut Client,
    pg_schema: &str,
    queries: &[Query],
    configs: &[GeqoConfig],
    analyze: bool,
    collapse_limit: u32,
) -> anyhow::Result<Vec<BenchResult>> {
    client.batch_execute(&format!("SET search_path TO {pg_schema}, public;"))?;
    // Postgres only lets the optimizer (GEQO included) reorder as many
    // explicitly-JOINed tables as join_collapse_limit allows (default 8);
    // beyond that it silently keeps part of the join tree fixed in whatever
    // order the query text was written. Raise it so the full query is
    // actually exposed to whichever join-order search we're benchmarking.
    client.batch_execute(&format!(
        "SET join_collapse_limit = {collapse_limit}; SET from_collapse_limit = {collapse_limit};"
    ))?;
    let mut rng = rand::rng();

    let mut results = Vec::new();
    for config in configs {
        for q in queries {
            for repeat_index in 0..config.repeat.max(1) {
                let seed_used = apply_config(client, config, &mut rng)?;
                let mut result = explain_one(client, q, &config.label, analyze)?;
                result.repeat_index = repeat_index;
                result.geqo_seed = seed_used;
                results.push(result);
            }
        }
        reset_config(client, config)?;
    }
    Ok(results)
}

/// Applies the config's GEQO settings, drawing (or reusing a pinned) random
/// `geqo_seed` when GEQO is on. Returns the seed actually used, if any.
fn apply_config(client: &mut Client, config: &GeqoConfig, rng: &mut impl Rng) -> anyhow::Result<Option<f64>> {
    if let Some(geqo) = config.geqo {
        client.batch_execute(&format!("SET geqo = {};", if geqo { "on" } else { "off" }))?;
    }
    if let Some(threshold) = config.geqo_threshold {
        client.batch_execute(&format!("SET geqo_threshold = {threshold};"))?;
    }

    if config.geqo == Some(true) {
        // geqo_seed is only meaningful with geqo on; left unset, it defaults
        // to a fixed value, so identical repeats would just replay the same
        // genetic search. Drawing a fresh one is what actually exercises the
        // randomness the user wants to observe.
        let seed = config.pinned_seed.unwrap_or_else(|| rng.random::<f64>());
        client.batch_execute(&format!("SET geqo_seed = {seed};"))?;
        Ok(Some(seed))
    } else {
        Ok(None)
    }
}

fn reset_config(client: &mut Client, config: &GeqoConfig) -> anyhow::Result<()> {
    if config.geqo.is_some() {
        client.batch_execute("RESET geqo;")?;
    }
    if config.geqo_threshold.is_some() {
        client.batch_execute("RESET geqo_threshold;")?;
    }
    if config.geqo == Some(true) {
        client.batch_execute("RESET geqo_seed;")?;
    }
    Ok(())
}

/// Cost/timing/shape extracted from one `EXPLAIN` run, independent of
/// whatever query or GEQO config produced it — shared by `bench` and
/// `sweep` so they don't each reimplement JSON plan parsing.
#[derive(Debug, Clone)]
pub struct PlanStats {
    pub planning_time_ms: f64,
    pub execution_time_ms: Option<f64>,
    pub total_cost: f64,
    /// Canonical `NodeType(child,child,...)` string (relation names at the
    /// leaves) describing the concrete plan shape/join order chosen — the
    /// thing that actually varies between GEQO runs, not just the cost.
    pub plan_signature: String,
}

/// Run `EXPLAIN` (plan-only, or with `ANALYZE` if `analyze`) on `sql` against
/// whatever session GUCs the caller has already set, and extract its stats.
pub fn explain_query(client: &mut Client, sql: &str, analyze: bool) -> anyhow::Result<PlanStats> {
    // SUMMARY true forces "Planning Time" into the JSON even without ANALYZE
    // (by default it's only emitted alongside ANALYZE's execution stats).
    let explain_kw = if analyze {
        "EXPLAIN (ANALYZE, FORMAT JSON, SUMMARY true)"
    } else {
        "EXPLAIN (FORMAT JSON, SUMMARY true)"
    };
    let full_sql = format!("{explain_kw} {sql}");
    let row = client.query_one(&full_sql, &[])?;
    let json: serde_json::Value = row.get(0);
    let top = &json[0];
    let plan = &top["Plan"];

    Ok(PlanStats {
        planning_time_ms: top["Planning Time"].as_f64().unwrap_or(0.0),
        execution_time_ms: top.get("Execution Time").and_then(|v| v.as_f64()),
        total_cost: plan["Total Cost"].as_f64().unwrap_or(0.0),
        plan_signature: plan_signature(plan),
    })
}

fn explain_one(client: &mut Client, q: &Query, config_label: &str, analyze: bool) -> anyhow::Result<BenchResult> {
    let stats = explain_query(client, &q.sql, analyze)?;
    Ok(BenchResult {
        query_id: q.id.clone(),
        label: q.label.clone(),
        num_tables: q.num_tables,
        config: config_label.to_string(),
        repeat_index: 0,
        geqo_seed: None,
        planning_time_ms: stats.planning_time_ms,
        execution_time_ms: stats.execution_time_ms,
        total_cost: stats.total_cost,
        plan_signature: stats.plan_signature,
    })
}

/// Canonicalize a plan tree into `NodeType(childSig,childSig,...)`, with
/// scan nodes collapsed to the relation they scan. Two runs that picked the
/// same join order and join methods produce the same signature; anything
/// GEQO did differently (order, join method, which side is outer) shows up
/// as a different string.
fn plan_signature(node: &serde_json::Value) -> String {
    if let Some(rel) = node.get("Relation Name").and_then(|v| v.as_str()) {
        return rel.to_string();
    }
    let node_type = node.get("Node Type").and_then(|v| v.as_str()).unwrap_or("?");
    match node.get("Plans").and_then(|v| v.as_array()) {
        Some(children) if !children.is_empty() => {
            let parts: Vec<String> = children.iter().map(plan_signature).collect();
            format!("{node_type}({})", parts.join(","))
        }
        _ => node_type.to_string(),
    }
}

pub fn write_results(results: &[BenchResult], path: &Path) -> anyhow::Result<()> {
    let mut writer = csv::Writer::from_path(path)?;
    for r in results {
        writer.serialize(r)?;
    }
    writer.flush()?;
    Ok(())
}

pub fn print_summary(results: &[BenchResult]) {
    println!(
        "{:<16} {:>3} {:<10} {:>3} {:>8} {:>10} {:>10} {:>10}  {}",
        "query", "n", "config", "rep", "seed", "plan(ms)", "exec(ms)", "cost", "plan"
    );
    for r in results {
        let exec = r
            .execution_time_ms
            .map(|v| format!("{v:.3}"))
            .unwrap_or_else(|| "-".to_string());
        let seed = r.geqo_seed.map(|s| format!("{s:.4}")).unwrap_or_else(|| "-".to_string());
        println!(
            "{:<16} {:>3} {:<10} {:>3} {:>8} {:>10.3} {:>10} {:>10.1}  {}",
            r.query_id, r.num_tables, r.config, r.repeat_index, seed, r.planning_time_ms, exec, r.total_cost,
            r.plan_signature,
        );
    }
    print_geqo_variability(results);
}

/// Per-query summary of `geqo_on` (averaged across its repeats) vs.
/// `geqo_off` (the exhaustive-search counterfactual), for cost, planning
/// time, and execution time.
#[derive(Debug, Serialize)]
pub struct GeqoComparison {
    pub query_id: String,
    pub num_tables: usize,
    pub geqo_on_repeats: usize,
    pub geqo_off_cost: f64,
    pub geqo_on_cost_avg: f64,
    pub geqo_on_cost_min: f64,
    pub geqo_on_cost_max: f64,
    pub cost_pct_diff: f64,
    pub geqo_off_planning_ms: f64,
    pub geqo_on_planning_ms_avg: f64,
    pub planning_pct_diff: f64,
    pub geqo_off_exec_ms: Option<f64>,
    pub geqo_on_exec_ms_avg: Option<f64>,
    pub exec_pct_diff: Option<f64>,
}

/// Pair up each query's single `geqo_off` run with its (possibly repeated)
/// `geqo_on` runs and summarize the difference. Percent diffs are
/// `(geqo_on avg - geqo_off) / geqo_off * 100`: positive means GEQO came out
/// higher/slower than exhaustive search, negative means GEQO did better.
pub fn compute_comparisons(results: &[BenchResult]) -> Vec<GeqoComparison> {
    use std::collections::BTreeMap;

    let mut off_by_query: BTreeMap<&str, &BenchResult> = BTreeMap::new();
    let mut on_by_query: BTreeMap<&str, Vec<&BenchResult>> = BTreeMap::new();
    for r in results {
        if r.config == GEQO_OFF_LABEL {
            off_by_query.insert(r.query_id.as_str(), r);
        } else if r.config == GEQO_ON_LABEL {
            on_by_query.entry(r.query_id.as_str()).or_default().push(r);
        }
    }

    fn pct_diff(off: f64, on: f64) -> f64 {
        if off.abs() > f64::EPSILON { (on - off) / off * 100.0 } else { 0.0 }
    }
    fn avg(values: &[f64]) -> f64 {
        values.iter().sum::<f64>() / values.len() as f64
    }

    let mut out = Vec::new();
    for (query_id, off) in &off_by_query {
        let Some(on) = on_by_query.get(*query_id).filter(|runs| !runs.is_empty()) else {
            continue;
        };

        let on_costs: Vec<f64> = on.iter().map(|r| r.total_cost).collect();
        let on_planning: Vec<f64> = on.iter().map(|r| r.planning_time_ms).collect();
        let on_exec: Vec<f64> = on.iter().filter_map(|r| r.execution_time_ms).collect();
        let on_exec_avg = (!on_exec.is_empty()).then(|| avg(&on_exec));

        out.push(GeqoComparison {
            query_id: query_id.to_string(),
            num_tables: off.num_tables,
            geqo_on_repeats: on.len(),
            geqo_off_cost: off.total_cost,
            geqo_on_cost_avg: avg(&on_costs),
            geqo_on_cost_min: on_costs.iter().cloned().fold(f64::INFINITY, f64::min),
            geqo_on_cost_max: on_costs.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
            cost_pct_diff: pct_diff(off.total_cost, avg(&on_costs)),
            geqo_off_planning_ms: off.planning_time_ms,
            geqo_on_planning_ms_avg: avg(&on_planning),
            planning_pct_diff: pct_diff(off.planning_time_ms, avg(&on_planning)),
            geqo_off_exec_ms: off.execution_time_ms,
            geqo_on_exec_ms_avg: on_exec_avg,
            exec_pct_diff: match (off.execution_time_ms, on_exec_avg) {
                (Some(o), Some(a)) => Some(pct_diff(o, a)),
                _ => None,
            },
        });
    }
    out
}

pub fn write_comparisons(comparisons: &[GeqoComparison], path: &Path) -> anyhow::Result<()> {
    let mut writer = csv::Writer::from_path(path)?;
    for c in comparisons {
        writer.serialize(c)?;
    }
    writer.flush()?;
    Ok(())
}

pub fn print_comparison_summary(comparisons: &[GeqoComparison]) {
    if comparisons.is_empty() {
        return;
    }
    println!("\nGEQO ({GEQO_ON_LABEL}, avg of its repeats) vs exhaustive search ({GEQO_OFF_LABEL}):");
    println!(
        "{:<10} {:>3} {:>12} {:>12} {:>8} {:>10} {:>12} {:>8} {:>10} {:>12} {:>8}  verdict",
        "query", "n", "off_cost", "on_cost", "cost_d%", "off_plan", "on_plan", "plan_d%", "off_exec", "on_exec", "exec_d%"
    );
    for c in comparisons {
        let exec_off = c.geqo_off_exec_ms.map(|v| format!("{v:.3}")).unwrap_or_else(|| "-".into());
        let exec_on = c.geqo_on_exec_ms_avg.map(|v| format!("{v:.3}")).unwrap_or_else(|| "-".into());
        let exec_pct = c.exec_pct_diff.map(|v| format!("{v:+.1}%")).unwrap_or_else(|| "-".into());
        let verdict = if c.cost_pct_diff > 1.0 {
            "GEQO WORSE"
        } else if c.cost_pct_diff < -1.0 {
            "GEQO BETTER"
        } else {
            "tie"
        };
        println!(
            "{:<10} {:>3} {:>12.1} {:>12.1} {:>7.1}% {:>10.3} {:>12.3} {:>7.1}% {:>10} {:>12} {:>8}  {verdict}",
            c.query_id,
            c.num_tables,
            c.geqo_off_cost,
            c.geqo_on_cost_avg,
            c.cost_pct_diff,
            c.geqo_off_planning_ms,
            c.geqo_on_planning_ms_avg,
            c.planning_pct_diff,
            exec_off,
            exec_on,
            exec_pct,
        );
    }
    println!(
        "(d% = (geqo_on avg - geqo_off) / geqo_off * 100; positive = GEQO higher/slower than \
         exhaustive search. geqo_on columns are averaged across its {} repeat(s); on_cost also \
         has min/max in comparison.csv.)",
        comparisons.first().map(|c| c.geqo_on_repeats).unwrap_or(1)
    );
}

/// For queries run more than once under GEQO, report how many distinct
/// plans (and costs) were actually produced across those repeats.
fn print_geqo_variability(results: &[BenchResult]) {
    use std::collections::BTreeMap;

    let mut by_query: BTreeMap<&str, Vec<&BenchResult>> = BTreeMap::new();
    for r in results.iter().filter(|r| r.config == GEQO_ON_LABEL) {
        by_query.entry(r.query_id.as_str()).or_default().push(r);
    }
    if !by_query.values().any(|runs| runs.len() > 1) {
        return;
    }

    println!("\nGEQO run-to-run variability ({GEQO_ON_LABEL}, repeated runs):");
    for (query_id, runs) in by_query {
        if runs.len() < 2 {
            continue;
        }
        let distinct_plans: std::collections::HashSet<&str> =
            runs.iter().map(|r| r.plan_signature.as_str()).collect();
        let costs: Vec<String> = runs.iter().map(|r| format!("{:.1}", r.total_cost)).collect();
        println!(
            "  {query_id}: {} repeats -> {} distinct plan(s) (costs: {})",
            runs.len(),
            distinct_plans.len(),
            costs.join(", "),
        );
    }
}

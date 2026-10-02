mod bench;
mod cli;
mod db;
mod ddl;
mod generate;
mod manifest;
mod queries;
mod schema;
mod sweep;
mod words;

use anyhow::{Context, Result};
use cli::{BenchArgs, Cli, Command, GenerateArgs, LoadArgs, RunArgs, SweepArgs};
use clap::Parser;
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Generate(args) => cmd_generate(&args),
        Command::Load(args) => cmd_load(&args),
        Command::Bench(args) => cmd_bench(&args),
        Command::Run(args) => cmd_run(&args),
        Command::Sweep(args) => cmd_sweep(&args),
    }
}

fn cmd_generate(args: &GenerateArgs) -> Result<()> {
    std::fs::create_dir_all(&args.out_dir)?;

    let seed = args.seed.unwrap_or_else(|| rand::rng().random());
    let mut rng = ChaCha8Rng::seed_from_u64(seed);

    let filter_selectivity_max = args.filter_selectivity_max.unwrap_or(args.filter_selectivity);
    let sizing = schema::SizingParams {
        min_rows: args.min_rows,
        max_rows: args.max_rows,
        fact_multiplier: args.fact_multiplier,
        filter_selectivity_min: args.filter_selectivity,
        filter_selectivity_max,
    };
    let built = schema::build_schema(args.schema, args.tables, &sizing, &mut rng);

    println!(
        "Generating {} tables ({} topology), seed={seed} -> {}",
        built.tables.len(),
        built.topology,
        args.out_dir.display()
    );

    generate::generate_data(&built, args.skew, seed, &args.out_dir).context("generating table data")?;

    let ddl = ddl::render_ddl(&built, &args.pg_schema);
    std::fs::write(args.out_dir.join("schema.sql"), &ddl)?;

    let qs = queries::generate_queries(&built, &args.pg_schema, args.join_syntax);
    queries::save_queries(&qs, &args.out_dir)?;
    let queries_sql: String = qs
        .iter()
        .map(|q| format!("-- {} ({} tables): {}\n{}\n\n", q.id, q.num_tables, q.label, q.sql))
        .collect();
    std::fs::write(args.out_dir.join("queries.sql"), queries_sql)?;

    let num_tables = built.tables.len();
    let manifest = manifest::Manifest {
        schema: built,
        seed,
        skew: args.skew,
        filter_selectivity_min: args.filter_selectivity,
        filter_selectivity_max,
        join_syntax: args.join_syntax,
        pg_schema: args.pg_schema.clone(),
        generated_at: chrono::Utc::now().to_rfc3339(),
    };
    manifest::save(&manifest, &args.out_dir)?;

    println!(
        "Wrote schema.sql, queries.sql/json, manifest.json, and {num_tables} table CSVs to {}",
        args.out_dir.display()
    );
    println!(
        "Postgres' geqo_threshold defaults to 12; with {num_tables} tables, queries joining more \
         than that many tables will engage GEQO on a default server."
    );
    let (sel_min, sel_max) = (args.filter_selectivity.clamp(0.0, 1.0), filter_selectivity_max.clamp(0.0, 1.0));
    if sel_min >= 1.0 && sel_max >= 1.0 {
        println!(
            "filter_selectivity=1.0: no WHERE filters were added, so joins stay lossless FK->PK \
             1:1 and plan cost will likely be near order-invariant. Pass --filter-selectivity \
             below 1.0 (default 0.1) if you want join order to actually affect cost."
        );
    } else if (sel_max - sel_min).abs() < f64::EPSILON {
        println!(
            "Each non-fact table gets a WHERE filter passing the SAME ~{:.0}% of its rows. Note \
             that giving every table equal selectivity makes plan cost close to order-invariant \
             (every join shrinks the running row count by the same proportion); pass \
             --filter-selectivity-max to vary selectivity per table instead.",
            sel_min * 100.0
        );
    } else {
        println!(
            "Each non-fact table gets its OWN WHERE filter, independently passing between \
             ~{:.0}% and ~{:.0}% of its rows — varying selectivity across tables is what makes \
             join order genuinely change plan cost.",
            sel_min * 100.0,
            sel_max * 100.0
        );
    }
    match args.join_syntax {
        queries::JoinSyntax::Explicit => println!(
            "join_syntax=explicit: queries use JOIN...ON, so Postgres' join_collapse_limit \
             (default 8) caps how much of the join the optimizer will reorder unless you raise it \
             with `bench --join-collapse-limit`."
        ),
        queries::JoinSyntax::Comma => println!(
            "join_syntax=comma: queries use a flat FROM a, b, c WHERE ... list, which Postgres \
             parses with nothing to collapse — the whole join is exposed to the optimizer \
             unconditionally, so only geqo_threshold decides whether GEQO engages."
        ),
    }
    Ok(())
}

fn cmd_load(args: &LoadArgs) -> Result<()> {
    let manifest = manifest::load(&args.out_dir).context("run `generate` first")?;
    let ddl = std::fs::read_to_string(args.out_dir.join("schema.sql"))?;
    let mut client = db::connect(&args.dsn).context("connecting to Postgres")?;

    println!(
        "Creating schema `{}`{}...",
        manifest.pg_schema,
        if args.recreate { " (recreating)" } else { "" }
    );
    db::create_schema(&mut client, &manifest.pg_schema, &ddl, args.recreate)?;

    for table in &manifest.schema.tables {
        let csv_path = args.out_dir.join("tables").join(format!("{}.csv", table.name));
        let rows = db::load_table_csv(&mut client, &manifest.pg_schema, &table.name, &csv_path)?;
        println!("  loaded {rows} rows into {}.{}", manifest.pg_schema, table.name);
    }
    println!("Load complete.");
    Ok(())
}

fn cmd_bench(args: &BenchArgs) -> Result<()> {
    let manifest = manifest::load(&args.out_dir).context("run `generate` first")?;
    let all_queries = queries::load_queries(&args.out_dir).context("run `generate` first")?;
    let mut client = db::connect(&args.dsn).context("connecting to Postgres")?;

    let server_threshold: i32 = client
        .query_one("SHOW geqo_threshold", &[])
        .ok()
        .and_then(|row| row.try_get::<_, String>(0).ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(12);
    let effective_threshold = args.geqo_threshold.map(|t| t as i32).unwrap_or(server_threshold);

    let qs: Vec<_> = if args.include_below_threshold {
        all_queries
    } else {
        all_queries
            .into_iter()
            .filter(|q| q.num_tables as i32 > effective_threshold)
            .collect()
    };

    println!(
        "Server geqo_threshold = {server_threshold}{}; schema has {} tables; {} of the generated \
         queries invoke GEQO (> {effective_threshold} tables).",
        args.geqo_threshold
            .map(|t| format!(" (overridden to {t} for this run)"))
            .unwrap_or_default(),
        manifest.schema.tables.len(),
        qs.len(),
    );

    if qs.is_empty() {
        println!(
            "No queries exceed the GEQO threshold ({effective_threshold} tables) so there's nothing \
             to benchmark. Generate with more tables (--tables > {effective_threshold}), pass \
             --geqo-threshold to lower the threshold, or pass --include-below-threshold to \
             benchmark the full query ladder anyway."
        );
        return Ok(());
    }

    let max_query_tables = qs.iter().map(|q| q.num_tables).max().unwrap_or(8) as u32;
    let collapse_limit = args.join_collapse_limit.unwrap_or(max_query_tables);
    println!(
        "Using join_collapse_limit = from_collapse_limit = {collapse_limit} (Postgres defaults to \
         8, which caps how many explicitly-JOINed tables the optimizer will reorder regardless of \
         geqo_threshold; pass --join-collapse-limit 8 to reproduce that default cap)."
    );

    let configs = bench::build_configs(args.geqo_threshold, args.repeat, args.geqo_seed);
    let results = bench::run_bench(&mut client, &manifest.pg_schema, &qs, &configs, args.analyze, collapse_limit)?;

    let results_path = args.out_dir.join(&args.results_file);
    bench::write_results(&results, &results_path)?;
    bench::print_summary(&results);

    let comparisons = bench::compute_comparisons(&results);
    let comparison_path = args.out_dir.join("comparison.csv");
    bench::write_comparisons(&comparisons, &comparison_path)?;
    bench::print_comparison_summary(&comparisons);

    println!(
        "Wrote {} result rows to {} and {} comparison rows to {}",
        results.len(),
        results_path.display(),
        comparisons.len(),
        comparison_path.display()
    );
    Ok(())
}

fn cmd_run(args: &RunArgs) -> Result<()> {
    cmd_generate(&args.generate)?;

    cmd_load(&LoadArgs {
        out_dir: args.generate.out_dir.clone(),
        dsn: args.dsn.clone(),
        recreate: args.recreate,
    })?;

    cmd_bench(&BenchArgs {
        out_dir: args.generate.out_dir.clone(),
        dsn: args.dsn.clone(),
        analyze: args.analyze,
        geqo_threshold: args.geqo_threshold,
        include_below_threshold: args.include_below_threshold,
        repeat: args.repeat,
        geqo_seed: args.geqo_seed,
        join_collapse_limit: args.join_collapse_limit,
        results_file: args.results_file.clone(),
    })
}

fn cmd_sweep(args: &SweepArgs) -> Result<()> {
    let manifest = manifest::load(&args.out_dir).context("run `generate` first")?;
    let all_queries = queries::load_queries(&args.out_dir).context("run `generate` first")?;
    let query = all_queries
        .into_iter()
        .find(|q| q.num_tables == args.num_tables)
        .with_context(|| {
            format!(
                "no generated query joins exactly {} tables — check queries.json for available sizes",
                args.num_tables
            )
        })?;

    let mut client = db::connect(&args.dsn).context("connecting to Postgres")?;
    let collapse_limit = args.join_collapse_limit.unwrap_or(args.num_tables as u32);

    println!(
        "Sweeping GEQO tuning for {} ({} tables); join_collapse_limit={collapse_limit}, {} repeat(s) per setting.",
        query.id, query.num_tables, args.repeat
    );

    let config = sweep::SweepConfig {
        pool_sizes: args.pool_sizes.clone().unwrap_or_else(sweep::default_pool_sizes),
        generations: args.generations.clone().unwrap_or_else(sweep::default_generations),
        efforts: args.efforts.clone().unwrap_or_else(sweep::default_efforts),
        repeat: args.repeat,
        pinned_seed: args.geqo_seed,
    };

    let grid_cells = config.pool_sizes.len() * config.generations.len();
    let total_calls = 1 + config.repeat * (1 + config.efforts.len() + grid_cells);
    println!(
        "{grid_cells} pool_size x generations cells, {} effort levels, plus references -> {total_calls} \
         EXPLAIN calls total.",
        config.efforts.len(),
    );

    let runs = sweep::run_sweep(&mut client, &manifest.pg_schema, &query.sql, &config, args.analyze, collapse_limit)?;

    let runs_path = args.out_dir.join(&args.runs_file);
    sweep::write_runs(&runs, &runs_path)?;

    let cells = sweep::aggregate_cells(&runs);
    let summary_path = args.out_dir.join(&args.summary_file);
    sweep::write_cells(&cells, &summary_path)?;
    sweep::print_summary(&cells);

    println!(
        "Wrote {} raw runs to {} and {} aggregated settings to {}",
        runs.len(),
        runs_path.display(),
        cells.len(),
        summary_path.display()
    );
    Ok(())
}

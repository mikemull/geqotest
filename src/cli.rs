use crate::queries::JoinSyntax;
use crate::schema::Topology;
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "geqotest",
    version,
    about = "Generate synthetic schemas/data and benchmark PostgreSQL's join-order optimizer (GEQO)."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Generate schema DDL, CSV data, and join queries into an output directory.
    Generate(GenerateArgs),
    /// Load a previously generated schema + data into Postgres.
    Load(LoadArgs),
    /// Run EXPLAIN against a loaded database and report GEQO-relevant plan stats.
    Bench(BenchArgs),
    /// Generate, load, and bench in one step.
    Run(RunArgs),
    /// Sweep GEQO tuning knobs (geqo_effort, geqo_pool_size, geqo_generations)
    /// against one query and report the planning-time/cost tradeoff.
    Sweep(SweepArgs),
}

#[derive(Args, Clone)]
pub struct GenerateArgs {
    /// Number of tables to generate. Postgres' geqo_threshold defaults to 12:
    /// joins at or below that size get exhaustive search, larger ones fall
    /// back to the genetic optimizer.
    #[arg(long, default_value_t = 12)]
    pub tables: usize,

    /// Join topology to generate.
    #[arg(long, value_enum, default_value_t = Topology::Star)]
    pub schema: Topology,

    /// Minimum row count for a generated table (dimension/chain tables).
    #[arg(long, default_value_t = 1_000)]
    pub min_rows: usize,

    /// Maximum row count for a generated table (dimension/chain tables).
    #[arg(long, default_value_t = 10_000)]
    pub max_rows: usize,

    /// Fact table row count = (largest dimension's row count) * this value.
    /// Ignored for the chain topology (no fact table).
    #[arg(long, default_value_t = 20)]
    pub fact_multiplier: usize,

    /// Zipfian exponent for foreign-key reference skew (0 = uniform random).
    #[arg(long, default_value_t = 1.2)]
    pub skew: f64,

    /// Fraction of rows (0.0-1.0) that pass a WHERE filter added to every
    /// non-fact table. Without a filter, every join here is a lossless
    /// FK->PK 1:1 join that preserves row counts, so total plan cost ends up
    /// nearly invariant to join order and GEQO has nothing to meaningfully
    /// search for. 1.0 disables filtering. Used as a fixed value for every
    /// table unless --filter-selectivity-max is also given.
    #[arg(long, default_value_t = 0.1)]
    pub filter_selectivity: f64,

    /// Upper bound for per-table filter selectivity variation. When given,
    /// each table's selectivity is drawn independently and uniformly from
    /// [filter-selectivity, filter-selectivity-max] instead of using
    /// filter-selectivity as a fixed value for every table. Giving every
    /// table the *same* selectivity makes plan cost close to order-invariant
    /// (every join shrinks the running row count by the same proportion
    /// regardless of order) — varying it per table is what actually rewards
    /// a smart join order, the way real predicates with different
    /// selectivities do.
    #[arg(long)]
    pub filter_selectivity_max: Option<f64>,

    /// SQL join style to emit. `explicit` (`JOIN ... ON`) is capped by
    /// Postgres' join_collapse_limit (default 8) unless you raise it at
    /// bench time (see `bench --join-collapse-limit`). `comma`
    /// (`FROM a, b, c WHERE ...`) is parsed as an already-flat relation
    /// list with nothing to collapse, so the whole join is exposed to the
    /// optimizer unconditionally — only geqo_threshold decides whether
    /// GEQO engages.
    #[arg(long, value_enum, default_value_t = JoinSyntax::Explicit)]
    pub join_syntax: JoinSyntax,

    /// RNG seed; omit for a random one (printed on every run for repro).
    #[arg(long)]
    pub seed: Option<u64>,

    /// Postgres schema (namespace) the generated tables will live under.
    #[arg(long, default_value = "geqotest")]
    pub pg_schema: String,

    /// Directory to write schema.sql, queries.sql/json, manifest.json, and tables/*.csv into.
    #[arg(long, default_value = "out")]
    pub out_dir: PathBuf,
}

#[derive(Args, Clone)]
pub struct LoadArgs {
    /// Directory previously populated by `generate`.
    #[arg(long, default_value = "out")]
    pub out_dir: PathBuf,

    /// Postgres connection string, e.g. postgres://user:pass@localhost/dbname
    #[arg(long, env = "GEQOTEST_DSN")]
    pub dsn: String,

    /// Drop and recreate the target Postgres schema before loading.
    #[arg(long, default_value_t = false)]
    pub recreate: bool,
}

#[derive(Args, Clone)]
pub struct BenchArgs {
    /// Directory previously populated by `generate`.
    #[arg(long, default_value = "out")]
    pub out_dir: PathBuf,

    /// Postgres connection string, e.g. postgres://user:pass@localhost/dbname
    #[arg(long, env = "GEQOTEST_DSN")]
    pub dsn: String,

    /// Use EXPLAIN ANALYZE (actually executes each query) instead of plan-only EXPLAIN.
    #[arg(long, default_value_t = false)]
    pub analyze: bool,

    /// Override the server's geqo_threshold for the GEQO-on config (also
    /// used to decide which generated queries "invoke GEQO"). Omit to use
    /// whatever the server currently has configured.
    #[arg(long)]
    pub geqo_threshold: Option<u32>,

    /// Benchmark every generated query regardless of size, not just the ones
    /// whose join count exceeds the (effective) geqo_threshold.
    #[arg(long, default_value_t = false)]
    pub include_below_threshold: bool,

    /// Re-plan each qualifying query this many times under GEQO (each with a
    /// fresh random geqo_seed) to surface GEQO's run-to-run variability.
    #[arg(long, default_value_t = 1)]
    pub repeat: usize,

    /// Pin geqo_seed to this value on every repeat instead of drawing a
    /// fresh random one each time.
    #[arg(long)]
    pub geqo_seed: Option<f64>,

    /// Session join_collapse_limit (and from_collapse_limit) to use while
    /// benchmarking. Postgres defaults this to 8, which silently caps how
    /// many explicitly-JOINed tables the optimizer (GEQO included) will even
    /// try to reorder, independent of geqo_threshold. Omit to auto-raise it
    /// to the largest query being benchmarked so the full join list is
    /// always exposed; pass 8 explicitly to reproduce the default cap.
    #[arg(long)]
    pub join_collapse_limit: Option<u32>,

    /// Results CSV filename, written inside out_dir.
    #[arg(long, default_value = "results.csv")]
    pub results_file: PathBuf,
}

#[derive(Args, Clone)]
pub struct RunArgs {
    #[command(flatten)]
    pub generate: GenerateArgs,

    /// Postgres connection string, e.g. postgres://user:pass@localhost/dbname
    #[arg(long, env = "GEQOTEST_DSN")]
    pub dsn: String,

    /// Drop and recreate the target Postgres schema before loading.
    #[arg(long, default_value_t = false)]
    pub recreate: bool,

    /// Use EXPLAIN ANALYZE (actually executes each query) instead of plan-only EXPLAIN.
    #[arg(long, default_value_t = false)]
    pub analyze: bool,

    /// Override the server's geqo_threshold for the GEQO-on config (also
    /// used to decide which generated queries "invoke GEQO"). Omit to use
    /// whatever the server currently has configured.
    #[arg(long)]
    pub geqo_threshold: Option<u32>,

    /// Benchmark every generated query regardless of size, not just the ones
    /// whose join count exceeds the (effective) geqo_threshold.
    #[arg(long, default_value_t = false)]
    pub include_below_threshold: bool,

    /// Re-plan each qualifying query this many times under GEQO (each with a
    /// fresh random geqo_seed) to surface GEQO's run-to-run variability.
    #[arg(long, default_value_t = 1)]
    pub repeat: usize,

    /// Pin geqo_seed to this value on every repeat instead of drawing a
    /// fresh random one each time.
    #[arg(long)]
    pub geqo_seed: Option<f64>,

    /// Session join_collapse_limit (and from_collapse_limit) to use while
    /// benchmarking. Postgres defaults this to 8, which silently caps how
    /// many explicitly-JOINed tables the optimizer (GEQO included) will even
    /// try to reorder, independent of geqo_threshold. Omit to auto-raise it
    /// to the largest query being benchmarked so the full join list is
    /// always exposed; pass 8 explicitly to reproduce the default cap.
    #[arg(long)]
    pub join_collapse_limit: Option<u32>,

    /// Results CSV filename, written inside out_dir.
    #[arg(long, default_value = "results.csv")]
    pub results_file: PathBuf,
}

#[derive(Args, Clone)]
pub struct SweepArgs {
    /// Directory previously populated by `generate`.
    #[arg(long, default_value = "out")]
    pub out_dir: PathBuf,

    /// Postgres connection string, e.g. postgres://user:pass@localhost/dbname
    #[arg(long, env = "GEQOTEST_DSN")]
    pub dsn: String,

    /// Which rung of the generated query ladder to sweep, by its table
    /// count (e.g. 14 picks the query that joins exactly 14 tables).
    #[arg(long)]
    pub num_tables: usize,

    /// geqo_pool_size values to grid over (with --generations), e.g.
    /// 10,50,200,1000. Omit for a log-spaced default.
    #[arg(long, value_delimiter = ',')]
    pub pool_sizes: Option<Vec<u32>>,

    /// geqo_generations values to grid over (with --pool-sizes). Omit for a
    /// log-spaced default.
    #[arg(long, value_delimiter = ',')]
    pub generations: Option<Vec<u32>>,

    /// geqo_effort values (1-10) to sweep independently of pool_size and
    /// generations — effort auto-derives both of those when they're left at
    /// their default of 0, so this is how GEQO is tuned in practice before
    /// reaching for manual pool_size/generations. Omit to sweep all of 1-10.
    #[arg(long, value_delimiter = ',')]
    pub efforts: Option<Vec<u32>>,

    /// Samples per setting. GEQO is randomized, so one sample per setting is
    /// noise; defaults higher than `bench --repeat` since variance *is* the
    /// thing being measured here.
    #[arg(long, default_value_t = 5)]
    pub repeat: usize,

    /// Pin geqo_seed to this value on every sample instead of drawing a
    /// fresh random one each time.
    #[arg(long)]
    pub geqo_seed: Option<f64>,

    /// Use EXPLAIN ANALYZE (actually executes the query on every sample)
    /// instead of plan-only EXPLAIN. A full sweep runs many EXPLAINs, so
    /// this can get slow — left off by default.
    #[arg(long, default_value_t = false)]
    pub analyze: bool,

    /// Session join_collapse_limit (and from_collapse_limit) to use while
    /// sweeping. Omit to auto-raise it to the swept query's table count so
    /// the full join is exposed; pass 8 to reproduce Postgres' default cap.
    #[arg(long)]
    pub join_collapse_limit: Option<u32>,

    /// Raw per-run CSV filename, written inside out_dir.
    #[arg(long, default_value = "sweep_runs.csv")]
    pub runs_file: PathBuf,

    /// Aggregated per-setting CSV filename, written inside out_dir.
    #[arg(long, default_value = "sweep_summary.csv")]
    pub summary_file: PathBuf,
}

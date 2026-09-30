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
    /// non-fact table in each generated query. Without a filter, every join
    /// here is a lossless FK->PK 1:1 join that preserves row counts, so
    /// total plan cost ends up nearly invariant to join order and GEQO has
    /// nothing to meaningfully search for. 1.0 disables filtering.
    #[arg(long, default_value_t = 0.1)]
    pub filter_selectivity: f64,

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

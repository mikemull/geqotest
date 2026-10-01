use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Topology {
    /// t1 <- t2 <- t3 <- ... <- tN, each table joined to exactly one neighbor.
    Chain,
    /// One fact table referencing N-1 independent dimension tables.
    Star,
    /// A star schema where roughly half of the dimensions are further
    /// normalized into a sub-dimension (dim -> subdim), i.e. two join levels.
    Snowflake,
    /// Every table joins directly to every other table (a complete join
    /// graph, C(n,2) edges). All tables share one row count so their PK
    /// domains overlap, making `ti.id = tj.id` valid for every pair. This is
    /// the hardest case for join-order search: every subset of relations is
    /// connected, so nothing can be pruned by connectivity alone (see
    /// Moerkotte & Neumann's DP join-enumeration benchmarks).
    Clique,
}

impl std::fmt::Display for Topology {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Topology::Chain => "chain",
            Topology::Star => "star",
            Topology::Snowflake => "snowflake",
            Topology::Clique => "clique",
        };
        write!(f, "{s}")
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum ColumnType {
    BigInt,
    Integer,
    Text,
    DoublePrecision,
    Numeric,
    Date,
}

impl ColumnType {
    pub fn sql(&self) -> &'static str {
        match self {
            ColumnType::BigInt => "BIGINT",
            ColumnType::Integer => "INTEGER",
            ColumnType::Text => "TEXT",
            ColumnType::DoublePrecision => "DOUBLE PRECISION",
            ColumnType::Numeric => "NUMERIC(12,2)",
            ColumnType::Date => "DATE",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    pub ty: ColumnType,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForeignKey {
    /// Column in this table holding the reference.
    pub column: String,
    /// Table being referenced.
    pub ref_table: String,
    /// Column being referenced in `ref_table` (always its primary key).
    pub ref_column: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Table {
    pub name: String,
    pub row_count: usize,
    /// Primary key column name (always the first column).
    pub pk: String,
    pub columns: Vec<Column>,
    pub foreign_keys: Vec<ForeignKey>,
    /// Fraction of this table's rows (0.0-1.0) that pass a generated
    /// `attribute_1 <= threshold` predicate in queries that include it.
    /// `None` for tables with no `attribute_1` column (fact tables).
    /// Sampled independently per table from
    /// `[filter_selectivity_min, filter_selectivity_max]` at schema-build
    /// time — different tables having different selectivity is what makes
    /// join order actually change plan cost; see `SizingParams`.
    pub filter_selectivity: Option<f64>,
}

impl Table {
    pub fn fk_to(&self, ref_table: &str) -> Option<&ForeignKey> {
        self.foreign_keys.iter().find(|fk| fk.ref_table == ref_table)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schema {
    pub topology: Topology,
    /// Tables in dependency order: a table only references tables that
    /// appear earlier in this list.
    pub tables: Vec<Table>,
}

impl Schema {
    pub fn table(&self, name: &str) -> &Table {
        self.tables
            .iter()
            .find(|t| t.name == name)
            .unwrap_or_else(|| panic!("unknown table {name}"))
    }
}

fn attribute_columns() -> Vec<Column> {
    vec![
        Column { name: "name".into(), ty: ColumnType::Text },
        Column { name: "code".into(), ty: ColumnType::Text },
        Column { name: "attribute_1".into(), ty: ColumnType::DoublePrecision },
        Column { name: "created_at".into(), ty: ColumnType::Date },
    ]
}

fn fact_measure_columns() -> Vec<Column> {
    vec![
        Column { name: "quantity".into(), ty: ColumnType::Integer },
        Column { name: "amount".into(), ty: ColumnType::Numeric },
        Column { name: "event_date".into(), ty: ColumnType::Date },
    ]
}

pub struct SizingParams {
    pub min_rows: usize,
    pub max_rows: usize,
    pub fact_multiplier: usize,
    /// Range each table's filter_selectivity is independently sampled from.
    /// Equal min/max gives every table the same fixed selectivity (which
    /// makes plan cost close to order-invariant); a real range gives each
    /// table its own selectivity, which is what actually rewards a smart
    /// join order.
    pub filter_selectivity_min: f64,
    pub filter_selectivity_max: f64,
}

/// Build a schema definition (no data yet) for the requested topology.
///
/// `num_tables` is the total table count, matched against Postgres'
/// `geqo_threshold` (default 12): joins at or below the threshold use
/// exhaustive search, joins above it fall back to the genetic optimizer.
pub fn build_schema(
    topology: Topology,
    num_tables: usize,
    sizing: &SizingParams,
    rng: &mut impl rand::Rng,
) -> Schema {
    assert!(num_tables >= 2, "need at least 2 tables to join");

    let tables = match topology {
        Topology::Chain => build_chain(num_tables, sizing, rng),
        Topology::Star => build_star(num_tables, sizing, rng),
        Topology::Snowflake => build_snowflake(num_tables, sizing, rng),
        Topology::Clique => build_clique(num_tables, sizing, rng),
    };

    Schema { topology, tables }
}

fn random_row_count(sizing: &SizingParams, rng: &mut impl rand::Rng) -> usize {
    rng.random_range(sizing.min_rows..=sizing.max_rows)
}

fn random_filter_selectivity(sizing: &SizingParams, rng: &mut impl rand::Rng) -> f64 {
    let lo = sizing.filter_selectivity_min.min(sizing.filter_selectivity_max);
    let hi = sizing.filter_selectivity_min.max(sizing.filter_selectivity_max);
    if (hi - lo).abs() < f64::EPSILON { lo } else { rng.random_range(lo..=hi) }
}

fn build_chain(num_tables: usize, sizing: &SizingParams, rng: &mut impl rand::Rng) -> Vec<Table> {
    let mut tables = Vec::with_capacity(num_tables);
    for i in 1..=num_tables {
        let name = format!("t{i}");
        let mut columns = vec![Column { name: "id".into(), ty: ColumnType::BigInt }];
        let mut foreign_keys = Vec::new();
        if i > 1 {
            let prev = format!("t{}", i - 1);
            let fk_col = format!("{prev}_id");
            columns.push(Column { name: fk_col.clone(), ty: ColumnType::BigInt });
            foreign_keys.push(ForeignKey { column: fk_col, ref_table: prev, ref_column: "id".into() });
        }
        columns.extend(attribute_columns());
        tables.push(Table {
            name,
            row_count: random_row_count(sizing, rng),
            pk: "id".into(),
            columns,
            foreign_keys,
            filter_selectivity: Some(random_filter_selectivity(sizing, rng)),
        });
    }
    tables
}

fn build_star(num_tables: usize, sizing: &SizingParams, rng: &mut impl rand::Rng) -> Vec<Table> {
    let num_dims = num_tables - 1;
    let mut tables = Vec::with_capacity(num_tables);

    // Dimensions first: fact references them, so they must exist already.
    for i in 1..=num_dims {
        let name = format!("dim{i}");
        let mut columns = vec![Column { name: "id".into(), ty: ColumnType::BigInt }];
        columns.extend(attribute_columns());
        tables.push(Table {
            name,
            row_count: random_row_count(sizing, rng),
            pk: "id".into(),
            columns,
            foreign_keys: Vec::new(),
            filter_selectivity: Some(random_filter_selectivity(sizing, rng)),
        });
    }

    let mut fact_columns = vec![Column { name: "id".into(), ty: ColumnType::BigInt }];
    let mut fact_fks = Vec::new();
    for i in 1..=num_dims {
        let dim = format!("dim{i}");
        let fk_col = format!("{dim}_id");
        fact_columns.push(Column { name: fk_col.clone(), ty: ColumnType::BigInt });
        fact_fks.push(ForeignKey { column: fk_col, ref_table: dim, ref_column: "id".into() });
    }
    fact_columns.extend(fact_measure_columns());

    let dim_max_rows = tables.iter().map(|t| t.row_count).max().unwrap_or(sizing.max_rows);
    tables.push(Table {
        name: "fact".into(),
        row_count: (dim_max_rows * sizing.fact_multiplier).max(sizing.min_rows),
        pk: "id".into(),
        columns: fact_columns,
        foreign_keys: fact_fks,
        filter_selectivity: None,
    });

    tables
}

fn build_snowflake(num_tables: usize, sizing: &SizingParams, rng: &mut impl rand::Rng) -> Vec<Table> {
    let num_non_fact = num_tables - 1;
    let num_dims = num_non_fact.div_ceil(2);
    let num_subdims = num_non_fact - num_dims;

    let mut tables = Vec::with_capacity(num_tables);

    // Leaf sub-dimensions first (lower cardinality, referenced by a dim).
    let mut subdim_row_counts = Vec::with_capacity(num_subdims);
    for i in 1..=num_subdims {
        let name = format!("subdim{i}");
        let mut columns = vec![Column { name: "id".into(), ty: ColumnType::BigInt }];
        columns.extend(attribute_columns());
        // Sub-dimensions sit further up the normalization hierarchy, so they
        // hold fewer, lower-cardinality rows than the dimensions above them.
        let row_count = (random_row_count(sizing, rng) / 4).max(2);
        subdim_row_counts.push(row_count);
        tables.push(Table {
            name,
            row_count,
            pk: "id".into(),
            columns,
            foreign_keys: Vec::new(),
            filter_selectivity: Some(random_filter_selectivity(sizing, rng)),
        });
    }

    // Dimensions, each optionally referencing one sub-dimension.
    for i in 1..=num_dims {
        let name = format!("dim{i}");
        let mut columns = vec![Column { name: "id".into(), ty: ColumnType::BigInt }];
        let mut foreign_keys = Vec::new();
        if i <= num_subdims {
            let subdim = format!("subdim{i}");
            let fk_col = format!("{subdim}_id");
            columns.push(Column { name: fk_col.clone(), ty: ColumnType::BigInt });
            foreign_keys.push(ForeignKey { column: fk_col, ref_table: subdim, ref_column: "id".into() });
        }
        columns.extend(attribute_columns());
        tables.push(Table {
            name,
            row_count: random_row_count(sizing, rng),
            pk: "id".into(),
            columns,
            foreign_keys,
            filter_selectivity: Some(random_filter_selectivity(sizing, rng)),
        });
    }

    let mut fact_columns = vec![Column { name: "id".into(), ty: ColumnType::BigInt }];
    let mut fact_fks = Vec::new();
    for i in 1..=num_dims {
        let dim = format!("dim{i}");
        let fk_col = format!("{dim}_id");
        fact_columns.push(Column { name: fk_col.clone(), ty: ColumnType::BigInt });
        fact_fks.push(ForeignKey { column: fk_col, ref_table: dim, ref_column: "id".into() });
    }
    fact_columns.extend(fact_measure_columns());

    let dim_max_rows = tables
        .iter()
        .filter(|t| t.name.starts_with("dim"))
        .map(|t| t.row_count)
        .max()
        .unwrap_or(sizing.max_rows);
    tables.push(Table {
        name: "fact".into(),
        row_count: (dim_max_rows * sizing.fact_multiplier).max(sizing.min_rows),
        pk: "id".into(),
        columns: fact_columns,
        foreign_keys: fact_fks,
        filter_selectivity: None,
    });

    tables
}

fn build_clique(num_tables: usize, sizing: &SizingParams, rng: &mut impl rand::Rng) -> Vec<Table> {
    // Every table shares one row count so their `id` PK domains overlap
    // exactly (all are 1..=row_count), making `ti.id = tj.id` a valid,
    // non-empty join for every pair without needing a separate FK column
    // per edge — the shared PK column doubles as every pairwise FK.
    let row_count = random_row_count(sizing, rng);
    let mut tables = Vec::with_capacity(num_tables);
    for i in 1..=num_tables {
        let name = format!("t{i}");
        let mut columns = vec![Column { name: "id".into(), ty: ColumnType::BigInt }];
        columns.extend(attribute_columns());
        let foreign_keys = (1..i)
            .map(|j| ForeignKey { column: "id".into(), ref_table: format!("t{j}"), ref_column: "id".into() })
            .collect();
        let filter_selectivity = Some(random_filter_selectivity(sizing, rng));
        tables.push(Table { name, row_count, pk: "id".into(), columns, foreign_keys, filter_selectivity });
    }
    tables
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand_chacha::ChaCha8Rng;

    fn sizing() -> SizingParams {
        SizingParams {
            min_rows: 10,
            max_rows: 100,
            fact_multiplier: 5,
            filter_selectivity_min: 0.1,
            filter_selectivity_max: 0.1,
        }
    }

    fn rng() -> ChaCha8Rng {
        ChaCha8Rng::seed_from_u64(7)
    }

    /// Every FK must reference a table already present earlier in
    /// `schema.tables` — generation and query building both depend on that.
    fn assert_dependency_ordered(schema: &Schema) {
        for (i, table) in schema.tables.iter().enumerate() {
            for fk in &table.foreign_keys {
                let ref_idx = schema
                    .tables
                    .iter()
                    .position(|t| t.name == fk.ref_table)
                    .unwrap_or_else(|| panic!("{} references unknown table {}", table.name, fk.ref_table));
                assert!(ref_idx < i, "{} must be defined before {}", fk.ref_table, table.name);
            }
        }
    }

    #[test]
    fn topologies_produce_requested_table_count_and_valid_dependencies() {
        for topology in [Topology::Chain, Topology::Star, Topology::Snowflake, Topology::Clique] {
            for num_tables in [2, 3, 12, 25] {
                let schema = build_schema(topology, num_tables, &sizing(), &mut rng());
                assert_eq!(schema.tables.len(), num_tables, "{topology} with {num_tables} tables");
                assert_dependency_ordered(&schema);
            }
        }
    }

    #[test]
    fn star_and_snowflake_have_exactly_one_fact_table_referencing_every_dimension() {
        for topology in [Topology::Star, Topology::Snowflake] {
            let schema = build_schema(topology, 12, &sizing(), &mut rng());
            let fact = schema.table("fact");
            let num_dims = schema.tables.iter().filter(|t| t.name.starts_with("dim")).count();
            assert_eq!(fact.foreign_keys.len(), num_dims);
        }
    }

    #[test]
    fn clique_has_every_pairwise_edge_and_a_shared_row_count() {
        let n = 6;
        let schema = build_schema(Topology::Clique, n, &sizing(), &mut rng());

        let total_edges: usize = schema.tables.iter().map(|t| t.foreign_keys.len()).sum();
        assert_eq!(total_edges, n * (n - 1) / 2, "clique of {n} tables should have C(n,2) edges");

        let row_counts: std::collections::HashSet<usize> =
            schema.tables.iter().map(|t| t.row_count).collect();
        assert_eq!(row_counts.len(), 1, "clique tables must share one row count so PK domains overlap");
    }

    #[test]
    fn fact_tables_have_no_filter_selectivity_others_do() {
        for topology in [Topology::Star, Topology::Snowflake] {
            let schema = build_schema(topology, 12, &sizing(), &mut rng());
            assert_eq!(schema.table("fact").filter_selectivity, None);
            for t in schema.tables.iter().filter(|t| t.name != "fact") {
                assert!(t.filter_selectivity.is_some(), "{} should have a filter_selectivity", t.name);
            }
        }
        for topology in [Topology::Chain, Topology::Clique] {
            let schema = build_schema(topology, 8, &sizing(), &mut rng());
            assert!(schema.tables.iter().all(|t| t.filter_selectivity.is_some()));
        }
    }

    #[test]
    fn per_table_selectivity_varies_within_the_requested_range() {
        let sizing = SizingParams {
            min_rows: 10,
            max_rows: 100,
            fact_multiplier: 5,
            filter_selectivity_min: 0.05,
            filter_selectivity_max: 0.5,
        };
        let schema = build_schema(Topology::Chain, 10, &sizing, &mut rng());
        let values: Vec<f64> = schema.tables.iter().filter_map(|t| t.filter_selectivity).collect();
        assert!(values.iter().all(|v| (0.05..=0.5).contains(v)), "{values:?}");
        let distinct: std::collections::BTreeMap<u64, ()> =
            values.iter().map(|v| ((v * 1e9) as u64, ())).collect();
        assert!(distinct.len() > 1, "expected per-table variation, got {values:?}");
    }
}

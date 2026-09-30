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
}

impl std::fmt::Display for Topology {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Topology::Chain => "chain",
            Topology::Star => "star",
            Topology::Snowflake => "snowflake",
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
    };

    Schema { topology, tables }
}

fn random_row_count(sizing: &SizingParams, rng: &mut impl rand::Rng) -> usize {
    rng.random_range(sizing.min_rows..=sizing.max_rows)
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
    });

    tables
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand_chacha::ChaCha8Rng;

    fn sizing() -> SizingParams {
        SizingParams { min_rows: 10, max_rows: 100, fact_multiplier: 5 }
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
        for topology in [Topology::Chain, Topology::Star, Topology::Snowflake] {
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
}

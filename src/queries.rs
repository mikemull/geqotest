use crate::schema::{Schema, Table, Topology};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Query {
    pub id: String,
    pub label: String,
    pub num_tables: usize,
    pub sql: String,
}

/// Generate a ladder of join queries of increasing size, so a run can show
/// planning behavior on both sides of Postgres' `geqo_threshold`.
///
/// `filter_selectivity` (0.0..=1.0) adds a `WHERE table.attribute_1 <= ...`
/// predicate to every non-fact table in the join, sized to pass roughly that
/// fraction of rows. Without it, every join here is a lossless FK->PK 1:1
/// join that preserves row counts, which makes total plan cost close to
/// mathematically invariant to join order — there's nothing for GEQO's
/// search to actually improve on. A real filter breaks that symmetry so
/// join order (and which table gets filtered first) genuinely changes cost.
/// 1.0 passes every row, effectively disabling the filter.
pub fn generate_queries(schema: &Schema, pg_schema: &str, filter_selectivity: f64) -> Vec<Query> {
    match schema.topology {
        Topology::Chain => chain_queries(schema, pg_schema, filter_selectivity),
        Topology::Star => star_queries(schema, pg_schema, filter_selectivity),
        Topology::Snowflake => snowflake_queries(schema, pg_schema, filter_selectivity),
    }
}

pub fn save_queries(queries: &[Query], out_dir: &Path) -> anyhow::Result<()> {
    let path = out_dir.join("queries.json");
    std::fs::write(path, serde_json::to_string_pretty(queries)?)?;
    Ok(())
}

pub fn load_queries(out_dir: &Path) -> anyhow::Result<Vec<Query>> {
    let path = out_dir.join("queries.json");
    let data = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&data)?)
}

fn qualify(pg_schema: &str, table: &str) -> String {
    format!("{pg_schema}.{table}")
}

/// Build `SELECT count(*) FROM t1 JOIN t2 ON ... JOIN t3 ON ... WHERE ...;`
/// where each table after the first must share an FK edge (in either
/// direction) with at least one table already added — callers are
/// responsible for ordering `tables` so that holds. Every included table
/// that has an `attribute_1` column (i.e. every non-fact table) gets a
/// `attribute_1 <= threshold` predicate sized by `filter_selectivity`.
fn build_join_sql(pg_schema: &str, tables: &[&Table], filter_selectivity: f64) -> String {
    let mut included: Vec<&Table> = vec![tables[0]];
    let mut sql = format!("SELECT count(*) FROM {}", qualify(pg_schema, &tables[0].name));

    for t in &tables[1..] {
        let mut condition = None;
        for inc in &included {
            if let Some(fk) = t.fk_to(&inc.name) {
                condition = Some(format!(
                    "{}.{} = {}.{}",
                    qualify(pg_schema, &t.name),
                    fk.column,
                    qualify(pg_schema, &inc.name),
                    fk.ref_column
                ));
                break;
            }
            if let Some(fk) = inc.fk_to(&t.name) {
                condition = Some(format!(
                    "{}.{} = {}.{}",
                    qualify(pg_schema, &inc.name),
                    fk.column,
                    qualify(pg_schema, &t.name),
                    fk.ref_column
                ));
                break;
            }
        }
        let condition = condition
            .unwrap_or_else(|| panic!("no FK edge between {} and tables added so far", t.name));
        sql.push_str(&format!("\n  JOIN {} ON {}", qualify(pg_schema, &t.name), condition));
        included.push(t);
    }

    // attribute_1 is generated uniformly over [0.0, 1000.0), so this
    // threshold passes roughly `filter_selectivity` of a table's rows.
    // 1.0 means "pass everything" — skip emitting a no-op WHERE entirely.
    let clamped = filter_selectivity.clamp(0.0, 1.0);
    if clamped < 1.0 {
        let threshold = clamped * 1000.0;
        let filters: Vec<String> = included
            .iter()
            .filter(|t| t.columns.iter().any(|c| c.name == "attribute_1"))
            .map(|t| format!("{}.attribute_1 <= {threshold:.2}", qualify(pg_schema, &t.name)))
            .collect();
        if !filters.is_empty() {
            sql.push_str("\n  WHERE ");
            sql.push_str(&filters.join(" AND "));
        }
    }

    sql.push(';');
    sql
}

fn chain_queries(schema: &Schema, pg_schema: &str, filter_selectivity: f64) -> Vec<Query> {
    let n = schema.tables.len();
    (2..=n)
        .map(|k| {
            let tables: Vec<&Table> = schema.tables[0..k].iter().collect();
            Query {
                id: format!("chain_{k:02}"),
                label: format!("chain join of {k} tables"),
                num_tables: k,
                sql: build_join_sql(pg_schema, &tables, filter_selectivity),
            }
        })
        .collect()
}

fn star_queries(schema: &Schema, pg_schema: &str, filter_selectivity: f64) -> Vec<Query> {
    let fact = schema.table("fact");
    let dims: Vec<&Table> = schema.tables.iter().filter(|t| t.name != "fact").collect();

    (1..=dims.len())
        .map(|k| {
            let mut tables = vec![fact];
            tables.extend_from_slice(&dims[0..k]);
            Query {
                id: format!("star_{:02}", k + 1),
                label: format!("star join: fact + {k} dimension(s)"),
                num_tables: k + 1,
                sql: build_join_sql(pg_schema, &tables, filter_selectivity),
            }
        })
        .collect()
}

fn snowflake_queries(schema: &Schema, pg_schema: &str, filter_selectivity: f64) -> Vec<Query> {
    let fact = schema.table("fact");
    let dims: Vec<&Table> = schema
        .tables
        .iter()
        .filter(|t| t.name.starts_with("dim"))
        .collect();

    let mut queries = Vec::new();
    let mut running: Vec<&Table> = vec![fact];
    for (i, dim) in dims.iter().enumerate() {
        running.push(dim);
        if let Some(fk) = dim.foreign_keys.first() {
            running.push(schema.table(&fk.ref_table));
        }
        queries.push(Query {
            id: format!("snowflake_{:02}", running.len()),
            label: format!("snowflake join: fact + {} dimension branch(es)", i + 1),
            num_tables: running.len(),
            sql: build_join_sql(pg_schema, &running, filter_selectivity),
        });
    }
    queries
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{build_schema, SizingParams, Topology};
    use rand::SeedableRng;
    use rand_chacha::ChaCha8Rng;

    fn sizing() -> SizingParams {
        SizingParams { min_rows: 10, max_rows: 100, fact_multiplier: 5 }
    }

    fn is_word_byte(b: u8) -> bool {
        b.is_ascii_alphanumeric() || b == b'_'
    }

    /// Count non-overlapping, word-boundary-respecting occurrences of
    /// `needle` in `haystack` (a plain `contains` would conflate e.g. `dim1`
    /// with `dim10`).
    fn count_word_matches(haystack: &str, needle: &str) -> usize {
        let bytes = haystack.as_bytes();
        let mut count = 0;
        let mut start = 0;
        while let Some(pos) = haystack[start..].find(needle) {
            let idx = start + pos;
            let end = idx + needle.len();
            let before_ok = idx == 0 || !is_word_byte(bytes[idx - 1]);
            let after_ok = end == bytes.len() || !is_word_byte(bytes[end]);
            if before_ok && after_ok {
                count += 1;
            }
            start = idx + 1;
        }
        count
    }

    #[test]
    fn every_query_joins_exactly_its_declared_tables_and_sizes_grow() {
        for topology in [Topology::Chain, Topology::Star, Topology::Snowflake] {
            let mut rng = ChaCha8Rng::seed_from_u64(3);
            let schema = build_schema(topology, 14, &sizing(), &mut rng);
            let qs = generate_queries(&schema, "geqotest", 0.1);

            assert!(!qs.is_empty(), "{topology} produced no queries");

            let mut prev_n = 1;
            for q in &qs {
                assert!(q.num_tables > prev_n, "{}: sizes must strictly increase", q.id);
                prev_n = q.num_tables;

                let mentioned = schema
                    .tables
                    .iter()
                    .filter(|t| count_word_matches(&q.sql, &format!("geqotest.{}", t.name)) > 0)
                    .count();
                assert_eq!(mentioned, q.num_tables, "{}: table mention count mismatch\n{}", q.id, q.sql);
            }
        }
    }

    #[test]
    fn filter_selectivity_of_one_disables_filtering() {
        let mut rng = ChaCha8Rng::seed_from_u64(3);
        let schema = build_schema(Topology::Star, 4, &sizing(), &mut rng);
        let qs = generate_queries(&schema, "geqotest", 1.0);
        assert!(qs.iter().all(|q| !q.sql.contains("WHERE")), "selectivity 1.0 should add no filters");
    }

    #[test]
    fn filter_predicate_covers_every_non_fact_table_in_the_join() {
        let mut rng = ChaCha8Rng::seed_from_u64(3);
        let schema = build_schema(Topology::Star, 4, &sizing(), &mut rng);
        let qs = generate_queries(&schema, "geqotest", 0.1);
        let full = qs.last().unwrap();
        for dim in ["dim1", "dim2", "dim3"] {
            assert!(
                full.sql.contains(&format!("geqotest.{dim}.attribute_1")),
                "{}: missing filter on {dim}\n{}",
                full.id,
                full.sql
            );
        }
        assert!(!full.sql.contains("geqotest.fact.attribute_1"), "fact has no attribute_1 column");
    }
}

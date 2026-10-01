use crate::schema::{Schema, Table, Topology};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum JoinSyntax {
    /// `FROM a JOIN b ON ... JOIN c ON ...`. Postgres only reorders as many
    /// of these as `join_collapse_limit` allows (default 8) — beyond that it
    /// silently keeps part of the join tree fixed in the order written.
    Explicit,
    /// `FROM a, b, c WHERE a.x = b.y AND ...`. Parsed as an already-flat
    /// relation list with nothing to "collapse", so the whole join is
    /// exposed to the optimizer unconditionally; only geqo_threshold decides
    /// whether GEQO engages.
    Comma,
}

impl std::fmt::Display for JoinSyntax {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            JoinSyntax::Explicit => "explicit",
            JoinSyntax::Comma => "comma",
        };
        write!(f, "{s}")
    }
}

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
/// Each table's `WHERE table.attribute_1 <= ...` filter (if any) uses the
/// `filter_selectivity` already sampled onto it in `schema` (see
/// `schema::SizingParams`) — tables with no `attribute_1` column (fact
/// tables) get none. Giving every table the *same* selectivity makes total
/// plan cost close to order-invariant (every join shrinks the running row
/// count by the same proportion regardless of order); varying it per table
/// is what actually rewards a smart join order.
pub fn generate_queries(schema: &Schema, pg_schema: &str, join_syntax: JoinSyntax) -> Vec<Query> {
    match schema.topology {
        Topology::Chain => chain_queries(schema, pg_schema, join_syntax),
        Topology::Star => star_queries(schema, pg_schema, join_syntax),
        Topology::Snowflake => snowflake_queries(schema, pg_schema, join_syntax),
        Topology::Clique => clique_queries(schema, pg_schema, join_syntax),
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

/// Build `table.attribute_1 <= threshold` for every table in `tables` that
/// has a `filter_selectivity` below 1.0 (attribute_1 is generated uniformly
/// over [0.0, 1000.0), so the threshold passes roughly that fraction of a
/// table's rows). Tables with no filter_selectivity (fact tables) or one
/// at/above 1.0 (a no-op predicate) are skipped entirely.
fn attribute_filters(pg_schema: &str, tables: &[&Table]) -> Vec<String> {
    tables
        .iter()
        .filter_map(|t| t.filter_selectivity.filter(|s| *s < 1.0).map(|s| (t, s)))
        .map(|(t, s)| {
            let threshold = s.clamp(0.0, 1.0) * 1000.0;
            format!("{}.attribute_1 <= {threshold:.2}", qualify(pg_schema, &t.name))
        })
        .collect()
}

/// Build a `SELECT count(*)` join over `tables`, in either `JOIN ... ON`
/// syntax or a comma FROM-list with the same conditions moved into WHERE.
/// Each table after the first must share an FK edge (in either direction)
/// with at least one table already added — callers are responsible for
/// ordering `tables` so that holds. Every included table with a
/// `filter_selectivity` gets an `attribute_1 <= threshold` predicate sized
/// by its own value.
fn build_join_sql(pg_schema: &str, tables: &[&Table], join_syntax: JoinSyntax) -> String {
    let mut included: Vec<&Table> = vec![tables[0]];
    let mut join_conditions: Vec<String> = Vec::new();

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
        join_conditions.push(condition);
        included.push(t);
    }

    let filter_conditions = attribute_filters(pg_schema, &included);

    match join_syntax {
        JoinSyntax::Explicit => {
            let mut sql = format!("SELECT count(*) FROM {}", qualify(pg_schema, &tables[0].name));
            for (t, condition) in tables[1..].iter().zip(&join_conditions) {
                sql.push_str(&format!("\n  JOIN {} ON {}", qualify(pg_schema, &t.name), condition));
            }
            if !filter_conditions.is_empty() {
                sql.push_str("\n  WHERE ");
                sql.push_str(&filter_conditions.join(" AND "));
            }
            sql.push(';');
            sql
        }
        JoinSyntax::Comma => {
            let from_list: Vec<String> = tables.iter().map(|t| qualify(pg_schema, &t.name)).collect();
            let mut sql = format!("SELECT count(*) FROM {}", from_list.join(", "));
            let all_conditions: Vec<String> =
                join_conditions.iter().cloned().chain(filter_conditions.iter().cloned()).collect();
            if !all_conditions.is_empty() {
                sql.push_str("\n  WHERE ");
                sql.push_str(&all_conditions.join(" AND "));
            }
            sql.push(';');
            sql
        }
    }
}

fn chain_queries(schema: &Schema, pg_schema: &str, join_syntax: JoinSyntax) -> Vec<Query> {
    let n = schema.tables.len();
    (2..=n)
        .map(|k| {
            let tables: Vec<&Table> = schema.tables[0..k].iter().collect();
            Query {
                id: format!("chain_{k:02}"),
                label: format!("chain join of {k} tables"),
                num_tables: k,
                sql: build_join_sql(pg_schema, &tables, join_syntax),
            }
        })
        .collect()
}

fn star_queries(schema: &Schema, pg_schema: &str, join_syntax: JoinSyntax) -> Vec<Query> {
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
                sql: build_join_sql(pg_schema, &tables, join_syntax),
            }
        })
        .collect()
}

fn snowflake_queries(schema: &Schema, pg_schema: &str, join_syntax: JoinSyntax) -> Vec<Query> {
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
            sql: build_join_sql(pg_schema, &running, join_syntax),
        });
    }
    queries
}

/// Build a `SELECT count(*)` join with a predicate between *every* pair of
/// `tables` (C(n,2) edges total), not just enough for a spanning tree. This
/// is the hardest case for join-order search: unlike a sparse chain/star,
/// every subset of relations is connected, so the optimizer can't prune any
/// candidate subset by connectivity alone.
fn build_clique_sql(pg_schema: &str, tables: &[&Table], join_syntax: JoinSyntax) -> String {
    // per_table_conditions[j] holds the edges between tables[j] and every
    // tables[i] with i < j — grouped this way so explicit syntax can put
    // them all in that table's own ON clause.
    let mut per_table_conditions: Vec<Vec<String>> = vec![Vec::new(); tables.len()];
    for j in 1..tables.len() {
        for i in 0..j {
            let (a, b) = (tables[j], tables[i]);
            let condition = if let Some(fk) = a.fk_to(&b.name) {
                format!(
                    "{}.{} = {}.{}",
                    qualify(pg_schema, &a.name),
                    fk.column,
                    qualify(pg_schema, &b.name),
                    fk.ref_column
                )
            } else if let Some(fk) = b.fk_to(&a.name) {
                format!(
                    "{}.{} = {}.{}",
                    qualify(pg_schema, &b.name),
                    fk.column,
                    qualify(pg_schema, &a.name),
                    fk.ref_column
                )
            } else {
                panic!("clique schema missing edge between {} and {}", a.name, b.name);
            };
            per_table_conditions[j].push(condition);
        }
    }

    let filter_conditions = attribute_filters(pg_schema, tables);

    match join_syntax {
        JoinSyntax::Explicit => {
            let mut sql = format!("SELECT count(*) FROM {}", qualify(pg_schema, &tables[0].name));
            for (j, conditions) in per_table_conditions.iter().enumerate().skip(1) {
                sql.push_str(&format!(
                    "\n  JOIN {} ON {}",
                    qualify(pg_schema, &tables[j].name),
                    conditions.join(" AND ")
                ));
            }
            if !filter_conditions.is_empty() {
                sql.push_str("\n  WHERE ");
                sql.push_str(&filter_conditions.join(" AND "));
            }
            sql.push(';');
            sql
        }
        JoinSyntax::Comma => {
            let from_list: Vec<String> = tables.iter().map(|t| qualify(pg_schema, &t.name)).collect();
            let mut sql = format!("SELECT count(*) FROM {}", from_list.join(", "));
            let all_conditions: Vec<String> =
                per_table_conditions.into_iter().flatten().chain(filter_conditions).collect();
            if !all_conditions.is_empty() {
                sql.push_str("\n  WHERE ");
                sql.push_str(&all_conditions.join(" AND "));
            }
            sql.push(';');
            sql
        }
    }
}

fn clique_queries(schema: &Schema, pg_schema: &str, join_syntax: JoinSyntax) -> Vec<Query> {
    let n = schema.tables.len();
    (2..=n)
        .map(|k| {
            let tables: Vec<&Table> = schema.tables[0..k].iter().collect();
            Query {
                id: format!("clique_{k:02}"),
                label: format!("clique join of {k} tables ({} edges)", k * (k - 1) / 2),
                num_tables: k,
                sql: build_clique_sql(pg_schema, &tables, join_syntax),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{build_schema, SizingParams, Topology};
    use rand::SeedableRng;
    use rand_chacha::ChaCha8Rng;

    fn sizing() -> SizingParams {
        sizing_with(0.1, 0.1)
    }

    fn sizing_with(filter_selectivity_min: f64, filter_selectivity_max: f64) -> SizingParams {
        SizingParams {
            min_rows: 10,
            max_rows: 100,
            fact_multiplier: 5,
            filter_selectivity_min,
            filter_selectivity_max,
        }
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
        for topology in [Topology::Chain, Topology::Star, Topology::Snowflake, Topology::Clique] {
            for join_syntax in [JoinSyntax::Explicit, JoinSyntax::Comma] {
                let mut rng = ChaCha8Rng::seed_from_u64(3);
                let schema = build_schema(topology, 14, &sizing(), &mut rng);
                let qs = generate_queries(&schema, "geqotest", join_syntax);

                assert!(!qs.is_empty(), "{topology}/{join_syntax} produced no queries");

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
    }

    #[test]
    fn filter_selectivity_of_one_disables_filtering() {
        // Comma syntax always has a WHERE (it carries the join conditions),
        // so check for the absence of the attribute_1 predicate specifically
        // rather than the absence of WHERE itself.
        for join_syntax in [JoinSyntax::Explicit, JoinSyntax::Comma] {
            let mut rng = ChaCha8Rng::seed_from_u64(3);
            let schema = build_schema(Topology::Star, 4, &sizing_with(1.0, 1.0), &mut rng);
            let qs = generate_queries(&schema, "geqotest", join_syntax);
            assert!(
                qs.iter().all(|q| !q.sql.contains("attribute_1")),
                "selectivity 1.0 should add no filters ({join_syntax})"
            );
        }
    }

    #[test]
    fn filter_predicate_covers_every_non_fact_table_in_the_join() {
        let mut rng = ChaCha8Rng::seed_from_u64(3);
        let schema = build_schema(Topology::Star, 4, &sizing(), &mut rng);
        let qs = generate_queries(&schema, "geqotest", JoinSyntax::Explicit);
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

    #[test]
    fn comma_syntax_has_no_join_keyword_and_moves_conditions_into_where() {
        let mut rng = ChaCha8Rng::seed_from_u64(3);
        let schema = build_schema(Topology::Star, 4, &sizing(), &mut rng);
        let qs = generate_queries(&schema, "geqotest", JoinSyntax::Comma);
        let full = qs.last().unwrap();

        assert!(!full.sql.contains("JOIN"), "comma syntax should not use JOIN\n{}", full.sql);
        assert!(full.sql.contains("FROM geqotest.fact, geqotest.dim1"), "{}", full.sql);
        // join edges (fact.dimX_id = dimX.id) and the attribute_1 filters
        // should both end up in the single WHERE clause.
        assert!(full.sql.contains("geqotest.fact.dim1_id = geqotest.dim1.id"), "{}", full.sql);
        assert!(full.sql.contains("geqotest.dim1.attribute_1 <="), "{}", full.sql);
    }

    #[test]
    fn clique_query_has_every_pairwise_edge() {
        let n = 5;
        let mut rng = ChaCha8Rng::seed_from_u64(5);
        // selectivity 1.0 so the WHERE clause holds only join edges, making
        // the edge count easy to check directly.
        let schema = build_schema(Topology::Clique, n, &sizing_with(1.0, 1.0), &mut rng);
        let qs = generate_queries(&schema, "geqotest", JoinSyntax::Comma);
        let full = qs.last().unwrap();
        assert_eq!(full.num_tables, n);

        let where_clause = full.sql.split("WHERE").nth(1).expect("clique query must have a WHERE clause");
        let edge_count = where_clause.matches(" AND ").count() + 1;
        assert_eq!(edge_count, n * (n - 1) / 2, "clique of {n} tables should have C(n,2) edges\n{}", full.sql);
    }

    #[test]
    fn per_table_selectivity_variation_shows_up_as_different_thresholds() {
        let mut rng = ChaCha8Rng::seed_from_u64(9);
        let schema = build_schema(Topology::Star, 6, &sizing_with(0.01, 0.5), &mut rng);
        let qs = generate_queries(&schema, "geqotest", JoinSyntax::Comma);
        let full = qs.last().unwrap();

        // Extract each `<table>.attribute_1 <= N` threshold mentioned in the
        // query and confirm they're not all the same value.
        let thresholds: std::collections::BTreeSet<String> = full
            .sql
            .split("AND")
            .filter(|clause| clause.contains("attribute_1"))
            .map(|clause| clause.trim().rsplit("<=").next().unwrap().trim().to_string())
            .collect();
        assert!(
            thresholds.len() > 1,
            "expected varied per-table thresholds, got {thresholds:?}\n{}",
            full.sql
        );
    }
}

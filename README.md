# geqotest

Synthetic schema/data generator and benchmark harness for testing PostgreSQL's
join-order optimizer — specifically **GEQO**, the genetic algorithm Postgres
falls back to when a query joins more tables than `geqo_threshold` (default
**12**). Below that threshold, Postgres searches join orders exhaustively;
above it, GEQO searches heuristically instead. `geqotest` generates schemas
and queries sized around that boundary, loads them into a real Postgres
instance, and reports how GEQO's plans compare to exhaustive search.

## Requirements

- Rust (edition 2024 toolchain)
- A reachable PostgreSQL server (local or remote) you're allowed to create a
  schema in

## Build

```sh
cargo build --release
```

The binary is `target/release/geqotest` (or use `cargo run --` during
development).

## Quick start

```sh
export GEQOTEST_DSN="postgres://user@localhost/mydb"

# generate a 14-table star schema, load it, and benchmark GEQO vs exhaustive
# search for every query that actually exceeds the GEQO threshold
geqotest run --tables 14 --geqo-threshold 8 --repeat 5
```

This is equivalent to running `generate`, `load`, and `bench` back to back
(see below). Output lands in `./out` by default.

## Concepts

### Topologies (`--schema`)

- **`chain`** — `t1 <- t2 <- t3 <- ... <- tN`, a linear chain of FK
  relationships.
- **`star`** (default) — one `fact` table with an FK to each of N-1
  independent dimension tables (`dim1..dimN-1`).
- **`snowflake`** — a star schema where roughly half the dimensions are
  further normalized: `fact -> dim -> subdim`, a two-level hierarchy.

Every FK is child-to-parent-primary-key (an inner-join-only, 1:1 equi-join)
— see "Why filters matter" below for why this alone isn't enough to make
join order interesting.

### Table count vs. `geqo_threshold`

`--tables` (default 12) controls the schema size. Postgres' own
`geqo_threshold` defaults to 12, so a query joining more than 12 tables
engages GEQO on a stock server. `bench` only benchmarks queries that exceed
the *effective* threshold (the server's, or your `--geqo-threshold`
override) — pass `--include-below-threshold` to fall back to benchmarking
every generated query regardless of size.

### Why filters matter (`--filter-selectivity`)

Every join here is a lossless FK→PK join: joining a fact row to its
dimension row never changes the row count. With no `WHERE` clause, total
plan cost ends up close to **mathematically invariant to join order** —
there's nothing for GEQO's search to actually improve on, and you'll see
GEQO explore different join orders (different plans) that all cost the same.

`--filter-selectivity <0.0-1.0>` (default `0.1`) adds a
`WHERE table.attribute_1 <= threshold` predicate to every non-fact table,
sized to pass roughly that fraction of rows. This breaks the symmetry so
join order (and which selective table gets applied first) genuinely affects
cost. Pass `1.0` to disable filtering and reproduce the "everything ties"
behavior.

### Explicit `JOIN` vs. comma joins (`--join-syntax`)

- **`explicit`** (default) — `FROM a JOIN b ON ... JOIN c ON ...`. Postgres
  only reorders as many of these as `join_collapse_limit` allows (**default
  8**) — independent of `geqo_threshold`. With more than 8 explicitly joined
  tables, part of the join tree stays fixed in the order you wrote it unless
  you raise the limit (see `bench --join-collapse-limit`, which `bench`
  auto-raises to fit your largest query by default).
- **`comma`** — `FROM a, b, c WHERE a.x = b.y AND ...`. Parsed as an
  already-flat relation list with nothing to collapse, so the whole join is
  exposed to the optimizer unconditionally. Use this if you want
  `geqo_threshold` to be the *only* variable controlling whether GEQO
  engages, with no `join_collapse_limit` interaction to reason about.

### `geqo_seed` and repeats (`--repeat`, `--geqo-seed`)

GEQO's genetic search is randomized, but `geqo_seed` (a Postgres session
setting) defaults to a fixed value — so re-planning the same query in the
same session normally replays the *identical* search and gives you the
identical plan every time. `bench --repeat N` re-plans each qualifying query
N times, drawing a fresh random `geqo_seed` before each one, so you can
actually observe GEQO's run-to-run variability (join order, cost, planning
time). Pass `--geqo-seed <f64>` to pin a specific seed instead — useful for
reproducing one exact run.

## Commands

### `generate`

Builds the schema, synthesizes CSV data, and writes a ladder of join queries
of increasing size — all to `--out-dir` (default `out`), without touching a
database.

```sh
geqotest generate \
  --schema star \
  --tables 14 \
  --min-rows 5000 --max-rows 50000 \
  --fact-multiplier 20 \
  --skew 1.2 \
  --filter-selectivity 0.1 \
  --join-syntax explicit \
  --seed 42 \
  --pg-schema geqotest \
  --out-dir out
```

| Flag | Default | Meaning |
|---|---|---|
| `--tables` | `12` | Number of tables to generate |
| `--schema` | `star` | `chain` \| `star` \| `snowflake` |
| `--min-rows` / `--max-rows` | `1000` / `10000` | Row-count range for dimension/chain tables |
| `--fact-multiplier` | `20` | Fact rows = largest dimension's rows × this (star/snowflake only) |
| `--skew` | `1.2` | Zipfian exponent for FK reference skew (`0` = uniform) |
| `--filter-selectivity` | `0.1` | Fraction of rows passing the generated WHERE filter (`1.0` = off) |
| `--join-syntax` | `explicit` | `explicit` \| `comma` |
| `--seed` | random | RNG seed (printed every run, for reproducibility) |
| `--pg-schema` | `geqotest` | Postgres schema/namespace the tables will live under |
| `--out-dir` | `out` | Output directory |

### `load`

Creates the Postgres schema (from `schema.sql`) and bulk-loads each table's
CSV via `COPY`.

```sh
geqotest load --out-dir out --dsn "$GEQOTEST_DSN" --recreate
```

`--recreate` drops and recreates the target Postgres schema first — omit it
to load into an existing schema (e.g. to add data without touching DDL).

### `bench`

Runs `EXPLAIN` against the loaded database for every query that exceeds the
GEQO threshold, under both `geqo_off` (exhaustive search, the counterfactual)
and `geqo_on`, and reports the difference.

```sh
geqotest bench \
  --out-dir out --dsn "$GEQOTEST_DSN" \
  --geqo-threshold 8 \
  --repeat 5 \
  --analyze
```

| Flag | Default | Meaning |
|---|---|---|
| `--geqo-threshold` | server's current setting | Override for the `geqo_on` config; also decides which queries "invoke GEQO" |
| `--include-below-threshold` | off | Benchmark every generated query, not just ones above the threshold |
| `--repeat` | `1` | Re-plan each qualifying query this many times under GEQO with a fresh seed |
| `--geqo-seed` | random per repeat | Pin `geqo_seed` instead of randomizing it |
| `--join-collapse-limit` | auto (largest query's table count) | Session `join_collapse_limit`/`from_collapse_limit`; pass `8` to reproduce Postgres' default cap |
| `--analyze` | off | Use `EXPLAIN ANALYZE` (actually executes queries) instead of plan-only `EXPLAIN` |
| `--results-file` | `results.csv` | Output filename inside `out_dir` |

### `run`

`generate` + `load` + `bench` in one step. Takes the union of all their
flags (see `geqotest run --help`).

## Output files

Written to `--out-dir`:

| File | Contents |
|---|---|
| `schema.sql` | `CREATE SCHEMA`/`CREATE TABLE`/FK constraints/indexes |
| `tables/<name>.csv` | Generated data for each table |
| `queries.sql` | Human-readable ladder of generated join queries |
| `queries.json` | Same queries as structured data (id, sql, table count) |
| `manifest.json` | Full schema definition plus the seed/skew/filter/join-syntax/pg-schema used to generate it |
| `results.csv` | One row per (query, config, repeat): cost, planning time, execution time, plan shape |
| `comparison.csv` | One row per query: `geqo_off` vs. `geqo_on` (averaged across repeats) for cost, planning time, execution time |

`bench` also prints:
- The raw per-row results table, including a `plan` column (a canonicalized
  `NodeType(child,child,...)` string) so you can see the actual join
  order/method chosen, not just its cost.
- A **GEQO run-to-run variability** section (when `--repeat > 1`): how many
  distinct plans/costs a query produced across its repeats.
- A **GEQO vs exhaustive search comparison** section: cost/planning-time/
  execution-time deltas per query, with a `GEQO WORSE` / `GEQO BETTER` /
  `tie` verdict based on cost.

## Development

```sh
cargo test      # unit tests for schema/query generation invariants
cargo clippy    # lints
```

use crate::schema::{ColumnType, Schema, Table};
use chrono::{Duration, NaiveDate};
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use rand_distr::{Distribution, Zipf};
use std::collections::HashMap;
use std::fs::File;
use std::path::Path;

/// Generate CSV data for every table in `schema`, written to `<out_dir>/tables/<name>.csv`.
///
/// Primary keys are dense sequential integers `1..=row_count`, so a foreign
/// key can be sampled purely from the referenced table's row count without
/// materializing its data first (tables are generated in `schema.tables`
/// order, which is already dependency-ordered).
pub fn generate_data(schema: &Schema, skew: f64, seed: u64, out_dir: &Path) -> anyhow::Result<()> {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let row_counts: HashMap<&str, usize> =
        schema.tables.iter().map(|t| (t.name.as_str(), t.row_count)).collect();

    let tables_dir = out_dir.join("tables");
    std::fs::create_dir_all(&tables_dir)?;

    for table in &schema.tables {
        let path = tables_dir.join(format!("{}.csv", table.name));
        let file = File::create(&path)?;
        let mut writer = csv::Writer::from_writer(file);

        let headers: Vec<&str> = table.columns.iter().map(|c| c.name.as_str()).collect();
        writer.write_record(&headers)?;

        for i in 1..=table.row_count as u64 {
            let mut record = Vec::with_capacity(table.columns.len());
            for col in &table.columns {
                let value = if col.name == table.pk {
                    i.to_string()
                } else if let Some(fk) = table.foreign_keys.iter().find(|fk| fk.column == col.name) {
                    let n = *row_counts
                        .get(fk.ref_table.as_str())
                        .unwrap_or_else(|| panic!("referenced table {} not sized", fk.ref_table));
                    sample_fk(&mut rng, n, skew).to_string()
                } else {
                    random_value(&mut rng, col.ty, &col.name, table)
                };
                record.push(value);
            }
            writer.write_record(&record)?;
        }
        writer.flush()?;
    }
    Ok(())
}

/// Sample a referenced primary key in `1..=n`. `skew > 0` uses a Zipfian
/// distribution (larger skew = more traffic concentrated on low ids),
/// modeling the uneven selectivity real fact/child tables exhibit.
fn sample_fk(rng: &mut impl Rng, n: usize, skew: f64) -> u64 {
    if n <= 1 {
        return 1;
    }
    if skew > 0.0 {
        let zipf = Zipf::new(n as f64, skew).expect("valid zipf parameters");
        let rank: f64 = zipf.sample(rng);
        (rank as u64).clamp(1, n as u64)
    } else {
        rng.random_range(1..=n as u64)
    }
}

fn random_value(rng: &mut impl Rng, ty: ColumnType, name: &str, table: &Table) -> String {
    match name {
        "code" => format!("{}-{:06}", table.name.to_uppercase(), rng.random_range(0..1_000_000u32)),
        "name" => crate::words::random_name(rng),
        "quantity" => rng.random_range(1..=100).to_string(),
        "amount" => format!("{:.2}", rng.random_range(1.0..500.0)),
        "attribute_1" => format!("{:.2}", rng.random_range(0.0..1000.0)),
        "created_at" | "event_date" => random_date(rng).to_string(),
        _ => match ty {
            ColumnType::BigInt | ColumnType::Integer => rng.random_range(0..1000).to_string(),
            ColumnType::DoublePrecision | ColumnType::Numeric => {
                format!("{:.2}", rng.random_range(0.0..1000.0))
            }
            ColumnType::Text => crate::words::random_name(rng),
            ColumnType::Date => random_date(rng).to_string(),
        },
    }
}

fn random_date(rng: &mut impl Rng) -> NaiveDate {
    let base = NaiveDate::from_ymd_opt(2021, 1, 1).unwrap();
    let offset_days = rng.random_range(0..(365 * 5));
    base + Duration::days(offset_days)
}

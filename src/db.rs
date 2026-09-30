use postgres::{Client, NoTls};
use std::path::Path;

pub fn connect(dsn: &str) -> anyhow::Result<Client> {
    Ok(Client::connect(dsn, NoTls)?)
}

pub fn create_schema(client: &mut Client, pg_schema: &str, ddl: &str, recreate: bool) -> anyhow::Result<()> {
    if recreate {
        client.batch_execute(&format!("DROP SCHEMA IF EXISTS {pg_schema} CASCADE;"))?;
    }
    client.batch_execute(ddl)?;
    Ok(())
}

/// Bulk-load one table's CSV via `COPY ... FROM STDIN`. Returns rows copied.
pub fn load_table_csv(
    client: &mut Client,
    pg_schema: &str,
    table: &str,
    csv_path: &Path,
) -> anyhow::Result<u64> {
    let sql = format!("COPY {pg_schema}.{table} FROM STDIN WITH (FORMAT csv, HEADER true)");
    let mut writer = client.copy_in(&sql)?;
    let mut file = std::fs::File::open(csv_path)?;
    std::io::copy(&mut file, &mut writer)?;
    Ok(writer.finish()?)
}

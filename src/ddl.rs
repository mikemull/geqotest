use crate::schema::Schema;

/// Render CREATE SCHEMA / CREATE TABLE / FK / index statements for `schema`,
/// namespaced under the given Postgres schema name.
pub fn render_ddl(schema: &Schema, pg_schema: &str) -> String {
    let mut out = String::new();

    out.push_str(&format!("CREATE SCHEMA IF NOT EXISTS {pg_schema};\n\n"));

    for table in &schema.tables {
        out.push_str(&format!("CREATE TABLE {pg_schema}.{} (\n", table.name));
        let lines: Vec<String> = table
            .columns
            .iter()
            .map(|col| {
                let mut line = format!("    {} {}", col.name, col.ty.sql());
                if col.name == table.pk {
                    line.push_str(" PRIMARY KEY");
                }
                line
            })
            .collect();
        out.push_str(&lines.join(",\n"));
        out.push_str("\n);\n\n");
    }

    for table in &schema.tables {
        for fk in &table.foreign_keys {
            out.push_str(&format!(
                "ALTER TABLE {pg_schema}.{} ADD CONSTRAINT fk_{}_{} FOREIGN KEY ({}) REFERENCES {pg_schema}.{}({});\n",
                table.name, table.name, fk.column, fk.column, fk.ref_table, fk.ref_column
            ));
        }
    }
    out.push('\n');

    // Postgres does not auto-index FK columns; without these, every join
    // degrades to a sequential scan regardless of what the optimizer picks.
    for table in &schema.tables {
        for fk in &table.foreign_keys {
            out.push_str(&format!(
                "CREATE INDEX idx_{}_{} ON {pg_schema}.{}({});\n",
                table.name, fk.column, table.name, fk.column
            ));
        }
    }

    out
}

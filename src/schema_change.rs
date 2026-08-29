use anyhow::Result;
use tokio_postgres::Client;

use crate::queries;

pub struct SchemaChangePlan {
    pub table_name: String,
    pub alter_statement: String,
    pub batches: Vec<String>,
    pub estimated_duration_secs: u32,
}

pub async fn plan_column_type_change(
    client: &Client,
    table_name: &str,
    column_name: &str,
    new_type: &str,
) -> Result<SchemaChangePlan> {
    // Get all child partitions
    let query = format!(
        r#"
        SELECT c.relname
        FROM pg_inherits i
        JOIN pg_class p ON p.oid = i.inhparent
        JOIN pg_class c ON c.oid = i.inhrelid
        WHERE p.oid = {}::regclass
        ORDER BY c.relname
        "#,
        queries::quote_regclass_literal(table_name)
    );

    let rows = client.query(&query, &[]).await?;

    let mut batches = Vec::new();
    for row in rows {
        let child_name: String = row.get(0);
        let batch = format!(
            "ALTER TABLE {} ALTER COLUMN {} TYPE {}",
            queries::quote_ident(&child_name),
            queries::quote_ident(column_name),
            new_type
        );
        batches.push(batch);
    }

    let alter_statement = format!(
        "ALTER TABLE {} ALTER COLUMN {} TYPE {}",
        queries::quote_ident(table_name),
        queries::quote_ident(column_name),
        new_type
    );

    let batch_count = batches.len();
    let estimated_duration_secs = (batch_count * 5) as u32; // rough estimate

    Ok(SchemaChangePlan {
        table_name: table_name.to_string(),
        alter_statement,
        batches,
        estimated_duration_secs,
    })
}

pub async fn apply_schema_change_batch(
    client: &Client,
    batch_statement: &str,
) -> Result<()> {
    client.execute(batch_statement, &[]).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_schema_change_plan() {
        let plan = SchemaChangePlan {
            table_name: "events".to_string(),
            alter_statement: "ALTER TABLE events ALTER COLUMN created_at TYPE timestamptz".to_string(),
            batches: vec![],
            estimated_duration_secs: 30,
        };

        assert_eq!(plan.table_name, "events");
    }
}

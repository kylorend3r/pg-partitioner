use anyhow::Result;
use tokio_postgres::Client;

use crate::queries;
use crate::retry;
use crate::types::RetryPolicy;

#[derive(Debug, Clone)]
pub struct IndexDef {
    pub name: String,
    pub table_oid: u32,
    pub columns: Vec<String>,
    pub is_unique: bool,
    pub is_primary: bool,
    pub definition: String,
}

pub async fn get_indexes_for_table(
    client: &Client,
    schema: &str,
    table: &str,
) -> Result<Vec<IndexDef>> {
    let query = format!(
        r#"
        SELECT i.relname, i.oid::int, array_agg(a.attname ORDER BY a.attnum),
               ix.indisunique, ix.indisprimary, pg_get_indexdef(i.oid)
        FROM pg_class t
        JOIN pg_namespace n ON n.oid = t.relnamespace
        JOIN pg_index ix ON ix.indrelid = t.oid
        JOIN pg_class i ON i.oid = ix.indexrelid
        JOIN pg_attribute a ON a.attrelid = ix.indrelid
            AND a.attnum = ANY(ix.indkey)
        WHERE n.nspname = $1 AND t.relname = $2
            AND i.relkind = 'i'
        GROUP BY i.relname, i.oid, ix.indisunique, ix.indisprimary
        ORDER BY i.relname
        "#
    );

    let rows = client.query(&query, &[&schema, &table]).await?;

    let mut indexes = Vec::new();
    for row in rows {
        let index_def = IndexDef {
            name: row.get(0),
            table_oid: row.get::<_, i32>(1) as u32,
            columns: row.get(2),
            is_unique: row.get(3),
            is_primary: row.get(4),
            definition: row.get(5),
        };
        indexes.push(index_def);
    }

    Ok(indexes)
}

pub async fn create_index_on_parent(
    client: &Client,
    schema: &str,
    parent_table: &str,
    index_name: &str,
    columns: &[String],
    is_unique: bool,
    concurrently: bool,
    retry_policy: &RetryPolicy,
) -> Result<()> {
    let columns_str = columns.join(", ");
    let unique_keyword = if is_unique { "UNIQUE" } else { "" };
    let concurrently_keyword = if concurrently { "CONCURRENTLY" } else { "" };

    let query = format!(
        "CREATE {} INDEX {} {} ON {}.{} ({})",
        unique_keyword,
        concurrently_keyword,
        queries::quote_ident(index_name),
        queries::quote_ident(schema),
        queries::quote_ident(parent_table),
        columns_str
    );

    retry::execute_batch_with_retry(client, "create_index_on_parent", &query, retry_policy).await
}

pub async fn drop_index(
    client: &Client,
    schema: &str,
    index_name: &str,
    concurrently: bool,
) -> Result<()> {
    let concurrently_keyword = if concurrently { "CONCURRENTLY" } else { "" };

    let query = format!(
        "DROP INDEX {} {}.{}",
        concurrently_keyword,
        queries::quote_ident(schema),
        queries::quote_ident(index_name)
    );

    client.execute(&query, &[]).await?;

    Ok(())
}

pub async fn check_index_validity(
    client: &Client,
    schema: &str,
    index_name: &str,
) -> Result<bool> {
    let query = format!(
        r#"
        SELECT ix.indisvalid
        FROM pg_class i
        JOIN pg_namespace n ON n.oid = i.relnamespace
        JOIN pg_index ix ON ix.indexrelid = i.oid
        WHERE n.nspname = $1 AND i.relname = $2
        "#
    );

    let rows = client.query(&query, &[&schema, &index_name]).await?;

    if let Some(row) = rows.first() {
        Ok(row.get(0))
    } else {
        Ok(false)
    }
}

pub async fn get_invalid_indexes(
    client: &Client,
    schema: &str,
    parent_table: &str,
) -> Result<Vec<IndexDef>> {
    let query = format!(
        r#"
        SELECT i.relname, i.oid::int, array_agg(a.attname ORDER BY a.attnum),
               ix.indisunique, ix.indisprimary, pg_get_indexdef(i.oid)
        FROM pg_class p
        JOIN pg_namespace n ON n.oid = p.relnamespace
        JOIN pg_inherits inh ON inh.inhparent = p.oid
        JOIN pg_class c ON c.oid = inh.inhrelid
        JOIN pg_class i ON i.oid IN (
            SELECT indexrelid FROM pg_index
            WHERE indrelid = c.oid AND indisvalid = false
        )
        JOIN pg_index ix ON ix.indexrelid = i.oid
        JOIN pg_attribute a ON a.attrelid = ix.indrelid
            AND a.attnum = ANY(ix.indkey)
        WHERE n.nspname = $1 AND p.relname = $2
        GROUP BY i.relname, i.oid, ix.indisunique, ix.indisprimary
        "#
    );

    let rows = client.query(&query, &[&schema, &parent_table]).await?;

    let mut indexes = Vec::new();
    for row in rows {
        let index_def = IndexDef {
            name: row.get(0),
            table_oid: row.get::<_, i32>(1) as u32,
            columns: row.get(2),
            is_unique: row.get(3),
            is_primary: row.get(4),
            definition: row.get(5),
        };
        indexes.push(index_def);
    }

    Ok(indexes)
}

pub async fn reindex_concurrently(
    client: &Client,
    schema: &str,
    index_name: &str,
) -> Result<()> {
    let query = format!(
        "REINDEX INDEX CONCURRENTLY {}.{}",
        queries::quote_ident(schema),
        queries::quote_ident(index_name)
    );

    client.execute(&query, &[]).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_index_def_creation() {
        let index = IndexDef {
            name: "idx_test".to_string(),
            table_oid: 12345,
            columns: vec!["col1".to_string(), "col2".to_string()],
            is_unique: true,
            is_primary: false,
            definition: "CREATE UNIQUE INDEX idx_test ON table (col1, col2)".to_string(),
        };

        assert_eq!(index.name, "idx_test");
        assert!(index.is_unique);
        assert_eq!(index.columns.len(), 2);
    }
}

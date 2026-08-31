use anyhow::Result;
use tokio_postgres::Client;

use crate::queries;
use crate::types::{MigrationConfig, PartitionKey, PartitionStrategy, ValidationError};

pub async fn validate_table_for_partitioning(
    client: &Client,
    schema: &str,
    table: &str,
    config: &MigrationConfig,
) -> Result<Vec<ValidationError>> {
    let mut errors = Vec::new();

    // Verify table exists
    if !queries::verify_table_exists(client, schema, table).await? {
        errors.push(ValidationError {
            category: "table_not_found".to_string(),
            message: format!("Table {}.{} does not exist", schema, table),
            suggestion: None,
        });
        return Ok(errors);
    }

    // Check partition key columns exist
    errors.extend(
        validate_partition_key_columns(client, schema, table, &config.partition_key).await?,
    );

    // Check the key's shape is legal for the requested strategy
    errors.extend(validate_strategy_key_shape(config));

    // Check for naming collisions (63-byte identifier limit)
    errors.extend(validate_identifier_lengths(
        schema,
        table,
        &config.partition_key,
        &config.interval,
        config.partition_strategy,
    ));

    // Check for unique indexes without partition key
    errors.extend(validate_unique_constraints(client, schema, table, &config.partition_key).await?);

    // Check for timezone mismatches
    errors.extend(validate_timezone_compatibility(client, &config.partition_key).await?);

    Ok(errors)
}

async fn validate_partition_key_columns(
    client: &Client,
    schema: &str,
    table: &str,
    partition_key: &PartitionKey,
) -> Result<Vec<ValidationError>> {
    let mut errors = Vec::new();

    for column in &partition_key.columns {
        let query = format!(
            "SELECT 1 FROM information_schema.columns
             WHERE table_schema = $1 AND table_name = $2 AND column_name = $3"
        );

        let rows = client.query(&query, &[&schema, &table, column]).await?;

        if rows.is_empty() {
            errors.push(ValidationError {
                category: "column_not_found".to_string(),
                message: format!(
                    "Partition key column '{}' not found in {}.{}",
                    column, schema, table
                ),
                suggestion: Some("Check the column name and table structure".to_string()),
            });
        }
    }

    Ok(errors)
}

/// Constraints PostgreSQL puts on the *shape* of a partition key, per
/// strategy. Shared by the cutover and template-creation entry points, since
/// an illegal key is illegal either way.
fn validate_strategy_key_shape(config: &MigrationConfig) -> Vec<ValidationError> {
    let mut errors = Vec::new();

    // `PARTITION BY LIST` accepts exactly one column or expression — composite
    // list keys aren't merely unsupported here, they aren't valid syntax.
    // RANGE and HASH both accept multi-column keys.
    if config.partition_strategy == PartitionStrategy::List && config.partition_key.is_composite() {
        errors.push(ValidationError {
            category: "list_key_must_be_single_column".to_string(),
            message: format!(
                "List partitioning was requested with a composite partition key ({}); \
                 PostgreSQL's PARTITION BY LIST accepts only a single column",
                config.partition_key.columns.join(", ")
            ),
            suggestion: Some(
                "List partitioning requires exactly one partition key column".to_string(),
            ),
        });
    }

    errors
}

fn validate_identifier_lengths(
    schema: &str,
    table: &str,
    partition_key: &PartitionKey,
    interval: &str,
    strategy: PartitionStrategy,
) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    const IDENTIFIER_LIMIT: usize = 63;
    // Postgres's real limit is 63 bytes, but every derived partition/index
    // name adds a suffix on top of the table name — capping the table name
    // itself well below the hard limit leaves headroom for those suffixes
    // instead of relying solely on per-suffix math.
    const MAX_TABLE_NAME_LEN: usize = 60;

    if table.len() > MAX_TABLE_NAME_LEN {
        errors.push(ValidationError {
            category: "table_name_too_long".to_string(),
            message: format!(
                "Table name '{}' is {} characters; this tool caps table names at {} to leave \
                 headroom below PostgreSQL's 63-byte identifier limit for derived partition/index names",
                table,
                table.len(),
                MAX_TABLE_NAME_LEN
            ),
            suggestion: Some(format!("Use a table name of {} characters or fewer", MAX_TABLE_NAME_LEN)),
        });
    }

    // Range only: this composes the table name with the *interval*, which is
    // how range partition names used to be derived. For list and hash the
    // interval is an inert default the CLI never asked for, so charging its
    // length against the identifier budget rejects names that are perfectly
    // legal for those strategies -- and does it before the checks that would
    // have explained the real problem.
    let base_name = format!("{}_{}", table, interval);

    if strategy == PartitionStrategy::Range && base_name.len() > IDENTIFIER_LIMIT {
        errors.push(ValidationError {
            category: "identifier_too_long".to_string(),
            message: format!(
                "Base partition name '{}' exceeds PostgreSQL 63-byte limit (length: {})",
                base_name,
                base_name.len()
            ),
            suggestion: Some("Use shorter table or interval names".to_string()),
        });
    }

    for col in &partition_key.columns {
        let index_name = format!("{}_{}_{}_idx", schema, table, col);
        if index_name.len() > IDENTIFIER_LIMIT {
            errors.push(ValidationError {
                category: "index_name_too_long".to_string(),
                message: format!(
                    "Derived index name would be too long ({}). Schema: {}, Table: {}, Column: {}",
                    index_name.len(),
                    schema,
                    table,
                    col
                ),
                suggestion: Some("Use shorter names".to_string()),
            });
        }
    }

    // Date-range partition names (`{table}_YYYY_MM_DD_YYYY_MM_DD`) always add
    // exactly 22 bytes (6 underscores + 16 digits) regardless of the actual
    // dates involved, so this is a pure function of table.len() — no need to
    // compute real boundaries here. Only range partitioning derives names this
    // way: list children are named explicitly by the caller and hash buckets
    // take a much shorter `_p{n}` suffix, so charging either of them 22 bytes
    // of headroom would reject table names that are in fact perfectly legal.
    // Hash's own suffix is bounded by its modulus and checked separately, by
    // `validate_hash_partition_names`.
    const DATE_RANGE_SUFFIX_LEN: usize = 22;
    let worst_case_len = table.len() + DATE_RANGE_SUFFIX_LEN;
    if strategy == PartitionStrategy::Range && worst_case_len > IDENTIFIER_LIMIT {
        errors.push(ValidationError {
            category: "partition_name_too_long".to_string(),
            message: format!(
                "Date-range partition names for '{}' would exceed PostgreSQL's 63-byte limit \
                 (worst case: {} bytes, e.g. '{}_YYYY_MM_DD_YYYY_MM_DD')",
                table, worst_case_len, table
            ),
            suggestion: Some("Use a shorter table name".to_string()),
        });
    }

    errors
}

/// Types this tool accepts as a hash partition key, matching
/// `information_schema.columns.data_type`'s spelling (not the `int2`/`int4`/
/// `int8` internal aliases).
///
/// PostgreSQL itself is far more permissive — it hashes anything with a hash
/// operator class, `text` and `date` included. This narrower list is a
/// deliberate guardrail rather than a technical limit: hash partitioning
/// distributes by `hash(key) % modulus`, so a key with few distinct values or
/// a skewed distribution silently produces badly unbalanced buckets that only
/// show up as a performance problem much later. Integer and UUID keys are the
/// ones that reliably spread.
const HASH_KEY_TYPES: [&str; 4] = ["smallint", "integer", "bigint", "uuid"];

/// Checks that the widest hash bucket name still fits in an identifier.
///
/// Bucket names are `{table}_p{remainder}`, so the worst case is the largest
/// remainder, `modulus - 1`. This matters more than the few bytes suggest:
/// PostgreSQL **truncates** an over-long identifier silently rather than
/// erroring, so `events_p409` and `events_p4095` can collapse onto the same
/// name. The second `CREATE TABLE IF NOT EXISTS` then no-ops, the bucket set
/// comes up one remainder short, and the failure surfaces much later as rows
/// being rejected with no matching partition.
fn validate_hash_partition_names(table: &str, modulus: usize) -> Vec<ValidationError> {
    const IDENTIFIER_LIMIT: usize = 63;

    let widest = format!("{}_p{}", table, modulus.saturating_sub(1));
    if widest.len() <= IDENTIFIER_LIMIT {
        return Vec::new();
    }

    vec![ValidationError {
        category: "hash_partition_name_too_long".to_string(),
        message: format!(
            "Hash bucket names for '{}' at MODULUS {} would exceed PostgreSQL's 63-byte \
             identifier limit (widest: '{}', {} bytes). PostgreSQL truncates silently, which \
             would collapse two buckets onto one name and leave a remainder uncovered",
            table,
            modulus,
            widest,
            widest.len()
        ),
        suggestion: Some(format!(
            "Use a table name of {} characters or fewer at this modulus, or reduce \
             --hash-partitions",
            IDENTIFIER_LIMIT - (widest.len() - table.len())
        )),
    }]
}

/// Checks that every hash partition-key column is one of `HASH_KEY_TYPES`.
///
/// Evaluated against the **template**, since the target inherits its column
/// types verbatim through `LIKE`. This is the codebase's first column-*type*
/// lookup; everything before it only ever asked whether a column existed.
async fn validate_hash_key_types(
    client: &Client,
    schema: &str,
    table: &str,
    partition_key: &PartitionKey,
) -> Result<Vec<ValidationError>> {
    let mut errors = Vec::new();

    let query = r#"
        SELECT column_name, data_type
        FROM information_schema.columns
        WHERE table_schema = $1 AND table_name = $2 AND column_name = ANY($3)
    "#;

    let rows = client
        .query(query, &[&schema, &table, &partition_key.columns])
        .await?;

    for row in rows {
        let column_name: String = row.get(0);
        let data_type: String = row.get(1);

        if !HASH_KEY_TYPES.contains(&data_type.as_str()) {
            errors.push(ValidationError {
                category: "hash_key_type_unsupported".to_string(),
                message: format!(
                    "Hash partition key column '{}' is {}; hash partitioning here accepts {}",
                    column_name,
                    data_type,
                    HASH_KEY_TYPES.join(", ")
                ),
                suggestion: Some(format!(
                    "Use a {} column as the hash key, or partition BY RANGE or BY LIST instead",
                    HASH_KEY_TYPES.join(" / ")
                )),
            });
        }
    }

    Ok(errors)
}

async fn validate_unique_constraints(
    client: &Client,
    schema: &str,
    table: &str,
    partition_key: &PartitionKey,
) -> Result<Vec<ValidationError>> {
    let mut errors = Vec::new();

    let query = format!(
        r#"
        SELECT i.relname, array_agg(a.attname order by a.attnum) as columns
        FROM pg_class t
        JOIN pg_namespace n ON n.oid = t.relnamespace
        JOIN pg_index ix ON ix.indrelid = t.oid
        JOIN pg_class i ON i.oid = ix.indexrelid
        JOIN pg_attribute a ON a.attrelid = ix.indrelid
            AND a.attnum = ANY(ix.indkey)
        WHERE n.nspname = '{}' AND t.relname = '{}' AND ix.indisunique
        GROUP BY i.relname
        "#,
        schema, table
    );

    let rows = client.query(&query, &[]).await?;

    for row in rows {
        let index_name: String = row.get(0);
        let columns: Vec<String> = row.get::<_, Vec<String>>(1);

        let has_partition_key = partition_key.columns.iter().all(|pk_col| columns.contains(pk_col));

        if !has_partition_key {
            errors.push(ValidationError {
                category: "unique_index_missing_partition_key".to_string(),
                message: format!(
                    "Unique index '{}' does not include all partition key columns ({}). \
                     Unique constraints on partitioned tables must include the partition key.",
                    index_name,
                    partition_key.columns.join(", ")
                ),
                suggestion: Some("Add partition key columns to the index or recreate it".to_string()),
            });
        }
    }

    Ok(errors)
}

/// Pre-flight for the template-creation flow, the counterpart to
/// `validate_table_for_partitioning`.
///
/// Everything structural is checked against the **template**, since that's
/// where the target's columns, defaults, and indexes come from. Only the
/// identifier-length budget is checked against the target, because that's the
/// name the derived child/index identifiers will be built from.
///
/// The target's own existence is deliberately *not* checked here: "the target
/// already exists" isn't uniformly an error (an already-matching table is a
/// legitimate no-op), so that three-way decision lives in
/// `migration::classify_template_target` instead.
pub async fn validate_template_creation(
    client: &Client,
    template_schema: &str,
    template_table: &str,
    target_schema: &str,
    target_table: &str,
    config: &MigrationConfig,
) -> Result<Vec<ValidationError>> {
    let mut errors = Vec::new();

    if !queries::verify_table_exists(client, template_schema, template_table).await? {
        errors.push(ValidationError {
            category: "template_table_not_found".to_string(),
            message: format!(
                "Template table {}.{} does not exist",
                template_schema, template_table
            ),
            suggestion: Some(
                "The template must be an existing table whose columns the new partitioned \
                 table should copy; it is only read, never modified"
                    .to_string(),
            ),
        });
        return Ok(errors);
    }

    errors.extend(
        validate_partition_key_columns(
            client,
            template_schema,
            template_table,
            &config.partition_key,
        )
        .await?,
    );

    errors.extend(validate_strategy_key_shape(config));

    if config.partition_strategy == PartitionStrategy::Hash {
        errors.extend(
            validate_hash_key_types(client, template_schema, template_table, &config.partition_key)
                .await?,
        );
        if let Some(modulus) = config.hash_modulus {
            errors.extend(validate_hash_partition_names(target_table, modulus));
        }
    }

    errors.extend(validate_identifier_lengths(
        target_schema,
        target_table,
        &config.partition_key,
        &config.interval,
        config.partition_strategy,
    ));

    // Reported against the template for the same reason as the column checks:
    // whatever unique indexes it carries are what a later CreateIndex step
    // would try to recreate on the target. Downgraded to a warning by the
    // planner, exactly as in the cutover path.
    errors.extend(
        validate_unique_constraints(client, template_schema, template_table, &config.partition_key)
            .await?,
    );

    errors.extend(validate_timezone_compatibility(client, &config.partition_key).await?);

    Ok(errors)
}

/// Pre-flight for `add-partition`. The target's strategy is read from the live
/// catalog rather than taken on the caller's word, so pointing this at a
/// range- or hash-partitioned table (or an ordinary one) fails with a clear
/// message instead of producing DDL PostgreSQL will reject for a less obvious
/// reason.
pub async fn validate_add_list_partition(
    client: &Client,
    schema: &str,
    table: &str,
    partition_name: &str,
    values: &[String],
) -> Result<Vec<ValidationError>> {
    let mut errors = Vec::new();

    if values.is_empty() {
        errors.push(ValidationError {
            category: "list_values_required".to_string(),
            message: "No partition values were given".to_string(),
            suggestion: Some(
                "Pass at least one value, e.g. --values eu-west,eu-central".to_string(),
            ),
        });
    }

    match crate::schema::get_partition_strategy_and_key(client, schema, table).await? {
        Some((PartitionStrategy::List, _)) => {}
        Some((other, _)) => {
            errors.push(ValidationError {
                category: "target_not_list_partitioned".to_string(),
                message: format!(
                    "{}.{} is partitioned BY {}, not BY LIST",
                    schema,
                    table,
                    other.as_sql_keyword()
                ),
                suggestion: Some(
                    "add-partition adds FOR VALUES IN (…) children, which only list-partitioned \
                     tables accept"
                        .to_string(),
                ),
            });
            return Ok(errors);
        }
        None => {
            let exists = queries::verify_table_exists(client, schema, table).await?;
            errors.push(ValidationError {
                category: if exists {
                    "target_not_partitioned".to_string()
                } else {
                    "table_not_found".to_string()
                },
                message: if exists {
                    format!("{}.{} exists but is not a partitioned table", schema, table)
                } else {
                    format!("Table {}.{} does not exist", schema, table)
                },
                suggestion: Some(
                    "Create the list-partitioned parent first (plan --strategy list \
                     --template-table …, then apply)"
                        .to_string(),
                ),
            });
            return Ok(errors);
        }
    }

    // A name already in use is only a problem when it means something *else*.
    // Re-running add-partition with the same name and the same values is a
    // no-op by design; the same name with a different value set is a mistake
    // worth surfacing, which the DDL's own `IF NOT EXISTS` would otherwise
    // swallow silently.
    match crate::schema::get_partition_bound(client, schema, table, partition_name).await? {
        Some(bound) => {
            if !crate::migration::list_bound_matches(&bound, values) {
                errors.push(ValidationError {
                    category: "partition_bound_mismatch".to_string(),
                    message: format!(
                        "{}.{} is already a partition of {}.{} with bounds `{}`, which differ \
                         from the requested values ({})",
                        schema,
                        partition_name,
                        schema,
                        table,
                        bound,
                        values.join(", ")
                    ),
                    suggestion: Some(
                        "Choose a different partition name, or re-run with the values this \
                         partition already holds"
                            .to_string(),
                    ),
                });
            }
        }
        None => {
            if queries::verify_table_exists(client, schema, partition_name).await? {
                errors.push(ValidationError {
                    category: "partition_name_collision".to_string(),
                    message: format!(
                        "{}.{} already exists and is not a partition of {}.{}",
                        schema, partition_name, schema, table
                    ),
                    suggestion: Some("Choose a different --partition-name".to_string()),
                });
            }
        }
    }

    Ok(errors)
}

/// Placeholder. A real check would compare the server's `timezone` setting with
/// the client's and flag a `timestamp without time zone` partition key, which is
/// where the mismatch actually bites — a boundary computed in one zone and
/// evaluated in another lands rows in the neighbouring partition.
///
/// It previously read `current_setting('timezone')` and threw the answer away,
/// spending a round trip per `plan` to reach the same empty result. Kept wired
/// into both planning paths so the check has somewhere to land when written.
async fn validate_timezone_compatibility(
    _client: &Client,
    _partition_key: &PartitionKey,
) -> Result<Vec<ValidationError>> {
    Ok(Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::PartitionStrategy;

    fn config_with(strategy: PartitionStrategy, key: PartitionKey) -> MigrationConfig {
        MigrationConfig {
            source_table: "public.events".to_string(),
            partition_strategy: strategy,
            partition_key: key,
            interval: "1 month".to_string(),
            premake_count: 3,
            use_bulk_copy: false,
            retention_policy: None,
            start_date: None,
            template_table: None,
            list_partition_name: None,
            list_partition_values: None,
            hash_modulus: None,
        }
    }

    #[test]
    fn test_validate_identifier_lengths() {
        let errors = validate_identifier_lengths("public", "events", &PartitionKey::single("created_at".to_string()), "2026_07", PartitionStrategy::Range);
        assert!(errors.is_empty()); // Normal case should pass

        let long_table = "a".repeat(60);
        let errors = validate_identifier_lengths("public", &long_table, &PartitionKey::single("created_at".to_string()), "2026_07", PartitionStrategy::Range);
        assert!(!errors.is_empty()); // Long name should fail
    }

    #[test]
    fn test_validate_table_name_max_60_chars() {
        let at_limit = "a".repeat(60);
        let errors = validate_identifier_lengths("public", &at_limit, &PartitionKey::single("created_at".to_string()), "d", PartitionStrategy::Range);
        assert!(!errors.iter().any(|e| e.category == "table_name_too_long")); // exactly 60 is allowed

        let over_limit = "a".repeat(61);
        let errors = validate_identifier_lengths("public", &over_limit, &PartitionKey::single("created_at".to_string()), "d", PartitionStrategy::Range);
        assert!(errors.iter().any(|e| e.category == "table_name_too_long"));
    }

    #[test]
    fn test_validate_identifier_lengths_date_range_partition_name() {
        // Short enough to pass the existing interval-suffix check (45 + 1 + 7 = 53 <= 63)
        // but long enough to overflow the date-range partition name check (45 + 22 = 67 > 63).
        let table = "a".repeat(45);
        let errors = validate_identifier_lengths("public", &table, &PartitionKey::single("created_at".to_string()), "2026_07", PartitionStrategy::Range);
        assert!(errors.iter().any(|e| e.category == "partition_name_too_long"));
        assert!(!errors.iter().any(|e| e.category == "identifier_too_long"));
    }

    #[test]
    fn test_date_range_name_budget_is_range_only() {
        // The same 45-character name is fine for list: its children are named
        // explicitly by the caller, never as `{table}_YYYY_MM_DD_YYYY_MM_DD`.
        let table = "a".repeat(45);
        let errors = validate_identifier_lengths("public", &table, &PartitionKey::single("region".to_string()), "2026_07", PartitionStrategy::List);
        assert!(!errors.iter().any(|e| e.category == "partition_name_too_long"));
    }

    #[test]
    fn test_validate_hash_partition_names() {
        // Ordinary names at ordinary moduli are fine.
        assert!(validate_hash_partition_names("events", 8).is_empty());
        assert!(validate_hash_partition_names("events", 4096).is_empty());

        // 58 + "_p4095" = 64, one byte over — the case that would truncate.
        let table = "a".repeat(58);
        let errors = validate_hash_partition_names(&table, 4096);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].category, "hash_partition_name_too_long");

        // The same name is fine at a modulus whose widest remainder is shorter.
        assert!(validate_hash_partition_names(&table, 8).is_empty());

        // Exactly at the limit is allowed: 60 + "_p7" = 63.
        assert!(validate_hash_partition_names(&"a".repeat(60), 8).is_empty());
        // And one past it is not: 60 + "_p15" = 64.
        assert!(!validate_hash_partition_names(&"a".repeat(60), 16).is_empty());
    }

    #[test]
    fn test_list_key_must_be_single_column() {
        let composite = PartitionKey::new(vec!["region".to_string(), "tenant_id".to_string()]);

        let errors = validate_strategy_key_shape(&config_with(PartitionStrategy::List, composite.clone()));
        assert!(errors.iter().any(|e| e.category == "list_key_must_be_single_column"));

        // A single-column list key is fine...
        let errors = validate_strategy_key_shape(&config_with(
            PartitionStrategy::List,
            PartitionKey::single("region".to_string()),
        ));
        assert!(errors.is_empty());

        // ...and range/hash keep accepting composite keys.
        for strategy in [PartitionStrategy::Range, PartitionStrategy::Hash] {
            let errors = validate_strategy_key_shape(&config_with(strategy, composite.clone()));
            assert!(errors.is_empty());
        }
    }
}

use anyhow::Result;

use crate::types::{PartitionKey, PartitionStrategy};

pub struct RepartitionPlan {
    pub old_strategy: PartitionStrategy,
    pub old_key: PartitionKey,
    pub new_strategy: PartitionStrategy,
    pub new_key: PartitionKey,
    pub approach: RepartitionApproach,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum RepartitionApproach {
    /// Reuse existing cutover machinery (safest for most cases)
    CutoverAndBackfill,
    /// For large tables where a bulk import is acceptable
    BulkCopyAndRename,
}

// Note: no `&Client` parameter yet — this doesn't inspect the live table today
// (e.g. to validate the new key's columns actually exist); it only computes
// warnings from the strategy/key values passed in. Add one when that lands.
pub async fn plan_repartitioning(
    _table_name: &str,
    old_strategy: PartitionStrategy,
    old_key: PartitionKey,
    new_strategy: PartitionStrategy,
    new_key: PartitionKey,
) -> Result<RepartitionPlan> {
    let mut warnings = Vec::new();

    // Warn if changing to hash partitioning (Phase 3)
    if matches!(new_strategy, PartitionStrategy::Hash) {
        warnings.push(
            "Hash partitioning support is Phase 3; proceeding may require manual intervention"
                .to_string(),
        );
    }

    // Warn if partition key is changing to include fewer columns
    if old_key.columns.len() > new_key.columns.len() {
        warnings.push(
            "New partition key has fewer columns than the old one; data filtering may reduce row counts"
                .to_string(),
        );
    }

    Ok(RepartitionPlan {
        old_strategy,
        old_key,
        new_strategy,
        new_key,
        approach: RepartitionApproach::CutoverAndBackfill,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_repartition_plan_creation() {
        let old_key = PartitionKey::single("created_at".to_string());
        let new_key = PartitionKey::new(vec!["tenant_id".to_string(), "created_at".to_string()]);

        let plan = plan_repartitioning(
            "test_table",
            PartitionStrategy::Range,
            old_key,
            PartitionStrategy::Range,
            new_key,
        )
        .await;

        assert!(plan.is_ok());
    }
}

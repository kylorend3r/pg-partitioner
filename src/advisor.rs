use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::types::{PartitionKey, PartitionStrategy};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntervalRecommendation {
    pub recommended_interval: String,
    pub reasoning: String,
    pub confidence: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyRecommendation {
    pub strategy: PartitionStrategy,
    pub partition_key: PartitionKey,
    pub interval: Option<String>,
    pub reasoning: String,
    pub growth_rate_rows_per_day: i64,
}

pub struct PartitionAdvisor;

impl PartitionAdvisor {
    // Note: these don't yet query the database (`table_name`/`row_count`/etc.
    // are accepted for the Phase 4 query-pattern-aware version described in
    // roadmap.md but not implemented here) — they return a fixed, sensible
    // default. No `&Client` parameter: a signature that takes one but never
    // queries anything would misleadingly imply this does live analysis today.
    pub async fn recommend_strategy(
        _table_name: &str,
        _row_count: i64,
        _table_size_mb: i64,
    ) -> Result<StrategyRecommendation> {
        // Phase 4 feature: analyze query patterns and recommend strategy
        // For now, return a sensible default

        Ok(StrategyRecommendation {
            strategy: PartitionStrategy::Range,
            partition_key: PartitionKey::single("created_at".to_string()),
            interval: Some("1 month".to_string()),
            reasoning: "Time-series range partitioning is suitable for append-heavy workloads"
                .to_string(),
            growth_rate_rows_per_day: 10000,
        })
    }

    pub async fn recommend_interval(
        _table_name: &str,
        growth_rate_rows_per_day: i64,
        target_partition_size_mb: i64,
    ) -> Result<IntervalRecommendation> {
        // Estimate appropriate interval based on growth rate and target size
        let estimated_interval = if growth_rate_rows_per_day > 1_000_000 {
            "1 day".to_string()
        } else if growth_rate_rows_per_day > 100_000 {
            "1 week".to_string()
        } else if growth_rate_rows_per_day > 10_000 {
            "1 month".to_string()
        } else {
            "3 months".to_string()
        };

        Ok(IntervalRecommendation {
            recommended_interval: estimated_interval,
            reasoning: format!(
                "Based on growth rate of {} rows/day and target size of {} MB",
                growth_rate_rows_per_day, target_partition_size_mb
            ),
            confidence: 0.7,
        })
    }

    pub async fn estimate_storage_projection(
        _table_name: &str,
        growth_rate_rows_per_day: i64,
        current_size_mb: i64,
        projection_months: u32,
    ) -> Result<StorageProjection> {
        // Estimate future storage needs
        let bytes_per_row = if current_size_mb > 0 {
            (current_size_mb * 1024 * 1024) / 1 // Simplified
        } else {
            1000 // Default estimate
        };

        let rows_added = growth_rate_rows_per_day * 30 * projection_months as i64;
        let additional_mb = (rows_added * bytes_per_row) / (1024 * 1024);
        let projected_total_mb = current_size_mb + additional_mb;

        Ok(StorageProjection {
            current_size_mb,
            projected_size_mb: projected_total_mb,
            monthly_growth_mb: additional_mb / projection_months as i64,
            projection_months,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageProjection {
    pub current_size_mb: i64,
    pub projected_size_mb: i64,
    pub monthly_growth_mb: i64,
    pub projection_months: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_recommend_strategy() {
        let result =
            PartitionAdvisor::recommend_strategy("test_table", 1_000_000, 500).await;

        assert!(result.is_ok());
        let rec = result.unwrap();
        assert_eq!(rec.strategy, PartitionStrategy::Range);
    }

    #[tokio::test]
    async fn test_recommend_interval() {
        let result =
            PartitionAdvisor::recommend_interval("test_table", 100_000, 1000).await;

        assert!(result.is_ok());
        let rec = result.unwrap();
        assert!(!rec.recommended_interval.is_empty());
    }
}

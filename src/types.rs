use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PartitionStrategy {
    Range,
    List,
    Hash,
}

impl PartitionStrategy {
    /// String form used in `partitioner_registrations.strategy` — distinct from
    /// PostgreSQL's own single-letter `pg_partitioned_table.partstrat` codes
    /// (`r`/`l`/`h`) used when parsing the live catalog in `schema.rs`.
    pub fn as_registration_str(&self) -> &'static str {
        match self {
            PartitionStrategy::Range => "range",
            PartitionStrategy::List => "list",
            PartitionStrategy::Hash => "hash",
        }
    }

    pub fn from_registration_str(s: &str) -> Result<Self> {
        match s {
            "range" => Ok(PartitionStrategy::Range),
            "list" => Ok(PartitionStrategy::List),
            "hash" => Ok(PartitionStrategy::Hash),
            other => Err(anyhow!("Unknown partition strategy: {}", other)),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartitionKey {
    pub columns: Vec<String>,
}

impl PartitionKey {
    pub fn new(columns: Vec<String>) -> Self {
        Self { columns }
    }

    pub fn single(column: String) -> Self {
        Self {
            columns: vec![column],
        }
    }

    pub fn is_composite(&self) -> bool {
        self.columns.len() > 1
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartitionSetInfo {
    pub table_oid: u32,
    pub schema_name: String,
    pub table_name: String,
    pub full_name: String,
    pub strategy: PartitionStrategy,
    pub partition_key: PartitionKey,
    pub row_count: i64,
    pub size_bytes: i64,
    pub child_count: usize,
    pub active_child: Option<String>,
    pub default_partition: Option<String>,
    pub premake_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChildPartitionInfo {
    pub name: String,
    pub row_count: i64,
    pub size_bytes: i64,
    pub constraint_expr: Option<String>,
    pub is_default: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RiskSignal {
    UnpartitionedLargeTable {
        table_name: String,
        row_count: i64,
        size_bytes: i64,
    },
    StrayDefaultPartitionRows {
        table_name: String,
        row_count: i64,
    },
    PlannerRiskPartitionCount {
        table_name: String,
        child_count: usize,
    },
    NamingCollisionExposure {
        table_name: String,
        affected_identifiers: Vec<String>,
    },
    MaxLocksPerTransactionHeadroom {
        table_name: String,
        estimated_locks: usize,
    },
    TimezoneMismatch {
        description: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanAction {
    pub id: String,
    pub action_type: ActionType,
    pub table_name: String,
    pub description: String,
    pub estimated_duration_secs: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ActionType {
    CreatePartitionSet,
    MigrateToPartition,
    AttachPartition,
    DetachPartition,
    CreatePartition,
    DropPartition,
    EnforceRetention,
    ReconcileDefault,
    CreateIndex,
    DropIndex,
    RepairIndex,
    ValidateConstraint,
    AddConstraint,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionConfig {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    pub password: Option<String>,
    pub ssl_mode: SslMode,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum SslMode {
    Disable,
    Prefer,
    Require,
}

impl ConnectionConfig {
    pub fn connection_string(&self) -> String {
        let password_part = self
            .password
            .as_ref()
            .map(|p| format!(":{}@", p))
            .unwrap_or_else(|| "@".to_string());
        format!(
            "postgresql://{}{}{}:{}/{}",
            self.user, password_part, self.host, self.port, self.database
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub version: String,
    pub created_at: String,
    pub database: String,
    pub schema_checksum: String,
    pub actions: Vec<PlanAction>,
    /// The migration config this plan was computed from. Optional (and
    /// `#[serde(default)]`) so plan files written before this field existed
    /// still deserialize as `None` instead of failing.
    #[serde(default)]
    pub migration_config: Option<MigrationConfig>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReconciliationStatus {
    /// Registered and the live catalog matches the registration.
    Healthy,
    /// Registered but no matching partitioned table found in the live catalog.
    DriftMissing,
    /// Registered and live, but strategy or partition key columns differ.
    DriftMismatch,
    /// Found partitioned in the live catalog but not registered.
    Unmanaged,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconciliationEntry {
    pub schema_name: String,
    pub table_name: String,
    pub status: ReconciliationStatus,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconciliationSummary {
    pub entries: Vec<ReconciliationEntry>,
    pub note: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InspectReport {
    pub timestamp: String,
    pub database: String,
    pub tables: Vec<PartitionSetInfo>,
    pub risks: Vec<RiskSignal>,
    pub registrations: Vec<PartitionRegistration>,
    pub reconciliation: ReconciliationSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartitionRegistration {
    pub id: String,
    pub schema_name: String,
    pub table_name: String,
    pub strategy: PartitionStrategy,
    pub partition_key: PartitionKey,
    pub interval: String,
    pub premake_count: usize,
    pub retention_policy: Option<RetentionPolicy>,
    pub registered_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetentionPolicy {
    pub policy_type: RetentionType,
    pub value: i32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum RetentionType {
    Days,
    Months,
    Years,
    Count,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationConfig {
    pub source_table: String,
    pub partition_strategy: PartitionStrategy,
    pub partition_key: PartitionKey,
    pub interval: String,
    pub premake_count: usize,
    pub use_bulk_copy: bool,
    #[serde(default)]
    pub retention_policy: Option<RetentionPolicy>,
    /// Minimum date (`YYYY-MM-DD`) to start real per-period partitions from;
    /// data older than this lands in the one `MINVALUE`-bounded legacy
    /// partition instead. `#[serde(default)]` for the same backward-compat
    /// reason as `retention_policy` — protects deserializing plan JSON
    /// written before this field existed, even though the CLI requires it.
    #[serde(default)]
    pub start_date: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationError {
    pub category: String,
    pub message: String,
    pub suggestion: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryPolicy {
    pub lock_timeout_ms: u32,
    pub statement_timeout_ms: u32,
    pub max_attempts: u32,
    pub backoff_base_ms: u32,
    pub backoff_max_ms: u32,
    pub jitter: bool,
}

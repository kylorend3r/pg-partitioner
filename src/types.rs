use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PartitionStrategy {
    Range,
    List,
    Hash,
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
pub struct DoctorFinding {
    pub severity: FindingSeverity,
    pub category: String,
    pub table_name: String,
    pub message: String,
    pub remediation: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FindingSeverity {
    Info,
    Warning,
    Critical,
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
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InspectReport {
    pub timestamp: String,
    pub database: String,
    pub tables: Vec<PartitionSetInfo>,
    pub risks: Vec<RiskSignal>,
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

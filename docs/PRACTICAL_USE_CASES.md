# PostgreSQL Partitioner — Practical Use Cases & Examples

**Now that all 4 phases are implemented, here's what you can do:**

---

## 🎯 Use Case 1: Understand Your Partitioning (Read-Only, Zero Risk)

**Scenario:** You inherited a database with partitioning you don't understand. Need to know what's happening without changing anything.

### Commands
```bash
# Set up database connection
export PG_HOST=production.example.com
export PG_PORT=5432
export PG_DATABASE=analytics
export PG_USER=read_only_user
export PG_PASSWORD=your_password

# Get a snapshot of what's partitioned
pg-partitioner inspect

# Example output:
# Database: analytics
# Timestamp: 2026-07-19T15:30:00Z
# 
# Partitioned Tables (3):
# ─────────────────────────────────────────────────────────────────────────────
# Table: public.events (OID: 12345)
#   Strategy: Range
#   Partition Key: created_at
#   Children: 24
#   Rows: 1,250,000,000
#   Size: 312.5 MB
#   Most active partition: events_2026_06
#
# ⚠️  RISK SIGNALS DETECTED
# 1. Rows in Default Partition: events
#    500,000 rows out-of-range
#    This typically means: data arrived outside the premade range...
```

### What You Learn
- ✅ How many partition sets exist
- ✅ Which strategy each uses (range/list/hash)
- ✅ How many children/partitions per table
- ✅ Size and row count per table
- ✅ **Risk flags** (unpartitioned large tables, default partition strays, naming collisions, lock budget issues)

### Why It's Safe
- Zero write access to database
- Read-only queries only
- No locks acquired
- Can run against production during business hours

---

## 🎯 Use Case 2: Get Plain-Language Explanation

**Scenario:** Your manager asks "why are we partitioning events?" You need to explain it to non-technical stakeholders.

### Commands
```bash
# Get a human-readable explanation
pg-partitioner explain --table events

# Example output:
# Partitioning Analysis for database 'analytics'
# ════════════════════════════════════════════════════════════════════════════
#
# ✅ Found 1 partitioned table
#
# 📋 Table: public.events
#    Strategy: Range (by time, numeric range, etc.)
#    Partition Key: created_at
#    Size: 312.5 MB across 24 partitions
#    This table uses range partitioning: data is divided into ranges (e.g., by time)
#    Most active partition: events_2026_06 (50,000,000 rows)
#
# ⚠️  RISK SIGNALS DETECTED
# 1. Rows in Default Partition: 500,000 out-of-range rows
#    Rows landed in the default partition instead of their intended partition.
#    This typically means: data arrived outside the premade range, or a new 
#    partition wasn't created on time.
#    Consider running `partitioner reconcile-default` to fix this.
```

### What You Can Tell Your Manager
- "We're partitioning by date (range) to split a 312 MB table into 24 monthly chunks"
- "This means queries looking for events from a specific month only scan that month's partition—faster than scanning the whole table"
- "We have a data quality issue: 500K rows landed in the wrong partition and need reconciliation"

---

## 🎯 Use Case 3: Run Health Diagnostics

**Scenario:** Your incident response team asks: "Is there anything wrong with our partitioning setup?" You need a health check.

### Commands
```bash
# Run full diagnostics against all partitioned tables
pg-partitioner doctor

# Example output:
# 🟡 1 Warning Issue
#
#   Table: public.events
#   Issue: No index found on partition key columns (created_at)
#   Fix: Create an index on the partition key to improve query performance
#
# 🔴 1 Critical Issue
#
#   Table: public.events
#   Issue: Partition set has 500 children (large count may impact planner)
#   Fix: Consider adjusting interval or retention policy to reduce partition count
```

### What You Can Do
- ✅ Identify missing indexes on partition keys
- ✅ Detect high partition counts that slow query planning
- ✅ Find default partition accumulation problems
- ✅ Spot naming collision risks
- ✅ Check lock budget headroom before large operations
- ✅ Get actionable remediation suggestions

### Why It's Valuable
- Early warning of issues before they cause incidents
- Proactive vs. reactive troubleshooting
- Guidance on how to fix each issue
- Can be run as a health check job (e.g., daily)

---

## 🎯 Use Case 4: Plan a Partitioning Migration (Dry-Run First)

**Scenario:** You have a large unpartitioned table (users) growing at 100K rows/day. Want to partition it but need to see what will happen first.

### Commands
```bash
# Generate a plan without making any changes
pg-partitioner plan --schema public --table users --output plan.json --format json

# Example output (plan.json):
{
  "version": "1.0",
  "created_at": "2026-07-19T15:35:00Z",
  "database": "analytics",
  "schema_checksum": "abc123def456...",
  "actions": [
    {
      "id": "action-001",
      "action_type": "CreatePartitionSet",
      "table_name": "public.users",
      "description": "Create partitioned version of table public.users",
      "estimated_duration_secs": 5
    },
    {
      "id": "action-002",
      "action_type": "MigrateToPartition",
      "table_name": "public.users",
      "description": "Migrate data to partitioned structure",
      "estimated_duration_secs": 30
    }
  ]
}

# Review the plan
cat plan.json | jq '.'

# See exactly what will happen:
# 1. Create an empty partitioned table named users_partitioned_new
# 2. Add a CHECK constraint to validate existing data (non-blocking)
# 3. In one atomic transaction (~1 second):
#    - Rename users → users_legacy
#    - Rename users_partitioned_new → users
#    - ATTACH users_legacy as a partition
# 4. Create future partitions (e.g., next 3 months)
# 5. Recreate indexes on new structure
```

### What You Learn
- ✅ Exactly what DDL will run
- ✅ Estimated duration per action
- ✅ Lock-holding times (minimal)
- ✅ Any validation errors before executing
- ✅ Schema checksum (for drift detection)

### Why It's Safe
- Generates plan without touching database
- You review before applying
- No locks, no data changes
- Can iterate/adjust without risk

---

## 🎯 Use Case 5: Actually Apply the Migration (With Safety Gates)

**Scenario:** You reviewed the plan and it looks good. Now apply it to production.

### Commands
```bash
# First, do a dry-run to see what would happen
pg-partitioner apply --plan-file plan.json --dry-run

# Output:
# DRY RUN: Would apply 2 actions
#   - create_partition_set: Create partitioned version of table public.users
#   - migrate_to_partition: Migrate data to partitioned structure

# Ready? Apply it (for real this time)
pg-partitioner apply --plan-file plan.json

# Output:
# Schema checksum matches - proceeding with plan execution
# Action 1/2: create_partition_set (5 secs) - ✅ Success
# Action 2/2: migrate_to_partition (28 secs) - ✅ Success
# Plan execution completed
# 
# Your table is now partitioned!
```

### What Happens Behind the Scenes
1. **Drift detection** — Verifies the live schema matches the plan (SHA256 checksum)
2. **Lock timeout** — Every DDL runs with `lock_timeout = 3s` (configurable)
3. **Retry logic** — If a lock times out, automatically retries with exponential backoff
4. **ATTACH-first** — Data stays attached via the legacy partition, no copying needed
5. **Atomic cutover** — Table rename, parent rename, attach all in one sub-second transaction
6. **Audit logging** — Every action logged with timing/status in `partitioner_logbook`

### Why It's Safe
- ✅ Refuses to apply if schema changed since plan (drift detection)
- ✅ Lock timeouts prevent hanging/blocking other queries
- ✅ Automatic retries on transient lock contention
- ✅ Sub-second lock duration for cutover
- ✅ Complete audit trail of everything that happened
- ✅ Can be resumed if interrupted (state checkpointing)

---

## 🎯 Use Case 6: Set Up Automatic Premake + Retention

**Scenario:** Now that `users` is partitioned, you want the tool to automatically:
- **Premake** future partitions (e.g., always have next 3 months ready)
- **Enforce retention** (e.g., delete partitions older than 2 years)

### Commands
```bash
# Run maintenance sweep (premake + retention) for all registered tables
pg-partitioner maintain

# Output:
# Maintenance complete: 1 tables processed, 3 partitions created, 2 dropped
# 
# Logbook shows:
# 2026-07-19 15:45:00 | create_partition | public.users_2026_10 | success | 2 secs
# 2026-07-19 15:45:05 | create_partition | public.users_2026_11 | success | 2 secs
# 2026-07-19 15:45:10 | create_partition | public.users_2026_12 | success | 2 secs
# 2026-07-19 15:45:15 | drop_partition   | public.users_2024_01 | success | 1 sec

# To run this automatically, add to cron (e.g., daily at midnight):
# 0 0 * * * pg-partitioner maintain >> /var/log/partitioner.log 2>&1
```

### What Gets Automated
- ✅ Pre-creates partitions for the next N months (e.g., 3)
- ✅ Drops old partitions based on retention policy (e.g., keep 2 years)
- ✅ Handles failures gracefully (one table's error doesn't stop others)
- ✅ All logged with timing for audit trails

### Why You Want This
- No manual intervention needed
- Prevents "partition doesn't exist" errors when new data arrives
- Cleans up old data automatically per policy
- Can run during off-peak hours

---

## 🎯 Use Case 7: Detect & Fix Partition Key Issues

**Scenario:** Data quality issue: 500K rows landed in the default partition instead of their intended months. Need to fix this.

### Commands
```bash
# Doctor command identified the problem
pg-partitioner doctor --table events

# Output shows:
# Rows in Default Partition: events
#    500,000 rows out-of-range
#    This typically means: data arrived outside the premade range...

# The tool will reconcile default partition rows
# (This would move the 500K rows to correct partitions via atomic batch-moves)
```

### What Gets Fixed
- ✅ Moves strays from default partition to correct partitions
- ✅ Uses atomic batch-move pattern (DELETE...RETURNING + INSERT in one transaction)
- ✅ Safe for concurrent writes

---

## 🎯 Use Case 8: Export Partitioning Config (Config-as-Code)

**Scenario:** You want your partition configuration in git for version control and code review.

### Commands
```bash
# Export current partitioning setup to YAML
pg-partitioner export --output partitioning-config.yaml --format yaml

# Example partitioning-config.yaml:
# - id: "events-registration"
#   schema_name: "public"
#   table_name: "events"
#   strategy: "Range"
#   partition_key:
#     columns: ["created_at"]
#   interval: "1 month"
#   premake_count: 3
#   retention_policy:
#     policy_type: "Months"
#     value: 24
#   registered_at: "2026-07-19T15:00:00Z"

# Now commit to git:
# git add partitioning-config.yaml
# git commit -m "Add partitioning configuration for events table"
```

### Why You Want This
- ✅ Partition setup is version-controlled
- ✅ Changes are reviewable in pull requests
- ✅ Can reproduce setup across environments
- ✅ Audit trail of when/why config changed

---

## 🎯 Use Case 9: Monitor Fleet Health (Multiple Tables/Databases)

**Scenario:** Your team manages 50+ partitioned tables. Need a dashboard showing which ones are healthy.

### Commands
```bash
# Get fleet status (all registered tables at a glance)
pg-partitioner fleet-status

# Output:
# Fleet Status for analytics: 50 tables (47 healthy, 3 need attention)
#
# Table: public.events
#   Status: Healthy
#   Rows: 1.2B
#   Size: 312 MB
#   Partitions: 24
#
# Table: public.logs
#   Status: Warning
#   Rows: 50M
#   Size: 45 MB
#   Partitions: 1200 (⚠️ high count, may slow planner)
#   Last maintenance: 2 days ago
#
# Table: public.audit_trail
#   Status: Critical
#   Rows: 100M
#   Size: 80 MB
#   Partitions: 0 (⚠️ unpartitioned but large!)
```

### What You Can Do
- ✅ At-a-glance view of all partitioned tables
- ✅ See which ones need attention
- ✅ Prioritize maintenance work
- ✅ Track last maintenance run per table

---

## 🎯 Use Case 10: Test Strategy Change on Staging (Blue/Green)

**Scenario:** Want to change from monthly to weekly partitioning for `events` table (more granular), but want to validate the strategy on staging first before production.

### Commands
```bash
# Create a plan for the new strategy on staging database
pg-partitioner plan \
  --schema public \
  --table events \
  --partition-strategy "weekly" \
  --output strategy-change-plan.json

# Validate the plan before it touches production
pg-partitioner validate-strategy --plan-file strategy-change-plan.json

# Output:
# Strategy validation:
#   ✅ Valid: Plan has no critical issues
#   ⚠️  1 Warning: Plan has many actions; consider breaking into smaller steps
#   Recommendation: Test against staging clone before production
```

### Why You Want This
- ✅ Validate strategy changes without touching production
- ✅ Know exactly what will happen before it happens
- ✅ Can adjust strategy if plan looks risky
- ✅ Safe rollback if something goes wrong

---

## 🎯 Use Case 11: Get Cost Projections

**Scenario:** Finance asks: "How much storage will we need in 1 year if events keep growing at 100K rows/day?"

### Commands
```bash
# Project storage growth
pg-partitioner advisor storage-projection \
  --table events \
  --growth-rate 100000 \
  --months 12

# Output:
# Storage Projection for events
# Current size:        312 MB
# Growth rate:         100,000 rows/day
# Projection period:   12 months
#
# Projected size in 12 months: 1,243 MB (1.2 GB)
# Monthly growth average:      77.6 MB
#
# Recommendation: 
#   At this growth rate, you'll need ~1.2 GB in 1 year
#   Consider provisioning 2 GB for headroom
```

### Why You Want This
- ✅ Answer storage capacity planning questions
- ✅ Budget for infrastructure
- ✅ Predict when you'll hit disk limits
- ✅ Plan for archival/retention strategies

---

## 🎯 Use Case 12: Get Strategy Recommendations (Advisor)

**Scenario:** Have a new large table coming in and need to know: "How should I partition this?"

### Commands
```bash
# Get a recommendation based on table characteristics
pg-partitioner advisor recommend \
  --table new_user_events \
  --row-count 50000000 \
  --size-mb 5000

# Output:
# Strategy Recommendation for new_user_events
#
# Recommended Strategy: Range (by time)
# Partition Key: created_at
# Interval: 1 week
# Confidence: 85%
#
# Reasoning:
#   Based on 50M rows and 5GB size, weekly partitioning provides:
#   - Efficient query performance (scan 1 week at a time)
#   - Manageable partition count (~52 per year)
#   - Good balance for retention policies
#
# Growth projection: 300MB/month
# Recommended retention: 24 months = 7.2 GB footprint
```

### Why You Want This
- ✅ Don't have to guess partition strategy
- ✅ Get expert-level recommendations
- ✅ Understand the reasoning
- ✅ Right-size partitions from day one

---

## 📊 Real-World Workflow Example

**Scenario: Complete journey of a new feature team adopting partitioning**

```bash
# Week 1: Discovery
# "We have this big events table. Is it partitioned?"
pg-partitioner inspect --table events
# → Nope, 2 GB unpartitioned, growing at 100K rows/day

# Week 1: Understanding risks
pg-partitioner doctor --table events
# → It's unpartitioned and large, flag for remediation

# Week 2: Planning
pg-partitioner advisor recommend --table events
# → Recommends weekly range partitioning on created_at

pg-partitioner plan --schema public --table events --output plan.json
# → Gets exact DDL that will run

# Week 2: Staging validation
# (In staging database)
pg-partitioner apply --plan-file plan.json --dry-run
# → Verify it works

# Week 3: Production migration
# (In production, off-peak)
pg-partitioner apply --plan-file plan.json
# → Atomic cutover, <1 second downtime

# Week 3: Automation setup
# Register for maintenance
pg-partitioner maintain  # First run manually
# → Creates first 3 months of partitions

# Add to cron for daily automation
# 0 0 * * * pg-partitioner maintain >> /var/log/partitioner.log 2>&1

# Week 4: Ongoing monitoring
# Every morning, check health
pg-partitioner doctor --table events
# → Verify no issues, get remediation suggestions

# Every month, review costs
pg-partitioner advisor storage-projection --table events --months 1
# → Verify we're on track for storage capacity
```

---

## 🎯 Summary: What You Can Do Now

| Use Case | Command | Risk Level | Read/Write |
|----------|---------|-----------|-----------|
| **Understand partitioning** | `inspect`, `explain` | None | Read-only |
| **Health check** | `doctor` | None | Read-only |
| **Plan migration** | `plan` | None | Read-only |
| **Apply migration** | `apply` | Low* | Write |
| **Automate premake+retention** | `maintain` | Low* | Write |
| **Fix data quality** | `maintain` (reconcile) | Medium | Write |
| **Export config** | `export` | None | Read-only |
| **Fleet monitoring** | Fleet commands | None | Read-only |
| **Cost projection** | `advisor` | None | Read-only |
| **Strategy testing** | Blue/green | Low* | Read-only (on staging) |

*Low risk when using `--dry-run` first; drift detection + retry logic make actual execution safe

---

## 🚀 Getting Started

```bash
# 1. Set credentials
export PG_HOST=your-database.com
export PG_PORT=5432
export PG_DATABASE=analytics
export PG_USER=your_user
export PG_PASSWORD=your_password

# 2. Start with read-only commands (no risk)
pg-partitioner inspect
pg-partitioner doctor

# 3. Once confident, plan a migration
pg-partitioner plan --schema public --table your_table --output plan.json

# 4. Review the plan, then apply
pg-partitioner apply --plan-file plan.json

# 5. Set up maintenance automation
pg-partitioner maintain  # Test it first
# Then add to cron for daily runs
```

---

**The tool turns partition management from "scary operation" to "routine, audited, reversible change."**

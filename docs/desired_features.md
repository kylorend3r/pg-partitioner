# Desired Features — postgresql-partitioner CLI

*Design goal: close the operational-knowledge gap around partition management. Where pg_partman gives you correct automation, this tool should give you understanding, foresight, and control over that automation — as a standalone CLI, not an in-database extension.*

## 1. Discovery & explainability

- `partitioner inspect <table>` — connect to a live database and report, in plain language, whether a table is partitioned, how (range/list/hash, interval, column), how many children exist, their size and row counts, which one is currently receiving writes, and whether a default/catch-all partition has unexpected data in it.
- `partitioner explain <table>` — narrate the current partitioning setup and configuration back to the user (interval, retention, premake equivalent, constraints) so a new team member can understand an existing setup without reading DDL or extension config tables.
- Automatic detection of risk signals: tables that look like they should be partitioned but aren't (large, monotonically growing, no partitioning), partition counts approaching planner-performance concerns, or skewed partition sizes.

## 2. Plan-before-apply workflow

- `partitioner plan` — a Terraform-style dry run. Given a desired partitioning strategy (new or changed), print exactly what will happen: which partitions get created, which get dropped/detached, what locks will be taken and for how long, and any destructive or irreversible steps, before anything executes.
- `partitioner apply` — executes a previously generated plan, refusing to run if the live schema has drifted since the plan was generated (a checksum/version guard).
- Diffing of partitioning configuration over time, so a change to retention or interval settings is visible as a reviewable diff, not a silent behavior change on the next maintenance tick.
- Blue/green strategy testing: run a `plan` for a strategy change (e.g. monthly → weekly interval, or a full partition-key change) against a staging clone first, using the same plan mechanism that will later run against production, rather than validating a risky repartitioning decision for the first time on the real database.

## 3. Safe automation of the lifecycle

- Scheduling of partition premaking and retention enforcement, runnable either as an external process (cron/systemd timer/k8s CronJob) or embedded as a lightweight daemon — but always as an option, never a requirement, so it works against databases where the user has no extension-install rights.
- Built-in handling of the known footguns pg_partman documents but doesn't guard against: warn (not just document) about DST/timezone mismatches between client and server, warn before an operation that would exceed `max_locks_per_transaction`, detect table-name collisions before they occur under the 63-byte identifier truncation rather than after.
- Cutover-first migration of existing unpartitioned tables: attach the existing table as a partition immediately (near-instant, no data copy) rather than bulk-copying first and swapping at the end — the latter risks losing updates to existing rows during the copy window. Batched, resumable bulk-copy remains available as a fallback for the rare case direct attach isn't possible (see `migration_design.md`).
- A real, confirmable safety gate for destructive operations (subpartitioning, drops) — an interactive confirmation showing the actual blast radius (row counts, table names) rather than a string parameter that must equal `"yes"`.
- Partition-key strategy migration: safely move a table from one partitioning column/strategy to a completely different one (not just changing interval or retention on the existing key) by reusing the cutover-first-then-backfill machinery already designed for the initial plain-table conversion, rather than requiring a from-scratch teardown and rebuild.
- Safe schema-change batching: a column type change on a partitioned table still forces a per-partition table rewrite in PostgreSQL (unlike a plain `ADD COLUMN`). Batch and monitor those rewrites one partition at a time — through the same `retry.rs` lock/timeout/backoff path as everything else — instead of letting a single `ALTER TABLE` lock every child in sequence with no visibility into progress or a place to stop.

## 4. Observability out of the box

- Structured terminal/file logging (text and JSON) of every action taken — creation/drop events, duration, failures — without depending on a separate, loosely maintained monitoring extension.
- A `partitioner doctor` command that checks a live setup against known-bad patterns (missing indexes on partition key, orphaned children, mismatched constraints, an unmanaged default partition accumulating rows) and reports them with remediation suggestions.

## 5. Multi-strategy, multi-engine awareness

- Support range, list, and hash partitioning (pg_partman's list support is number-only and hash isn't supported at all today) — including combinations, and clear guidance on when subpartitioning is actually warranted versus when adjusting the interval is the better fix.
- Composite/multi-column partition keys from day one (e.g. range on `(tenant_id, created_at)`), not just the single-column case — modeled into the partition-key type from the start so it isn't a disruptive retrofit later once `plan`/`apply`/`wizard` already assume a single column.
- First-class support for global-uniqueness workarounds: detect and warn about primary-key/unique-constraint gaps across a partition set proactively (not via a separate, manually run Python script), and offer patterns (composite keys including the partition key, exclusion constraints) rather than just flagging the problem.
- Config portability: export/import a partitioning setup as a versionable file (YAML/JSON) that can live in a git repo and be code-reviewed, rather than only existing as rows in a database configuration table.

## 6. Operator experience

- Guided setup: an interactive wizard for first-time partitioning of a table that asks the right questions (expected growth rate, query patterns, retention needs) and recommends an interval/strategy instead of requiring the operator to already know the terminology.
- Undo/rollback for any operation the tool performs, mirroring pg_partman's reversibility bias but exposed as a first-class, discoverable command rather than a function an operator has to know to call.
- Works against any PostgreSQL 14+ target the user can connect to — self-hosted, RDS, Cloud SQL, Aurora, Crunchy Bridge — without requiring extension-install privileges, matching the constraint that many managed environments impose.

## 7. Pattern-specific recipes

- Multi-tenant "partition per tenant" recipe: a distinct, first-class guided flow for the common SaaS pattern — creating a tenant's partition on onboarding and archiving/dropping it on churn — driven by tenant lifecycle events rather than the time-series premake/retention model the rest of the tool is built around. Reuses the same `plan`/`apply`/`registrations` machinery, just triggered differently (on a tenant event, not a clock).

## Explicit non-goals (v1)

- Reimplementing PostgreSQL's own declarative partitioning mechanics — this tool orchestrates and advises, it doesn't replace core partitioning.
- Trigger-based partitioning (deprecated even by pg_partman itself) — target native declarative partitioning only, PostgreSQL 14+.
- Competing with pg_partman's raw maintenance-automation reliability as day one goal; matching it is table stakes, the differentiation is everything layered on top described above.

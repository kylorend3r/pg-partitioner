# Roadmap — postgresql-partitioner CLI

*Phased plan. Each phase should be independently useful and shippable; features carry over from `desired_features.md`, sequenced by what unblocks adoption fastest versus what needs a mature foundation first.*

## Phase 0 — Foundation (pre-release)

Goal: prove the core value prop (understanding > raw automation) against real databases before building anything automated.

- Connection/auth layer for PostgreSQL 14+ (self-hosted and major managed providers), read-only by default.
- `partitioner inspect` and `partitioner explain` — read-only discovery and plain-language reporting on existing partitioning setups (or lack thereof).
- Risk-signal detection: unpartitioned large/growing tables, planner-risk partition counts, default-partition data accumulation, naming-collision exposure.
- Config export to a versionable file format (YAML/JSON) as a read-only snapshot, no apply capability yet.

Exit criterion: a DBA can point the tool at an unfamiliar database and get an accurate, readable picture of its partitioning state in under a minute, with zero write access required.

## Phase 1 — MVP: guided setup + safe apply

Goal: become usable for greenfield partitioning decisions and simple ongoing maintenance, matching pg_partman's core automation for range/list partitioning.

- Interactive setup wizard recommending a strategy/interval based on table stats and stated growth expectations.
- `partitioner plan` / `partitioner apply` — dry-run-then-execute workflow for creating a new partition set, including drift detection between plan and apply.
- Premake + retention automation, runnable via external scheduler (cron/systemd timer/k8s CronJob) first — no embedded daemon required yet.
- Cutover-first conversion of existing unpartitioned tables via `ATTACH` (not blind bulk copy) — safe for tables with ongoing writes and updates to existing rows from the start; batched, resumable bulk-copy is a fallback for the rare case `ATTACH` can't apply directly (see `migration_design.md`).
- Index/constraint carryover during initial conversion — existing indexes on the source table are recreated on the new partitioned structure as part of the same plan (see `index_management.md`).
- Partition key modeled as one-or-more columns from the start (composite keys, e.g. `(tenant_id, created_at)`), even if the wizard defaults to guiding most users toward the single-column case — cheap to build in now, disruptive to retrofit later once `plan`/`apply`/`wizard` already assume one column.
- Real confirmation gates for destructive actions, showing actual blast radius (affected rows/tables), not a string flag.
- Basic structured logging of every action taken.

Exit criterion: someone can take an existing, unpartitioned production table and safely convert and maintain it end-to-end using only this tool, without touching SQL functions directly.

## Phase 2 — Operational parity + guardrails

Goal: close the specific footgun gaps identified in the pg_partman analysis — this is where "operational knowledge" becomes built into the tool rather than tribal knowledge.

- Proactive warnings: timezone/client-server mismatches, `max_locks_per_transaction` exhaustion risk, identifier-truncation collisions, before they cause an incident.
- `lock_timeout`/`statement_timeout`/`deadlock_timeout` on every lock-acquiring action, with automatic backoff-retry on transient lock contention and deadlocks (see `locking_and_retries.md`) — pg_partman leaves this entirely to the operator's own maintenance-window discipline.
- `partitioner doctor` — health-check command against known-bad patterns (missing partition-key indexes, orphaned children, constraint drift, unmanaged default partitions) with remediation suggestions.
- Global-uniqueness advisory: proactive detection of PK/unique-constraint gaps across a partition set, with suggested patterns (composite keys, exclusion constraints) instead of a separate manual script.
- Ad-hoc index lifecycle commands (`index create/drop/status/repair`) — concurrent build across existing and future partitions, invalid-child detection, targeted repair (see `index_management.md`).
- Undo/rollback exposed as a first-class command for every mutating operation the tool performs.
- Config diffing: changes to an exported partitioning config are reviewable (e.g., in a PR) before being applied.
- Partition-key strategy migration — moving a table from one partitioning column/strategy to a different one entirely, reusing the cutover-then-backfill machinery from Phase 1's initial-conversion path rather than a separate mechanism.
- Safe schema-change batching for partitioned tables — column type changes (which force a per-partition rewrite) batched and monitored one partition at a time through the same `retry.rs` path as every other action.
- Multi-tenant "partition per tenant" recipe — tenant-lifecycle-triggered partition creation/archival, layered on the same `registrations`/`plan`/`apply` machinery as the time-series case.

Exit criterion: the tool actively prevents at least the top three incident classes documented against pg_partman (lock exhaustion, timezone-driven boundary errors, silent naming collisions) rather than merely documenting them.

## Phase 3 — Fleet management

Goal: move from "single table, single operator" to "many tables, many databases, ongoing operations."

- Fleet view: manage partitioning config and health across multiple tables/databases from one place, with a lightweight optional daemon mode for environments where an embedded scheduler is preferable to external cron.
- Hash partitioning support and subpartitioning guidance (when it's actually warranted vs. an interval change).

Exit criterion: a team responsible for dozens of partitioned tables across multiple databases can monitor and operate them from this tool as their primary interface, not psql.

## Phase 4 — Differentiation & ecosystem

Goal: features that go beyond parity with pg_partman entirely.

- Query-pattern-aware interval recommendations (analyze actual WHERE-clause usage to suggest partitioning column/interval, not just growth rate).
- CI/CD integration: partitioning config-as-code in a repo, applied via pipeline with the same plan/apply safety model as infra-as-code tools.
- Migration assistant for teams currently on pg_partman or trigger-based partitioning, importing existing config rather than starting over.
- Cost/storage projection: forecast partition growth and storage cost trajectory based on historical patterns.
- Blue/green strategy testing — run a `plan` for a strategy change against a staging clone using the same mechanism that will later apply it to production, before ever touching the real database with an untested repartitioning decision.

## Sequencing rationale

Phases 0–1 intentionally prioritize read-only insight and guided setup over automation depth, because the stated pain point is a knowledge gap, not a lack of scripts — a tool that explains and safely guides before it automates directly addresses that gap and differentiates from day one, whereas racing to match pg_partman's decade of automation maturity first would delay any visible advantage. Phases 2–3 close the specific, named operational gaps identified in the competitive analysis. Phase 4 is where the tool stops being "pg_partman with a CLI" and becomes something with independent value.

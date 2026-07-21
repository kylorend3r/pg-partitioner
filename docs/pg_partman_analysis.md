# pg_partman: Strengths & Weaknesses Analysis

*Competitive analysis for the postgresql-partitioner CLI project. Source: pg_partman README, reference documentation (`doc/pg_partman.md`), and known operational reports from AWS RDS, Crunchy Data, and community discussions (as of mid-2026, versions 5.1–5.2.x).*

## What pg_partman is

pg_partman is a PostgreSQL extension (PL/pgSQL functions + optional background worker) that automates the creation, maintenance, and retirement of declaratively partitioned tables. It does not implement its own partitioning mechanism — since 5.0 it wraps PostgreSQL's built-in declarative partitioning and adds the lifecycle automation Postgres itself doesn't provide: premaking future partitions, dropping old ones on a retention policy, migrating existing tables into partition sets, and applying constraint-exclusion optimizations to old, static children.

Its core building blocks: `create_parent()` / `create_sub_parent()` to stand up a partition set, `run_maintenance()` (or the BGW) to keep it topped up and enforce retention, `partition_data_*()` to backfill or migrate existing data in batches, `undo_partition()` to reverse a partitioning decision, and a `part_config` table that holds all of this as declarative state per partition set.

## Strengths

**Deep, battle-tested automation of the boring parts.** Premake/retention/constraint-exclusion logic has been refined over roughly a decade of production use across many companies (it ships in AWS RDS and Crunchy Data by default). The edge cases around leap years, DST, month-length variance, and century boundaries are already handled.

**No external scheduler required.** The background worker (`pg_partman_bgw`) runs `run_maintenance()` on an interval from inside Postgres itself, so a cron job or Kubernetes CronJob isn't strictly necessary for basic operation.

**Batched, contention-aware data migration.** `partition_data_proc()` / `undo_partition_proc()` move data in commit-sized batches with configurable lock-wait and inter-batch pauses, which is exactly the kind of detail an engineer would get wrong on a first attempt at writing a one-off backfill script.

**Reversible by design.** `undo_partition()` can convert a partition set back into a single table without data loss, and by default it detaches rather than drops old children — a deliberate bias toward not destroying data.

**Configuration as data, not code.** All partitioning behavior for a table lives in the `part_config` / `part_config_sub` rows, which makes it introspectable via SQL and (in principle) diffable/auditable, though pg_partman itself doesn't provide tooling for that.

**Free, mature, permissively licensed, and widely packaged** (PGDG, RDS, most managed Postgres providers), so adoption has effectively zero procurement friction.

## Weaknesses

**No standalone CLI or UI — SQL is the only interface.** Every operation is a `SELECT`/`CALL` against extension functions. There is no `partman status`, no dry-run preview, no diffing of "what will change if I alter this config row." A DBA has to already know the function signatures and query `part_config` by hand to understand current state. This is the exact operational-knowledge gap the new tool should target.

**In-database extension, not portable tooling.** It requires installing an extension (and, for the BGW, a restart to add it to `shared_preload_libraries`), which is a nonstarter on some managed Postgres offerings and adds friction on locked-down production systems. There's no way to run it "from outside" against a database you don't have extension-install rights on.

**Weak observability and alerting story.** Health signals (is retention running, did maintenance fail, is data landing in the default partition) exist as functions you must poll (`check_default()`) or as optional integration with `pg_jobmon`, a separate, less-maintained extension. There's no built-in metrics endpoint, structured logging, or webhook/alert integration — you're expected to wire your own monitoring (Nagios/Circonus mentioned in the docs, both dated choices).

**No global uniqueness across partitions.** This is a PostgreSQL limitation, not pg_partman's fault, but pg_partman doesn't paper over it either — it ships a Python script (`check_unique_constraint.py`) that only monitors for violations after the fact rather than preventing them.

**Locking and timing footguns remain the user's problem.** `create_parent()` takes an ACCESS EXCLUSIVE lock; subpartitioning can crash a cluster via `max_locks_per_transaction` exhaustion if you don't know to raise it; time-zone mismatches between the DB and the calling client silently produce wrong partition boundaries. All of this is documented, but nothing in the tool itself warns you before you hit it — the knowledge has to already be in the operator's head.

**Naming collisions under long table names are silent and structural.** The 63-byte identifier truncation means two independently-reasonable table names can collide once suffixed, and pg_partman's docs simply recommend "keep names short" rather than detecting or preventing the collision.

**Destructive operations are gated by a string flag, not a real safety mechanism.** `create_sub_parent()`'s "confirm you understand this is destructive" mechanism is a parameter that must literally equal `'yes'` — a marginal guard against a very costly mistake (subpartitioning drops and recreates every child table).

**No plan/diff workflow.** Changing `part_config` (e.g., interval, retention, template table) takes effect silently on the next maintenance run. There's no equivalent of `terraform plan` — no way to see "here's exactly what will be created/dropped the next time maintenance runs" before it happens.

**Steep initial learning curve for a deceptively simple-looking tool.** The docs even say so directly ("much easier to use than it may first appear"), which is itself a signal: the mental model (template tables, default tables, constraint_cols, optimize_constraint, premake, subpartitioning quirks) takes real ramp-up time, and there's no guided onboarding, wizard, or validation layer to shorten that curve.

**Documentation is comprehensive but reference-style, not task-oriented.** It's a ~900-line function reference plus several how-to docs; there's no single-command "explain my current partitioning setup and flag anything risky" you can point a new team member at.

## Implication for this project

pg_partman has already solved the hard, tedious mechanics of partition lifecycle automation extremely well, and there is little value in re-deriving that logic. The genuinely open opportunity — and the one matching the stated pain point of "lack of operational knowledge" — is the operator experience layer that pg_partman deliberately doesn't provide: a real CLI/TUI, dry-run/plan-before-apply workflows, proactive guardrails against known footguns (locks, time zones, naming collisions, subpartition destructiveness), built-in observability, and guided/explainable setup that shortens the ramp-up curve instead of just documenting it. See `desired_features.md` and `roadmap.md`.

## Advantages of pg-partitioner over pg_partman

Concrete, checkable differences rather than general positioning — each one is something pg_partman's own documentation admits it doesn't do, not a subjective preference:

- **A real CLI instead of SQL-only.** `inspect`/`explain`/`doctor` give a readable report on a live database's partitioning state without already knowing `part_config`'s columns or which function to call — pg_partman has no equivalent entry point at all.
- **Plan before apply, with drift detection.** Every change goes through `plan` → `apply`, showing exactly what will be created/dropped/migrated and re-checksumming the live schema before executing. pg_partman changes take effect silently on the next scheduled `run_maintenance()` — there's no preview step, ever.
- **Real destructive-action confirmation.** Blast radius (actual row counts, table names) shown before a destructive step runs, instead of a parameter that just has to equal the string `"yes"` (`create_sub_parent()`'s `p_declarative_check`).
- **Built-in lock/timeout/retry discipline.** `lock_timeout`/`statement_timeout`/`deadlock_timeout` on every lock-acquiring action with automatic backoff-retry (see `locking_and_retries.md`) — pg_partman's docs simply tell the operator to "plan maintenance windows accordingly" and leave lock handling entirely to them.
- **Proactive guardrails, not documented warnings.** Timezone/client-server mismatches, `max_locks_per_transaction` exhaustion, and identifier-truncation collisions are checked and surfaced *before* they cause an incident, rather than being a paragraph in the README the operator has to already remember.
- **Default-partition lifecycle, not just monitoring.** `check_default()` in pg_partman tells you rows landed somewhere they shouldn't; this tool additionally reconciles them (creates the missing partition, moves the rows) and warns about the ACCESS-EXCLUSIVE-scan cost a growing default imposes on every future partition attach.
- **No separate, loosely maintained monitoring extension required.** Structured terminal/file logging (text and JSON) is native from Phase 1, instead of depending on `pg_jobmon`, an optional, separately-installed extension pg_partman defers to.
- **Standalone, not an in-database extension.** Runs against any PostgreSQL 14+ target the user can connect to — no `CREATE EXTENSION`, no `shared_preload_libraries` restart, works on managed providers that don't grant extension-install rights.
- **Config as reviewable code.** Partitioning setup exports to YAML/JSON that can live in a git repo and be code-reviewed, rather than existing only as rows in an in-database configuration table.
- **Guided setup for the initial decision.** An interactive wizard recommends a strategy/interval from actual table stats and growth expectations — pg_partman's own docs describe the tool as "easier to use than it first appears," which is itself an admission that the initial mental model is non-obvious and unaided.

The honest counterpoint, already noted above: pg_partman has roughly a decade of production hardening behind its core automation, and matching that reliability bar for the mechanics themselves (not the experience layer) is real work this project hasn't done yet — these advantages are about what happens around the automation, not a claim of superior automation on day one.

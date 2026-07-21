# pg-partitioner

> Operational knowledge for PostgreSQL declarative partitioning — plan before you apply, guardrails against the footguns pg_partman documents but doesn't prevent, and a real CLI instead of SQL function calls.

pg-partitioner is a standalone Rust CLI for planning, applying, and operating PostgreSQL 14+ declarative partitioning. It doesn't reimplement partitioning mechanics — PostgreSQL already does that well, and pg_partman already automates the lifecycle around it well. What it adds is the layer both of those leave to the operator: a `plan`-before-`apply` workflow with drift detection, proactive warnings for the specific incidents that take down partitioned tables in production, and a CLI that explains what's actually going on instead of requiring you to already know `part_config`'s column names.

## Why this exists

[`pg_partman`](https://github.com/pgpartman/pg_partman) is mature, battle-tested, and already solves the hard mechanical problem — premake, retention, constraint-exclusion, batched backfill. There's no value in re-deriving that. But pg_partman is SQL-only: every operation is a `SELECT`/`CALL` against extension functions, there's no dry-run, no diff of what a config change will actually do on the next maintenance tick, and the operational footguns (lock exhaustion, timezone-driven boundary errors, 63-byte identifier truncation collisions) are documented in the README rather than caught by the tool. It also requires `CREATE EXTENSION` privileges, which many managed providers won't grant.

pg-partitioner targets the gap: the same category of PostgreSQL-native partitioning work, but with a plan/apply workflow, real safety gates, and guardrails that catch the known failure modes before they cause an incident — not after.

## Who this is for

- **DBAs and platform engineers** operating PostgreSQL 14+ who need to partition existing, populated, actively-written tables without a maintenance window measured in hours.
- **Teams on managed Postgres** (RDS, Cloud SQL, Aurora, Crunchy Bridge) without `CREATE EXTENSION` rights, where pg_partman isn't an option at all.
- **Anyone who inherited a partitioned database** and needs to understand its current state — strategy, interval, retention, risk signals — without already knowing the extension's internal tables.
- **Teams that want partitioning config reviewable in a pull request**, not just rows in a database table that change silently on the next scheduled run.

Not a fit if you're already deep on pg_partman with a stable setup and don't need a CLI, dry-run, or standalone operation — pg_partman's core automation is not something this project claims to beat.

## What it does differently from pg_partman

| | pg_partman | pg-partitioner |
|---|---|---|
| Interface | SQL functions only | CLI (`inspect`, `plan`, `apply`, `doctor`, ...) |
| Preview before changing anything | No — config changes take effect on the next scheduled run | `plan` shows exactly what will run, `apply` refuses to proceed if the schema drifted since |
| Destructive-action confirmation | A parameter that must equal the string `"yes"` | Confirmation showing actual row counts and table names |
| Lock/timeout discipline | Left to the operator's maintenance-window judgment | `lock_timeout`/`statement_timeout` + backoff-retry on every DDL action, non-negotiably |
| Timezone mismatches | Documented as a risk | Detected and surfaced as a warning before `apply` runs |
| 63-byte identifier truncation collisions | Documented as a risk ("keep names short") | Checked and rejected at `plan` time |
| Existing-table conversion | Manual `partition_data_*()` batch calls | Cutover-first via `ATTACH` — near-instant, safe for tables with ongoing updates |
| Requires extension install | Yes (`CREATE EXTENSION`, BGW needs a restart) | No — connects like any client, works on managed providers without superuser |
| Config format | Rows in `part_config` | YAML/JSON, git-reviewable |

The honest counterpoint: pg_partman has roughly a decade of production hardening on its core automation. Matching that reliability bar for the mechanics themselves is real, ongoing work — these are advantages in the experience layer around the automation, not a claim of having out-automated it on day one.

## Core design decisions

These aren't defaults you should casually override — see `docs/IMPLEMENTATION_GUIDE.md` for the full reasoning.

- **PostgreSQL 14+ only.** No trigger-based partitioning, no shims for older versions.
- **Cutover-first migration, not bulk-copy-first.** Converting a populated table attaches it as a partition within one short transaction (pre-validated `CHECK` constraint + `ATTACH`) rather than copying data over an extended window while the old table stays live — the latter has a real correctness gap for tables with update activity on existing rows. Batched bulk-copy exists only as a fallback for the rare case direct attach isn't possible.
- **Every DDL action runs through a single lock/timeout/retry path.** `SET LOCAL lock_timeout`/`statement_timeout`, backoff-retry on `55P03`/`40P01`, every attempt logged — no action takes a bare, unguarded lock.
- **Plan/apply drift detection is mandatory.** `apply` refuses to run if the live schema's checksum doesn't match what `plan` computed against.
- **Approximate row counts, not `COUNT(*)`.** Status/risk reporting reads `pg_stat_user_tables.n_live_tup` instead of scanning the table — the table most likely to need a fast answer (a huge one, or one whose default partition has quietly filled up) is exactly the one where `COUNT(*)` is slowest.
- **Default partitions are actively reconciled, not just monitored.** Stray rows get moved to where they belong, not just flagged.

## Installation

```bash
git clone <repo-url>
cd postgresql-partitioner
cargo build --release
./target/release/pg-partitioner --help
```

Requires Rust 1.75+ and a PostgreSQL 14+ target. No extension install on the database side — connect with any role that has the appropriate catalog `SELECT` access (read-only commands) or `CREATE`/`ALTER`/`DROP` on the target schema (for `apply`).

## Quick start

```bash
export PG_HOST=localhost
export PG_PORT=5432
export PG_DATABASE=mydb
export PG_USER=myuser
export PG_PASSWORD=secret

# Read-only — safe against production at any time
pg-partitioner inspect              # what's partitioned, how, and any risk signals
pg-partitioner explain              # the same thing, narrated in plain language
pg-partitioner doctor               # health check with remediation suggestions

# Plan before touching anything
pg-partitioner plan --schema public --table events --output plan.json
cat plan.json | jq '.'              # review the exact DDL and duration estimate

# Dry-run, then apply for real
pg-partitioner apply --plan-file plan.json --dry-run
pg-partitioner apply --plan-file plan.json

# Automate ongoing premake + retention
pg-partitioner maintain             # test manually first, then add to cron/systemd/k8s CronJob
```

See `QUICK_START.md` for a copy-paste command reference and `PRACTICAL_USE_CASES.md` for worked scenarios (staging validation, fleet monitoring, storage projection, blue/green strategy testing).

## Commands

```
Read-only (always safe, no locks, no writes)
  inspect     current partitioning state — strategy, children, sizes, risk signals
  explain     the same state, as a plain-language narrative
  doctor      health check against known-bad patterns, with remediation
  export      partitioning config to YAML/JSON for version control

Plan → apply
  plan        compute desired vs. current state, preflight-validate, write a checksummed plan
  apply       re-checksum live schema against the plan (refuses on drift), execute

Automation
  maintain    premake future partitions + enforce retention across registered tables
```

Every subcommand supports `--host`/`--port`/`--database`/`--user`/`--password`/`--ssl-mode` (also settable via `PG_*` env vars or a TOML config file), `--log-level`, `--log-format` (`text`/`json`), and `--log-file`. Logging always writes to both the terminal and a log file — by default `$XDG_STATE_HOME/pg-partitioner/pg-partitioner.log` (or `~/.local/state/...` if that's unset), created automatically if missing.

## Project layout

```
src/            application source (see docs/project-structure.md for the module map)
tests/          integration tests + tests/fixtures/, a numbered zero-to-hero fixture catalog
docs/           design record — read docs/IMPLEMENTATION_GUIDE.md first
scripts/        setup_test_env.sh / reset_test_env.sh for the disposable test container
```

## Testing against a real database

```bash
./scripts/setup_test_env.sh --profile minimal   # fast inner loop: empty/small/large tables
./scripts/setup_test_env.sh --profile full       # full fixture catalog, including edge cases

export PG_HOST=localhost PG_PORT=5434 PG_DATABASE=partitioner_testdb \
       PG_USER=partitioner_test PG_PASSWORD=test123

pg-partitioner inspect --table public.table_02_small_populated
```

See `docs/test_environment.md` for the full fixture catalog and what each one exercises.

## Status

Phases 0–4 of the roadmap (`docs/roadmap.md`) are implemented: read-only discovery, guided plan/apply, operational guardrails (`doctor`, lock/timeout discipline, proactive risk detection), fleet-oriented maintenance, and early differentiation features (storage projection, blue/green strategy testing). See `COMPLETE_IMPLEMENTATION.md` for the phase-by-phase breakdown and what's still a stub pending integration testing against the fixture catalog.

## License

MIT

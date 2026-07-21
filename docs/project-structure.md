# Project Structure — pg-partitioner (Rust CLI)

Planning document for the postgresql-partitioner CLI, written before any code exists. Structure follows the same conventions as the sibling `postgresql-reindexer` (`pg-reindexer`) project in this workspace — same async/CLI stack, same module-per-responsibility style, same test layout — so the two tools stay consistent to work in and to hand to a new contributor. Scope maps to `roadmap.md`: modules needed for Phase 0–1 are built first; Phase 2–3 modules are named here but marked as stubs/future work.

## Directory Layout

```
postgresql-partitioner/
├── src/                     # All application source code
├── tests/                   # Integration and CLI tests
├── docs/                    # Developer documentation (this folder)
│   ├── IMPLEMENTATION_GUIDE.md   # start here — reading order, build order, non-negotiable decisions
│   ├── project-structure.md
│   ├── pg_partman_analysis.md
│   ├── desired_features.md
│   ├── roadmap.md
│   ├── index_management.md
│   ├── test_environment.md
│   ├── locking_and_retries.md
│   └── migration_design.md
├── tests/fixtures/          # numbered, zero-to-hero SQL fixture catalog (see test_environment.md)
├── scripts/                 # Helper shell scripts: setup_test_env.sh, reset_test_env.sh, release notes
├── .github/workflows/       # CI/CD: rust.yml, release.yml, security.yml
├── Cargo.toml
├── config.example.toml
├── README.md
└── CLAUDE.md
```

The three planning docs written earlier belong under `docs/` alongside this one once the repo is scaffolded — keeping `README.md` as the public-facing doc and `docs/` as the working design record, matching how `pg-reindexer` is organized.

## Cargo.toml (proposed)

```toml
[package]
name = "pg-partitioner"
version = "0.1.0"
edition = "2024"
description = "A CLI for planning, applying, and operating PostgreSQL declarative partitioning"
categories = ["database", "command-line-utilities"]
keywords = ["postgres", "postgresql", "partitioning"]
license = "MIT"

[dependencies]
tokio = { version = "1.0", features = ["macros", "rt-multi-thread", "sync", "time", "signal"] }
tokio-postgres = "0.7.18"
clap = { version = "4.0", features = ["derive"] }
anyhow = "1.0"
thiserror = "1.0"
native-tls = { version = "0.2", features = ["vendored"] }
postgres-native-tls = "0.5"
chrono = { version = "0.4", default-features = false, features = ["std", "clock", "serde"] }
chrono-tz = "0.9"                # explicit timezone handling — a named pg_partman footgun
uuid = { version = "1.0", features = ["v4"] }
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
serde_yaml = "0.9"               # human-editable, git-reviewable partition config export/import
toml = "0.8"
sha2 = "0.10"                    # checksum live schema state for plan/apply drift detection
zeroize = { version = "1", features = ["zeroize_derive"] }
dialoguer = "0.11"                # interactive setup wizard prompts
comfy-table = "7.0"               # human-readable inspect/doctor table output
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["json"] }

[dev-dependencies]
assert_cmd = "2.0"
predicates = "3.0"
tempfile = "3.8"
tokio-test = "0.4"

[profile.release]
opt-level = "z"
lto = true
codegen-units = 1
strip = true
```

Kept deliberately close to `pg-reindexer`'s dependency set (`tokio`, `tokio-postgres`, `clap`, `anyhow`, `chrono`, `uuid`, `serde`, `zeroize`, `native-tls`) for consistency across the two tools, plus what this domain specifically needs: `serde_yaml`/`sha2` for the plan/apply-with-drift-detection workflow, `dialoguer` for the guided wizard, `chrono-tz` because timezone mismatches are a named risk in `pg_partman_analysis.md`.

## Source Modules (`src/`)

### Entry point
- **main.rs** — `clap` derive `Args` + `Commands` enum (`Inspect`, `Explain`, `Plan`, `Apply`, `Doctor`, `Setup`, `Maintain`, `Undo`, `Export`, `Import`, `Index{Create, Drop, Status, Repair}`). Dispatches to the matching module. Handles SIGINT/SIGTERM for clean shutdown mid-apply.
- **lib.rs** — re-exports all public modules so `tests/` can import via `pg_partitioner::`.

### Read-only discovery path (Phase 0)
```
main.rs
  ├─► inspect.rs    # current-state report: partitioning setup, sizes, row counts, active child
  │     └─► schema.rs / queries.rs   # pg_catalog introspection (pg_partitioned_table, pg_inherits)
  ├─► explain.rs    # turns inspect.rs's report into a plain-language narrative
  └─► risk.rs       # shared risk-signal detection: unpartitioned-but-should-be, default-partition
                     # data, planner-risk partition counts, naming-collision exposure
```
This path never opens a write transaction — matches the "prove the value before automating" exit criterion in `roadmap.md` Phase 0.

### Plan / Apply path (Phase 1)
```
main.rs
  ├─► plan.rs      # diff desired vs current state → Vec<PlanAction>; serializes plan to file
  │     └─► validation.rs   # preflight: identifier collisions, max_locks_per_transaction headroom,
  │                          # interval/timezone sanity — before a plan is even written
  └─► apply.rs     # re-checksums live schema against the plan (drift guard, via sha2) before running
        └─► orchestrator.rs  # executes PlanActions in order, each through retry.rs
              ├─► retry.rs       # lock_timeout/statement_timeout + backoff-retry wrapper
              │                  # around every DDL action (see locking_and_retries.md)
              ├─► migration.rs   # cutover-first ATTACH-based conversion (default), bulk-copy-then-
              │                  # catchup fallback, DETACH CONCURRENTLY re-slicing, atomic
              │                  # DELETE...RETURNING+INSERT batch moves. Full design in
              │                  # migration_design.md — NOT a naive always-copy approach
              ├─► retention.rs   # retention policy evaluation and enforcement
              ├─► state.rs       # checkpoint/resume tracking (partitioner_state)
              └─► save.rs        # append-only audit log (partitioner_logbook)
```

### Guided setup & automation (Phase 1)
- **wizard.rs** — interactive `setup` command (via `dialoguer`): asks growth/query-pattern questions, recommends a strategy/interval, produces the desired-state input `plan.rs` consumes.
- **maintain.rs** — `maintain` subcommand: sweeps all registered partition sets, premaking future children and enforcing retention. Designed to be invoked by an external scheduler (cron/systemd timer/k8s CronJob) — no embedded daemon in v1, per `desired_features.md` §3.

### Supporting modules
| Module | Responsibility |
|---|---|
| `connection.rs` | `SecretString` password wrapper (zeroize-on-drop), connection string building/escaping, SSL/TLS setup |
| `credentials.rs` | Resolve password from CLI → env var → `.pgpass` |
| `config.rs` | Parse `config.toml`, merge with CLI args (CLI wins) |
| `schema.rs` | Catalog introspection: existing partition sets, child partitions, candidate unpartitioned tables, index/constraint presence |
| `queries.rs` | All SQL builders/consts; `quote_ident`/escaping helpers shared between real execution and dry-run preview so they can never drift apart (same pattern as `pg-reindexer`'s `index_operations.rs`) |
| `types.rs` | Shared types: `PartitionStrategy` (Range/List/Hash), `PartitionKey` (one or more columns — composite keys like `(tenant_id, created_at)` supported from the start, not retrofitted), `PartitionSetInfo`, `ChildPartitionInfo`, `RiskSignal`, `PlanAction`, `DoctorFinding` |
| `validation.rs` | Preflight checks: table/column existence, lock-budget headroom, naming-collision detection under the 63-byte identifier limit |
| `doctor.rs` | `doctor` command: runs `risk.rs` + constraint/index drift checks against a live partition set, reports remediation |
| `export.rs` | Export/import partitioning config as YAML/JSON — the config-as-code path from `desired_features.md` §5 |
| `index.rs` | `IndexSpec` DDL building, concurrent single-statement and decomposed/parallel build paths, post-build `indisvalid` verification, repair. Full design in `index_management.md`. Index carryover during initial table conversion is Phase 1; the ad-hoc `index create/drop/repair` commands are Phase 2 |
| `retry.rs` | `RetryPolicy`, the `SET LOCAL lock_timeout/statement_timeout` + backoff-retry wrapper every `PlanAction` executes through, SQLSTATE classification (retryable lock/deadlock errors vs. hard failures). Full design in `locking_and_retries.md` |
| `logging.rs` | Dual terminal + file logging, text and JSON formats (via `tracing`) |

### Planned for later phases (named now, not implemented in v1)
| Module | Phase | Responsibility |
|---|---|---|
| `repartition.rs` | 2 | Partition-key strategy migration — move a table from one partitioning column/strategy to another, reusing `migration.rs`'s cutover-then-backfill machinery rather than a separate mechanism |
| `schema_change.rs` | 2 | Safe, batched column-type rewrites across a partition set (through `retry.rs`), instead of one `ALTER TABLE` locking every child in sequence |
| `tenant.rs` | 2 | Multi-tenant "partition per tenant" recipe — tenant-lifecycle-triggered creation/archival on top of the same `registrations`/`plan`/`apply` machinery as the time-series case |
| `fleet.rs` | 3 | Multi-table/multi-database view and control |
| `daemon.rs` | 3 | Optional embedded scheduler, alternative to external cron for `maintain` |
| `advisor.rs` | 4 | Query-pattern-aware interval/strategy recommendations |
| `blue_green.rs` | 4 | Run a `plan` for a strategy change against a staging clone using the same mechanism that will later apply it to production |

## Test Files (`tests/`)

| File | What it covers |
|---|---|
| `cli_validation.rs` | Flag parsing, config precedence, `--dry-run` |
| `plan_subcommand.rs` | `plan` output (JSON/YAML), preflight validation surfacing |
| `apply_subcommand.rs` | Drift detection (plan checksum mismatch refuses to apply), rollback path |
| `migration_cutover.rs` | ATTACH-based cutover path (fixtures 01/02/03), concurrent-write correctness during cutover, fallback bulk-copy resumability, update-race warning surfaced by `plan` |
| `doctor_tests.rs` | Known-bad-pattern detection against fixture schemas |
| `index_lifecycle.rs` | Index recurses to existing + future children; drop concurrently removes from all |
| `index_repair.rs` | Interrupted concurrent build leaves expected child invalid; `index repair` fixes only that child |
| `retry_and_locking.rs` | Blocked-lock retry/backoff, deadlock detection and retry, non-retryable errors fail immediately, every attempt logged |
| `db_validation.rs` | Live DB integration tests (require a running Postgres 14+, loaded from `tests/fixtures/`) |
| `signal_handling.rs` | SIGINT/SIGTERM during a multi-batch apply |
| `run_db_tests.sh` | Orchestrates live DB test execution against the fixture catalog described in `test_environment.md` |

## Database Objects Created

Schema: `partitioner` (created on first `apply`; never touched by `inspect`/`explain`/`doctor`/`plan`).

| Table | Purpose |
|---|---|
| `partitioner_registrations` | Which tables this tool manages, their strategy/interval/retention/premake — what `maintain` reads on each scheduled run (the minimal analog of pg_partman's `part_config`) |
| `partitioner_state` | Checkpoint tracking for resumable backfills/undo operations |
| `partitioner_logbook` | Append-only audit log of every action taken (plan applied, partition created/dropped, backfill batch completed) |

## Key Data Flow

**Inspect:** `main.rs` → `inspect.rs` opens a read-only connection → `schema.rs`/`queries.rs` pull catalog + size/row-count data → `risk.rs` flags anomalies → rendered as a table (`comfy-table`) or JSON.

**Plan → Apply:** `plan.rs` computes desired vs. current state, runs `validation.rs` preflight, writes a plan file with a `sha2` checksum of the live schema it was computed against. `apply.rs` reloads that plan, recomputes the checksum against current live state, refuses to proceed on mismatch (drift), then hands the ordered `PlanAction` list to `orchestrator.rs`, which calls `migration.rs`/`retention.rs` per action and checkpoints via `state.rs` so an interrupted apply can resume rather than restart.

**Maintain:** invoked by an external scheduler, reads `partitioner_registrations`, and for each row runs the same premake/retention logic `apply.rs` would run for a one-off change — kept as shared logic rather than duplicated between the two entry points.

## Configuration Precedence (highest → lowest)
1. CLI flags
2. TOML config file (`--config` or default path)
3. Environment variables (`PG_HOST`, `PG_PORT`, `PG_DATABASE`, `PG_USER`, `PG_PASSWORD`)
4. Built-in defaults

Same precedence order as `pg-reindexer`, for consistency across both tools.

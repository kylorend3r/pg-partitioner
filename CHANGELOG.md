# Changelog

All notable changes to pg-partitioner are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **List partitioning.** A list-partitioned table is created from a separate **template table**
  that supplies its columns, rather than by converting an existing table:

  ```
  pg-partitioner plan --schema public --table events --strategy list --key region \
    --template-table events_template --output plan.json
  pg-partitioner apply --plan-file plan.json
  ```

  The template is only ever read — never modified, locked exclusively, or dropped. The parent is
  created empty, with no child partitions and no DEFAULT partition, so a row whose key matches no
  partition is rejected by PostgreSQL rather than quietly pooled somewhere it would later have to
  be reconciled out of.

- **`add-partition` command**, for adding one `FOR VALUES IN (…)` child at a time to a
  list-partitioned table:

  ```
  pg-partitioner add-partition --schema public --table events \
    --partition-name events_eu --values eu-west,eu-central [--dry-run]
  ```

  The target's partitioning strategy is read from the live catalog rather than taken from a flag,
  so pointing it at a range- or hash-partitioned table fails with a clear message. Pass the
  literal `NULL` as a value to create the partition that holds null partition keys. Re-running
  with the same name and values is a no-op; the same name with *different* values is an error
  rather than a silent skip. Every call is recorded in `partitioner_logbook` like any other DDL.

- **`plan --template-schema` / `--template-table`**, selecting template-based creation instead of
  cutover. `--template-schema` defaults to `--schema`.

- **Validation: list partition keys must be a single column**, which is a PostgreSQL constraint
  rather than a limitation of this tool (`PARTITION BY LIST` accepts one column; RANGE and HASH
  accept composite keys). Reported as `list_key_must_be_single_column`.

- `CLAUDE.md`, a development guide covering the Rust and PostgreSQL rules this codebase has
  learned the hard way, and this changelog.

### Changed

- **`plan`'s range-only flags are now rejected in template mode** rather than silently ignored.
  `--interval`, `--premake`, and `--start-date` describe a time-series window that list
  partitioning does not have. `--start-date` remains required for a range cutover.
- **`maintain` skips non-range registrations.** Premake extends a `FOR VALUES FROM/TO` sequence
  into the future; list has no "next" value set to derive and hash's buckets are fixed at
  creation. Previously it would have built range DDL for every registration regardless of
  strategy, failing on every sweep once a list table was registered.
- **`apply` registers a table only for plans that create a partition *set***. An `add-partition`
  plan targets one child of an already-known parent, so registering there would have overwritten
  that parent's real interval, premake, and retention settings with placeholders.
- **`apply` reports "Nothing to apply" for a zero-action plan** instead of exiting silently. A
  plan can legitimately have no actions — that is how template creation reports "the target
  already exists exactly as requested".
- README's Status section now separates what is implemented from what is not. It previously
  claimed all roadmap phases were complete and pointed at a document describing removed commands.

### Fixed

- **`inspect` reported `Children: 0` for every partitioned table, always.** Three queries in
  `schema.rs` passed a schema-qualified name through `quote_ident`, producing
  `"public.events"::regclass` — a single quoted identifier containing a dot, which PostgreSQL
  parses as a cast of a *column reference*. The resulting error was swallowed by the caller's
  `.unwrap_or_default()`, so a plausible zero was reported as fact. Fixed by a new
  `quote_regclass_literal` helper, applied at all eight affected sites.

- **`inspect` reported `Rows: 0` for every table, always**, and the "stray rows in default
  partition" risk signal could never fire. `WHERE relid = $1::regclass` makes PostgreSQL infer
  the *parameter* as `regclass`, and tokio-postgres can only bind Rust strings to text-family
  types — so the query failed at runtime, into another swallowed error. Relation-name parameters
  are now cast `$1::text::regclass`.

- **The DEFAULT partition was misidentified**, which would have made the stray-rows risk signal
  report an ordinary, correctly-populated partition as holding out-of-range data. The old test —
  "the child with no CHECK constraint" — holds for inheritance-based partitioning but is false for
  declarative partitioning, where no child has one and bounds live in `pg_class.relpartbound`. Now
  identified by its bound rendering as `DEFAULT`.

- **`ChildPartitionInfo` carried two structurally empty fields.** `is_default` was hardcoded
  `false`, and `constraint_expr` came from a `pg_constraint` join that is always NULL for a
  declarative partition. Both now derive from the partition bound, so `constraint_expr` carries
  the actual `FOR VALUES …` expression.

- **`plan` panicked on any table with no constraints.** `array_agg` over zero rows returns NULL,
  which deserializes into `Vec<String>` by panicking. A table with no primary key, unique, or
  CHECK constraint is entirely ordinary — and is exactly the shape a bare template table has.

## [0.1.0] — 2026-07-21

### Added

- Read-only discovery: `inspect`, `explain`, `export`, and risk-signal detection (unpartitioned
  large tables, stray default-partition rows, planner-risk partition counts, identifier-collision
  exposure, lock-budget headroom).
- Range partitioning of an existing table via ATTACH-first cutover: `plan` → `apply`, with a
  schema checksum that makes `apply` refuse to run against a drifted schema.
- `install`, provisioning `partitioner_registrations`, `partitioner_state`, and
  `partitioner_logbook` under a dedicated `partitioner` schema.
- `register` / `unregister` for declaring a table as managed, and auto-registration after a
  successful `apply`.
- `maintain` for premaking future range partitions, and `daemon` for running that sweep on a timer.
- Lock and timeout discipline on every DDL action, with backoff-retry.

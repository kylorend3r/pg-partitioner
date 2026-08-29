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

- **Hash partitioning.** Creates the parent and its full bucket set from a template in one plan:

  ```
  pg-partitioner plan --schema public --table events --strategy hash --key tenant_id \
    --template-table events_template --hash-partitions 8 --output plan.json
  ```

  All buckets are created at once because a hash table is only usable when every remainder is
  covered — a missing one rejects any row that hashes to it, and hash tables cannot have a
  DEFAULT partition to catch it. That same restriction is why hash is template-only: the cutover
  flow needs a default-like bucket to hold an existing table's rows, so a populated table cannot
  be converted to hash at all. `--strategy hash` without `--template-table` fails and explains
  why. There is no incremental `add-partition` for hash — changing the modulus means recreating
  every bucket.

- **Validation: hash partition keys must be `smallint`, `integer`, `bigint`, or `uuid`.**
  PostgreSQL will hash `text` or `date` too, but a key with low cardinality or a skewed
  distribution produces lopsided buckets that only surface as a performance problem much later.
  Reported as `hash_key_type_unsupported`. This is the first column-*type* check in the tool;
  everything before it only asked whether a column existed.

- **Index recreation when creating a table from a template.** The template's indexes are rebuilt
  on the new parent, so partitions added later inherit them. A unique index is carried over only
  when it includes the partition key, as PostgreSQL requires; one that doesn't is skipped with a
  warning. Expression indexes are skipped — they can't be rebuilt from column names alone.

- **`plan --template-schema` / `--template-table`**, selecting template-based creation instead of
  cutover. `--template-schema` defaults to `--schema`.

- **`plan --hash-partitions <N>`**, the bucket count for hash. Required with `--strategy hash`,
  rejected otherwise, and must be at least 1.

- **Validation: list partition keys must be a single column**, which is a PostgreSQL constraint
  rather than a limitation of this tool (`PARTITION BY LIST` accepts one column; RANGE and HASH
  accept composite keys). Reported as `list_key_must_be_single_column`.

- This changelog.

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

- **Recreated composite indexes could come out with their columns in the wrong order.**
  `index::get_indexes_for_table` ordered index columns by `attnum` rather than by their position
  in the index, so any index whose column order differed from the table's column order was
  reported reversed — and rebuilt that way on the new parent. Index column order determines which
  queries an index can serve, so this produced a valid index that answered different questions
  than the original.

- **A hash bucket set could come up a partition short on a long table name.** Bucket names are
  `{table}_p{remainder}`, and PostgreSQL truncates an over-long identifier silently rather than
  erroring — so at a high modulus two buckets could collapse onto one name, the second
  `CREATE TABLE IF NOT EXISTS` would no-op, and the missing remainder would surface much later as
  rows rejected with no matching partition. Now rejected at plan time as
  `hash_partition_name_too_long`.

- **The identifier-length budget charged list and hash tables for a range-only name.** `plan`
  measured `{table}_{interval}` against the 63-byte limit for every strategy, but `--interval` is
  an inert default for list and hash — so a legal table name could be rejected over a value the
  user never supplied, before the checks that would have explained the real problem.

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

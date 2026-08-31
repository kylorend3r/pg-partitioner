# Changelog

All notable changes to pg-partitioner are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Warning when a template's identity column won't be copied.** Creating a table from a template
  that has an identity column succeeds, but the new table's column arrives with neither the
  identity nor a default, so the first `INSERT` that omits it fails on a NOT NULL column. Reported
  as `identity_column_not_copied` — a warning, not a blocker, since the rest of the table is fine.


- **Plans state their execution order.** `PlanAction` gains a 1-based `sequence`, so the order is
  written into the plan file rather than left to be inferred from array position, and `plan` and
  `apply --dry-run` print a numbered list:

  ```
  Execution order:
    1. create_partition_set — Create public.sessions as PARTITION BY HASH (tenant_id) …
    2. create_partition — Create 4 hash bucket(s) for public.sessions (MODULUS 4)
    3. create_index — Recreate public.sessions_template's non-unique indexes on public.sessions
  ```

  Logs and `partitioner_logbook` now say `Step 4 of 6` instead of a bare action index. The list
  order remains what actually executes: `apply` checks the numbers against it and refuses a plan
  file that has been reordered by hand, so the field cannot drift into a second source of truth.
  A plan written before this field existed has no `sequence` and still applies.


- **Range partitioning can now create a new table from a template**, so all three strategies work
  both ways round — the tool has two migrations, and range is no longer stuck in one of them:

  ```
  pg-partitioner plan --schema public --table events --strategy range --key created_at \
    --template-table events_template --interval "1 month" --premake 3 --output plan.json
  ```

  The parent is created from the template's columns and its period window is created immediately:
  today's period (or `--start-date`'s) through `--premake` periods ahead. `--start-date` is
  optional here — it defaults to today, because a new table has no existing rows the window has to
  sit ahead of — and still may not be in the future. The new table is auto-registered, so
  `maintain` keeps its window topped up from the next sweep onward.

  **It gets no DEFAULT partition**, unlike the cutover flow. A row landing in DEFAULT blocks
  PostgreSQL from ever creating that period's real partition, which is exactly what the next
  maintenance sweep tries to do — so the sweep would start failing. An insert outside the premade
  window is rejected instead.

- **`register --hash-partitions <N>`**, recording a hash table's bucket count so `inspect` can tell
  a hand-dropped bucket from a healthy set. Rejected for range and list, like `plan`'s flag of the
  same name. `register` now also prints a note when the strategy it just recorded is one
  `maintain` will never act on.

- **`maintain` reports what it deliberately did not maintain.** Hash and list registrations are
  listed under `Informational (not maintained)` with the reason, instead of being skipped in
  silence:

  ```
  Maintenance complete: 8 tables processed, 1 partitions created, 0 dropped
  Informational (not maintained, 13):
    - public.hash_info (hash): hash buckets are fixed at creation; adding one changes the
      modulus, so every existing row would have to be rehashed and redistributed
  ```

- **`inspect` detects a hash bucket that has gone missing.** For hash — and only hash — every child
  is a bucket, so the registered modulus and the live child count should agree. When they don't,
  reconciliation reports `DriftMismatch` naming both numbers. This matters more than it sounds:
  there is no DEFAULT partition in a hash table, so a missing remainder means every row that hashes
  to it is rejected outright. A hash row registered before this release has no recorded modulus and
  is reported as healthy rather than as having zero buckets.

- **`scripts/partition_table.sh`**, a one-shot wrapper that partitions a single table on a
  remote database from environment variables alone — no flags to remember, so the same
  invocation works from a laptop, a jump host, or a CI job. It runs `install` → `plan` →
  `apply`, plus one `add-partition` per entry in `LIST_PARTITIONS`, and covers all three
  strategies:

  ```
  PG_HOST=db.internal PG_DATABASE=app PG_USER=deploy PG_PASSWORD=… \
  PARTITION_TABLE=events PARTITION_STRATEGY=range PARTITION_KEY=created_at \
  PARTITION_START_DATE=2026-04-01 ./scripts/partition_table.sh
  ```

  `PARTITION_HELP=1` prints every variable. `DRY_RUN=1` plans without executing; without
  `ASSUME_YES=1` it prints the target and prompts for confirmation, and refuses to execute DDL
  at all when there is no terminal to prompt on. `PG_HOST`/`PG_DATABASE`/`PG_USER` are required
  even though the CLI itself would default them, because against a remote database a typo that
  silently targets localhost is worse than a refusal. A variable that does not apply to the
  chosen strategy (`HASH_PARTITIONS` on a list run, `TEMPLATE_TABLE` on a range run) is an error
  rather than a no-op.

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

- **`partitioner_logbook`'s `timestamp` column is now `created_date`.** `timestamp` is a type name
  in SQL and needed quoting at every use site. **Breaking for an existing install:** the tool
  defines the schema but does not migrate it, so a database created before this change has the old
  column and its `INSERT`s will fail. Drop the table (or the whole `partitioner` schema) and re-run
  `install`.

- **The `max_locks_per_transaction >= 256` preflight check is gone.** It hard-blocked `plan` on any
  default-configured server, forcing an `ALTER SYSTEM` and a restart before anything could be
  planned at all. The threshold bore no relation to a plan's actual size — a cutover with
  `--premake 12` takes roughly fifteen locks against a default budget of 64. Lock pressure is still
  reported, by the two signals that measure the real thing: the planner's `large_partition_count`
  warning, and `inspect`'s headroom signal driven by a table's actual child count.


- **`tenant::create_tenant_partition` no longer takes a `tenant_column` argument.** A
  `PARTITION OF … FOR VALUES IN (…)` bound is matched against the column the parent was declared
  `PARTITION BY LIST` on, so the parameter had nowhere to go and was silently ignored. Library API
  only — no command exposes this module.

- **`maintain`'s "tables processed" now counts only the tables it can actually act on.** It
  previously counted every registration, including the hash and list tables it skips, which
  overstated what a sweep had done — a run that touched nothing still reported every registered
  table as processed.

- **`plan`'s range-window flags are now accepted in template mode for range**, where they describe
  the new table's period window. They remain rejected for list and hash, which have no time window,
  and the message now names the strategy.


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

- **Converting a table with an identity column failed half-way through the migration.** PostgreSQL
  does not allow a partition to own an identity column, and the shadow parent is built with
  `LIKE … INCLUDING DEFAULTS`, which does not carry identity across — so the original kept it and
  could never be attached. The cutover died at the ATTACH step, *after* the bounding CHECK had been
  added to and validated on the real table:

  ```
  table "events_legacy" being attached contains an identity column "id"
  DETAIL: The new partition may not contain an identity column. (SQLSTATE: 55000)
  ```

  `plan` now refuses up front with `identity_column_unsupported` and says how to proceed: convert
  the column to a plain sequence default, which the cutover *does* carry over.

  ```sql
  ALTER TABLE public.events ALTER COLUMN id DROP IDENTITY;
  CREATE SEQUENCE events_id_seq OWNED BY public.events.id;
  SELECT setval('events_id_seq', (SELECT COALESCE(max(id), 1) FROM public.events));
  ALTER TABLE public.events ALTER COLUMN id SET DEFAULT nextval('events_id_seq');
  ```

  Not a like-for-like swap: `GENERATED ALWAYS AS IDENTITY` rejects user-supplied values for the
  column and a plain default does not. `bigserial` and `uuid DEFAULT gen_random_uuid()` columns are
  unaffected — they are ordinary defaults, and always worked.

- **Validation told you what was wrong but never what to do about it.** Every `ValidationError`
  carries a `suggestion`, and `plan` discarded all of them when rendering, so an operator got a
  diagnosis and no remedy. Suggestions are now printed with the error that carries them.


- **A failed migration didn't say what went wrong.** `tokio_postgres::Error`'s `Display` is the
  fixed string `"db error"` — the message, `DETAIL`, `HINT` and the relation or constraint involved
  all hang off `as_db_error()`, which nothing called. Worse, the orchestrator's failure log recorded
  the action id but not the error, so a failed run's log file contained no reason at all. Before:

  ```
  Error: Batch action 'atomic_cutover' failed (attempt 1): db error (SQLSTATE: 23514)
  ```

  After:

  ```
  Error: Batch action 'atomic_cutover' failed (attempt 1): partition constraint of relation
  "orders_legacy" is violated by some row [relation orders_legacy] (SQLSTATE: 23514)
  ```

  The same text now reaches the terminal, the log file, and `partitioner_logbook.details`, and the
  retry paths log it too so a retry storm is diagnosable while it is happening. Note this can carry
  data values: PostgreSQL puts the offending row into `DETAIL` for some constraint violations, and
  that will appear in the log file and the audit table.

- **A logbook write that failed was discarded silently.** `append_log`'s result was dropped with
  `.ok()`, so a logbook that had stopped recording looked identical to one that was working. It
  stays non-fatal — aborting between two DDL statements is worse than losing an audit row — but is
  now logged as a warning.


- **`plan --format yaml` silently wrote JSON.** The flag was parsed and then never read, so the
  documented `json | yaml` choice had exactly one outcome. `--format yaml` now writes YAML,
  `apply` reads a plan file in either format, and an unrecognised `--format` is rejected rather
  than falling back to JSON.

- **Retention's "older than N days" selection ignored the age entirely.** The query behind
  `RetentionType::Days` referenced neither the day count nor the partition column; it selected
  every non-default child of the parent ordered by name, `LIMIT (COUNT(*) - 1)` — "all the
  partitions but one". Since the result feeds straight into `drop_partition`, wiring retention up
  would have dropped nearly a table's entire history no matter what policy was configured. It now
  returns nothing, matching its months/years counterparts, so retention stays a no-op until it is
  genuinely implemented rather than a destructive one.

- **Converting an existing table to list or hash produced a plan that failed half-way through the
  migration.** `plan --strategy list` without `--template-table` fell into the cutover path and
  wrote a plan whose ATTACH action carried a `FOR VALUES FROM (MINVALUE) TO (…)` bound — meaningless
  to a `PARTITION BY LIST` parent. Nothing caught it until `apply`, by which point the bounding
  CHECK constraint had already been added to and validated on the real table. Both strategies are
  now rejected at plan time, with a message saying to use `--template-table` instead.

- **A hash table that already existed was reported as being partitioned `BY LIST`.** The
  "nothing to create" message in the template flow hard-coded the strategy name, so re-running a
  hash plan against its own target described the table wrongly.

- **`--ssl-mode require` did not require TLS.** The chosen mode selected a TLS connector but was
  never written into the connection string, and tokio-postgres assumes `prefer` when the string
  is silent — so a `require` connection to a server with `ssl = off` negotiated down to plain
  text and succeeded, sending the password and every DDL statement in the clear. It now fails
  the connection instead, with a message naming both possible causes. Worth knowing when you
  hit it: TLS here validates the certificate chain and hostname, so `require` behaves like
  libpq's `verify-full` — a server with an internal-CA or self-signed certificate that
  `psql "sslmode=require"` accepts is rejected here.

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

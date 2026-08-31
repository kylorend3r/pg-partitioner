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
| Interface | SQL functions only | CLI (`inspect`, `plan`, `apply`, `maintain`, ...) |
| Preview before changing anything | No — config changes take effect on the next scheduled run | `plan` shows exactly what will run, `apply` refuses to proceed if the schema drifted since |
| Destructive-action confirmation | A parameter that must equal the string `"yes"` | Confirmation showing actual row counts and table names |
| Lock/timeout discipline | Left to the operator's maintenance-window judgment | `lock_timeout`/`statement_timeout` + backoff-retry on every DDL action, non-negotiably |
| Timezone mismatches | Documented as a risk | Detected and surfaced as a warning before `apply` runs |
| 63-byte identifier truncation collisions | Documented as a risk ("keep names short") | Checked and rejected at `plan` time — table names are capped at 60 characters, leaving headroom for derived partition/index name suffixes |
| Existing-table conversion | Manual `partition_data_*()` batch calls | Cutover-first via `ATTACH` — near-instant, safe for tables with ongoing updates |
| Requires extension install | Yes (`CREATE EXTENSION`, BGW needs a restart) | No — connects like any client, works on managed providers without superuser |
| Config format | Rows in `part_config` | YAML/JSON, git-reviewable |

The honest counterpoint: pg_partman has roughly a decade of production hardening on its core automation. Matching that reliability bar for the mechanics themselves is real, ongoing work — these are advantages in the experience layer around the automation, not a claim of having out-automated it on day one.

## Core design decisions

These aren't defaults you should casually override — `docs/IMPLEMENTATION_GUIDE.md` has the full reasoning (a local design record, not part of the repo).

- **PostgreSQL 14+ only.** No trigger-based partitioning, no shims for older versions.
- **Cutover-first migration, not bulk-copy-first.** Converting a populated table attaches it as a partition within one short transaction (pre-validated `CHECK` constraint + `ATTACH`) rather than copying data over an extended window while the old table stays live — the latter has a real correctness gap for tables with update activity on existing rows. Batched bulk-copy exists only as a fallback for the rare case direct attach isn't possible.
- **Every DDL action runs through a single lock/timeout/retry path.** `SET LOCAL lock_timeout`/`statement_timeout`, backoff-retry on `55P03`/`40P01`, every attempt logged — no action takes a bare, unguarded lock.
- **Plan/apply drift detection is mandatory.** `apply` refuses to run if the live schema's checksum doesn't match what `plan` computed against.
- **Approximate row counts, not `COUNT(*)`.** Status/risk reporting reads `pg_stat_user_tables.n_live_tup` instead of scanning the table — the table most likely to need a fast answer (a huge one, or one whose default partition has quietly filled up) is exactly the one where `COUNT(*)` is slowest.
- **Default partitions are treated as a problem to fix, not just a metric.** Rows that accumulate in a default partition are surfaced as a risk signal with a live count; moving them back to where they belong is the intended next step and is not yet implemented.

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

# Plan before touching anything
pg-partitioner plan --schema public --table events \
  --strategy range --key created_at --interval "1 month" --premake 3 \
  --start-date 2026-01-01 \
  --output plan.json
cat plan.json | jq '.'              # review the exact DDL and duration estimate

# Dry-run, then apply for real
pg-partitioner apply --plan-file plan.json --dry-run
pg-partitioner apply --plan-file plan.json

# Or: create a NEW range-partitioned table from a template instead of converting one
pg-partitioner plan --schema public --table events \
  --strategy range --key created_at --template-table events_template \
  --interval "1 month" --premake 3 --output plan.json
pg-partitioner apply --plan-file plan.json

# Or: create a NEW list-partitioned table from a template, then fill it in
pg-partitioner plan --schema public --table events \
  --strategy list --key region --template-table events_template \
  --output plan.json
pg-partitioner apply --plan-file plan.json
pg-partitioner add-partition --schema public --table events \
  --partition-name events_eu --values eu-west,eu-central

# Or: create a hash-partitioned table with a fixed number of buckets
pg-partitioner plan --schema public --table sessions \
  --strategy hash --key tenant_id --template-table sessions_template \
  --hash-partitions 8 --output plan.json
pg-partitioner apply --plan-file plan.json

# Automate ongoing premake + retention
pg-partitioner maintain             # test manually first, then add to cron/systemd/k8s CronJob
```

`pg-partitioner <command> --help` is the authoritative flag reference; the walkthroughs below cover the two migrations the tool performs.

## The two migrations

Everything this tool does to partition a table is one of two things, and which one you get is
decided by a single flag, `--template-table`:

| | **Existing-table strategy** (no `--template-table`) | **Template strategy** (`--template-table`) |
|---|---|---|
| What it does | Converts a table you already have, in place, ATTACH-first: the original becomes the first child of a new partitioned parent under the same name | Creates a *new* partitioned table whose columns are copied from a template table, which is only ever read |
| Existing rows | Preserved — they end up in the `_legacy` partition | None; the new table starts empty |
| Strategies | **Range only** | **Range, list, and hash** |
| Needs | `--start-date`, ahead of every existing row | A template table |

**Why the existing-table strategy is range-only, and will stay that way.** Converting a populated
table means giving its rows somewhere to live in the new parent. Hash offers nowhere: a
hash-partitioned table cannot have a DEFAULT partition, and every bucket's contents are fixed by
`hash(key) % modulus`. List could technically attach the old table as its DEFAULT partition, but
then no real `FOR VALUES IN (…)` partition could be created until every matching row had been
moved out of it — PostgreSQL refuses to create a partition that overlaps rows sitting in DEFAULT.
Both are rejected at plan time, pointing you at `--template-table`.

## How to partition an existing table (date/range partitioning)

This walks through converting a plain table — empty or already holding data — into a native `PARTITION BY RANGE` table keyed on a timestamp column, using the cutover-first strategy described above.

```bash
# 0. One-time setup: provision pg-partitioner's own config/state/audit tables,
#    under a dedicated `partitioner` schema (created automatically)
pg-partitioner install

# 1. Compute a plan against the table you want to convert
pg-partitioner plan --schema public --table events \
  --strategy range --key created_at --interval "1 month" --premake 3 \
  --retention-type months --retention-value 12 \
  --start-date 2026-01-01 \
  --output plan.json

# 2. Review it — both the actions and any warnings
cat plan.json | jq '.actions[].description'
cat plan.json | jq '.warnings'
```

`--strategy`/`--key`/`--start-date` are required (`--start-date` mirrors nothing else — it's new: the minimum date, `YYYY-MM-DD`, to generate real per-period partitions from). `--interval`/`--premake` default to `"1 month"`/`3` if omitted. `--retention-type`/`--retention-value` are optional, but if you set one you must set the other — and whatever you pass here carries straight through to the config `apply` auto-registers, so you don't need a separate `register` call afterward just to set retention.

**What `--start-date` actually controls**: data older than this date lands in one `MINVALUE`-bounded legacy partition (same as before); from `--start-date` forward through today plus `--premake` periods, you get a real, individually-named partition per period — not just one undifferentiated bucket for all pre-existing data. If that span is large (e.g. a `--start-date` years back with daily partitions), `plan` warns rather than blocking — check `.warnings` for a `large_partition_count` entry before you `apply`.

A normal OLTP table (surrogate `id` primary key, not on `created_at`) will show a warning like:

```
unique_index_missing_partition_key: Unique index 'events_pkey' does not include all
partition key columns (created_at). Unique constraints on partitioned tables must
include the partition key.
```

This is expected, not a blocker — PostgreSQL requires unique indexes on a partitioned table to include the partition key, so `apply` automatically **skips** recreating that specific index on the new parent (everything else gets recreated) and leaves the original constraint enforced only on the now-archived legacy partition. If you need a true partition-wide unique constraint, that's a separate, not-yet-automated step (redefine the index to include `created_at`, e.g. `UNIQUE (id, created_at)`).

```bash
# 3. Dry-run, then apply for real
pg-partitioner apply --plan-file plan.json --dry-run
pg-partitioner apply --plan-file plan.json

# 4. Confirm it worked
pg-partitioner inspect --table events
```

`inspect` should report the table under **Configuration Reconciliation** as `Healthy` — meaning the registered config now matches what's actually in the live catalog.

**What `apply` actually does**, in one short transaction at the core (the rest is cheap, non-blocking prep/cleanup around it):

1. Adds a `NOT VALID` bounding `CHECK` constraint on `events` (bounded at `--start-date`, e.g. `created_at < '2026-01-01 00:00:00'`), then validates it separately — a non-blocking scan under `SHARE UPDATE EXCLUSIVE`, not the table's normal lock.
2. Creates an empty partitioned shadow table with the same columns (`LIKE events INCLUDING DEFAULTS`).
3. **The actual cutover** — one transaction: renames `events` → `events_legacy`, renames the shadow table into `events`, attaches `events_legacy` as a partition (covering everything older than `--start-date`). Fast because step 1 already proved the bound; existing rows are never copied.
4. Creates a default partition, plus one real partition per period from `--start-date` through today plus `--premake` periods — each named `events_<start>_<end>` (e.g. `events_2026_07_22_2026_07_23`), not just a handful of forward-looking ones.
5. Recreates non-unique indexes on the new parent (unique/PK indexes are skipped, per above).

The table is also auto-registered into pg-partitioner's config at this point — no need to run `register` separately for a table converted this way. `register`/`unregister` exist for declaring management of a table partitioned by some *other* means, or for editing strategy/retention/premake after the fact:

```bash
pg-partitioner register --schema public --table events \
  --strategy range --key created_at --interval "1 month" --premake 3 \
  --retention-type months --retention-value 12
```

`maintain` keeps registered tables' premake window topped up: each run recomputes "today's period through `--premake` periods ahead" and creates whatever's missing — including a partition you deleted by hand, not just extending the tail. This works for range tables from either migration, converted or template-created.

**Hash and list registrations are informational only.** `maintain` records and reports them but never creates anything for them, and they are excluded from its "tables processed" count rather than padding it:

```
Maintenance complete: 8 tables processed, 1 partitions created, 0 dropped
Informational (not maintained, 13):
  - public.sessions (hash): hash buckets are fixed at creation; adding one changes the modulus,
    so every existing row would have to be rehashed and redistributed
  - public.events (list): list partitions are added deliberately, one value set at a time, with
    `pg-partitioner add-partition`
```

For hash this is structural, not a missing feature. A hash child's contents are decided by `hash(key) % modulus`, so adding a bucket changes the modulus and therefore changes which bucket **every row already stored** belongs to. That is not a partition creation, it is a full redistribution of the table — not something a scheduled sweep can do unattended. Register the table anyway: `register --hash-partitions <N>` records the bucket count, and `inspect` then reports `DriftMismatch` if a bucket ever goes missing, which otherwise surfaces only as rows being rejected with no matching partition.

**Known gap:** retention enforcement (dropping partitions older than the registered policy) is still a placeholder — `maintain` won't drop anything yet.

## How to create a new range-partitioned table from a template

The section above converts a table you already have. When there is nothing to convert — a table
you are about to create, or one you would rather build partitioned from the start — pass
`--template-table` and range works the same way list and hash do:

```bash
# The template: an ordinary table, used only as a structure source.
psql -c "CREATE TABLE events_template (
           id bigserial, tenant_id int NOT NULL,
           payload jsonb, created_at timestamptz NOT NULL DEFAULT now()
         )"

pg-partitioner plan --schema public --table events \
  --strategy range --key created_at \
  --template-table events_template \
  --interval "1 month" --premake 3 \
  --output plan.json
pg-partitioner apply --plan-file plan.json
```

That creates `public.events` partitioned `BY RANGE (created_at)`, with today's period plus three
ahead already in place, and registers it so `maintain` extends the window from the next sweep on.
The template's non-unique indexes are rebuilt on the new parent, so partitions added later inherit
them.

**`--start-date` is optional here**, and defaults to today. In the cutover flow it is required
because it is the boundary every existing row must predate; a table created from a template has no
existing rows, so today's period is the sensible first partition. Pass it to backfill earlier
periods — `--start-date 2026-01-01` creates every month from January through the premake window.
It still may not be in the future.

**It gets no DEFAULT partition**, and that is deliberate. A row that lands in a range table's
DEFAULT partition stops PostgreSQL from ever creating the real partition covering that period —
which is precisely what the next `maintain` sweep will try to do, so the sweep would start failing.
An insert outside the premade window is rejected instead, which is the failure you can see. (The
cutover flow does create one: it needs somewhere to put rows arriving beyond the window while the
swap happens.)

## How to create a new list-partitioned table

Range can be created either way. List works one way round only: you declare the value sets up
front, so there's nothing to convert — the parent is created new, from a **template table** that
supplies its columns.

```bash
# 0. A template: an ordinary, existing table used only as a structure source.
#    It is read, never modified, locked exclusively, or dropped.
psql -c "CREATE TABLE events_template (
           id bigserial, region text, tenant_id int NOT NULL,
           payload jsonb, created_at timestamptz NOT NULL DEFAULT now()
         )"

# 1. Create the parent — empty, with zero child partitions
pg-partitioner plan --schema public --table events \
  --strategy list --key region \
  --template-table events_template \
  --output plan.json
pg-partitioner apply --plan-file plan.json

# 2. Add partitions one value set at a time, as often as you need
pg-partitioner add-partition --schema public --table events \
  --partition-name events_eu --values eu-west,eu-central
pg-partitioner add-partition --schema public --table events \
  --partition-name events_us --values us-east
```

`--template-schema` defaults to `--schema`; pass it when the template lives elsewhere. The
range-only flags (`--interval`, `--premake`, `--start-date`) are **rejected** here rather than
silently ignored — list partitioning has no time window to premake.

**The parent is created empty and stays that way until you add partitions** — no default
partition, not even an empty one. A row whose `region` matches no partition is rejected by
PostgreSQL rather than quietly pooled somewhere you'd have to reconcile it out of later. That
is deliberate; for a catch-all, create one explicitly:

```bash
psql -c "CREATE TABLE events_catchall PARTITION OF events DEFAULT"
```

To partition on a column that can be null, add the partition that holds nulls with the literal
`NULL` — the one value that isn't a plain string:

```bash
pg-partitioner add-partition --schema public --table events \
  --partition-name events_unknown --values NULL
```

`add-partition` reads the target's strategy from the live catalog rather than trusting a flag,
so pointing it at a range- or hash-partitioned table (or a plain one) fails with a clear message
instead of producing DDL PostgreSQL rejects for a less obvious reason. It's safe to re-run:
the same name with the same values is a no-op, while the same name with *different* values is
an error rather than a silent skip. Every call goes through the same plan → apply →
`partitioner_logbook` path as everything else, and `--dry-run` shows the DDL without running it.

Creation is idempotent the same way: if the target already exists as a list-partitioned table
on the same key, `plan` emits a zero-action plan and says so; if it exists as anything else,
it's a hard error naming the collision rather than a silent no-op.

The template's indexes are recreated on the new parent, so partitions added later inherit them
automatically. A unique index is carried over only if it includes the partition key — PostgreSQL
requires that on a partitioned table — and one that doesn't is skipped with a warning rather than
failing the run.

**One thing list partitioning does not do.** `maintain` skips list tables entirely: premake is a
time-series idea and there's no "next" value set to derive, so new partitions are always an
explicit `add-partition` call.

## How to create a hash-partitioned table

Hash spreads rows evenly across a fixed number of buckets by `hash(key) % modulus`. It's for
distributing write load or table size across partitions when there's no natural time or category
to slice on — not for querying a subset, since a hash bucket has no meaning you can filter by.

```bash
pg-partitioner plan --schema public --table events \
  --strategy hash --key tenant_id \
  --template-table events_template \
  --hash-partitions 8 \
  --output plan.json
pg-partitioner apply --plan-file plan.json
```

That creates the parent plus all 8 buckets (`events_p0` … `events_p7`) and recreates the
template's indexes, in one plan.

**Hash is template-only, and that's structural rather than a missing feature.** A hash-partitioned
table cannot have a DEFAULT partition — every row is assigned to a bucket by
`hash(key) % modulus`, so there is no bucket meaning "everything else". The cutover flow depends
on exactly such a bucket to hold an existing table's rows, so there is no way to convert a
populated table to hash. `--strategy hash` without `--template-table` fails and says so.

**`--hash-partitions` is fixed at creation.** All buckets are created at once, because a hash
table is only usable when every remainder is covered — a missing one rejects any row that hashes
to it, with no default to catch it. There is no incremental `add-partition` for hash: changing
the modulus means recreating every bucket and redistributing every row. Pick a modulus with room
to grow.

**A hash table is therefore never maintained on a schedule**, only watched. `maintain` lists it
under `Informational (not maintained)` and moves on — there is no "next partition" to create, and
adding one would mean rehashing every row already stored. What registration buys you is detection:
record the bucket count with `register --hash-partitions 8` (or let `apply` record it for you), and
`inspect` reports a `DriftMismatch` if the live child count ever stops matching:

```
✗ public.events [DriftMismatch]: hash buckets: registered modulus=8 actual children=7
```

Worth watching for, because a hash table with a missing bucket has no DEFAULT partition to catch
the rows that hash to it — they are simply rejected.

**The partition key must be `smallint`, `integer`, `bigint`, or `uuid`.** PostgreSQL is more
permissive — it will hash `text` or `date` quite happily — but this is a deliberate guardrail.
Hash distribution is only as good as the key's cardinality and spread, and a poorly-distributed
key produces lopsided buckets that surface much later as a performance problem. Anything else is
rejected at `plan` time with `hash_key_type_unsupported`.

## Partitioning one table from environment variables

`scripts/partition_table.sh` runs the whole sequence — `install` → `plan` → `apply`, plus one
`add-partition` per list child — with no flags to remember: everything is an environment
variable, so the same invocation works from a laptop, a jump host, or a CI job against a remote
database.

```bash
cargo build --release        # the script finds target/release/pg-partitioner on its own

# Convert an existing time-series table to monthly range partitions
PG_HOST=db.internal PG_PORT=5432 PG_DATABASE=app PG_USER=deploy PG_PASSWORD=… \
PARTITION_TABLE=events PARTITION_STRATEGY=range PARTITION_KEY=created_at \
PARTITION_START_DATE=2026-04-01 \
./scripts/partition_table.sh

# Create a hash-partitioned table with 4 buckets from a template
PG_HOST=db.internal PG_DATABASE=app PG_USER=deploy PG_PASSWORD=… \
PARTITION_TABLE=sessions PARTITION_STRATEGY=hash PARTITION_KEY=tenant_id \
TEMPLATE_TABLE=sessions_template HASH_PARTITIONS=4 \
./scripts/partition_table.sh

# Create a NEW range-partitioned table from a template, rather than converting one
PG_HOST=db.internal PG_DATABASE=app PG_USER=deploy PG_PASSWORD=… \
PARTITION_TABLE=events PARTITION_STRATEGY=range PARTITION_KEY=created_at \
TEMPLATE_TABLE=events_template PARTITION_PREMAKE=6 \
./scripts/partition_table.sh

# Create a list-partitioned table and its children in one go
PG_HOST=db.internal PG_DATABASE=app PG_USER=deploy PG_PASSWORD=… \
PARTITION_TABLE=events PARTITION_STRATEGY=list PARTITION_KEY=region \
TEMPLATE_TABLE=events_template \
LIST_PARTITIONS='events_eu=eu-west,eu-central;events_us=us-east;events_null=NULL' \
./scripts/partition_table.sh
```

`PARTITION_HELP=1 ./scripts/partition_table.sh` prints every variable it reads. The ones worth
knowing up front:

| Variable | Meaning |
|---|---|
| `DRY_RUN=1` | plan and print the actions, execute no DDL |
| `ASSUME_YES=1` | skip the confirmation prompt — **required** for non-interactive runs |
| `SKIP_INSTALL=1` | don't run `install`; the metadata tables are already there |
| `PLAN_FILE` | where to write the plan (default: a temp file, kept and printed) |
| `PG_PARTITIONER_BIN` | which binary to run (default: `target/release`, then `target/debug`, then `$PATH`) |

Two deliberate behaviours:

- **`PG_HOST`, `PG_DATABASE`, and `PG_USER` are required by the script**, though the CLI itself
  would default them to `localhost`/`postgres`/… . Against a remote database a typo that silently
  targets localhost is worse than a refusal.
- **A variable that doesn't apply to the chosen strategy is an error, not a no-op** —
  `HASH_PARTITIONS` on a list run, `PARTITION_START_DATE` on a hash run. A run that appears to have
  honoured a setting it ignored is how the wrong layout reaches production. `TEMPLATE_TABLE` is
  required for list and hash and optional for range, where setting it switches from converting the
  existing table to creating a new one.

Without `ASSUME_YES=1` the script prints what it is about to do — server, target, strategy, key,
template, bucket count — and waits for you to type `yes`.

## Commands

```
Read-only (always safe, no locks, no writes)
  inspect        current partitioning state — strategy, children, sizes, risk signals
  explain        the same state, as a plain-language narrative
  export         partitioning config to YAML/JSON for version control

Setup
  install        provision pg-partitioner's own config/state/audit tables (under the `partitioner` schema)

Plan → apply
  plan           compute desired vs. current state, preflight-validate, write a checksummed plan
                 (with --template-table: create a new table, any strategy; without it: convert
                 an existing table, range only)
  apply          re-checksum live schema against the plan (refuses on drift), execute
  add-partition  add one FOR VALUES IN (...) partition to a list-partitioned table

Configuration
  register       declare a table as managed (or update its strategy/interval/retention/premake;
                 --hash-partitions records a hash table's bucket count)
  unregister     stop managing a table's partitioning configuration

Automation
  maintain       premake future partitions + enforce retention across registered range tables
                 (hash/list registrations are reported as informational, never modified)
```

`--ssl-mode require` is enforced on the wire, not merely requested: it is written into the connection string, so a server with `ssl = off` fails the connection instead of quietly negotiating down to cleartext. It also validates the certificate chain and hostname, which makes it closer to libpq's `verify-full` than to libpq's `require` — an internal-CA or self-signed server certificate is rejected rather than accepted.

Every subcommand supports `--host`/`--port`/`--database`/`--user`/`--password`/`--ssl-mode` (also settable via `PG_*` env vars or a TOML config file), `--log-level`, `--log-format` (`text`/`json`), and `--log-file`. Logging always writes to both the terminal and a log file — by default `$XDG_STATE_HOME/pg-partitioner/pg-partitioner.log` (or `~/.local/state/...` if that's unset), created automatically if missing.

## Project layout

```
src/            application source (see docs/project-structure.md for the module map)
tests/          integration tests + tests/fixtures/, a numbered zero-to-hero fixture catalog
docs/           design record — read docs/IMPLEMENTATION_GUIDE.md first
scripts/        partition_table.sh (env-var driven one-shot partitioning) +
                setup_test_env.sh / reset_test_env.sh for the disposable test container
CHANGELOG.md    what changed, per release
```

`docs/` and `tests/fixtures/` are gitignored — a local design record and fixture catalog, not
shipped content. The operational knowledge that matters for changing this code lives in comments
at the call sites it applies to (see `src/queries.rs` on relation-name quoting, and
`src/migration.rs` on parameter binding).

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

Working today, exercised end-to-end against a real PostgreSQL 16:

- **Read-only discovery** — `inspect`, `explain`, `export`, risk signals.
- **The existing-table strategy, for range** — `plan` → `apply`, the full ATTACH-first cutover,
  with drift detection and auto-registration.
- **The template strategy, for all three** — range (the period window, no DEFAULT), list (an empty
  parent, plus `add-partition` for adding value sets one at a time), and hash (the parent and its
  full bucket set).
- **Index recreation** — the template's indexes are rebuilt on a newly created parent, and a
  converted table's are rebuilt on the new parent.
- **Metadata and config** — `install`, `register`/`unregister`, and the `partitioner_logbook`
  audit trail every DDL action writes to.
- **One-shot partitioning from environment variables** — `scripts/partition_table.sh`, for
  driving all three strategies against a remote database without flags.
- **Premake maintenance** — `maintain` and the long-running `daemon`, for range tables from either
  migration. Hash and list registrations are recorded and reported, never maintained.

Not yet implemented, despite appearing in some older design notes:

- **Converting an existing table to list or hash.** Structural rather than missing — see
  [The two migrations](#the-two-migrations). Both are rejected at plan time.
- **Retention enforcement.** `--retention-type`/`--retention-value` are stored but nothing drops
  a partition yet.
- **Bulk-copy migration**, the fallback for tables that can't be attached as one range.
- **A DEFAULT partition for list tables** (hash cannot have one at all), and expression indexes,
  which are skipped during recreation because they can't be rebuilt from column names.

Treat anything in `docs/` claiming broader completeness as a design record rather than a
description of the shipped tool — several of those files predate the code and describe commands
that no longer exist.

## License

MIT

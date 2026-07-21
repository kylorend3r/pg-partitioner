# Index Management — Design

Detailed design for creating, dropping, and repairing indexes on partitioned tables. Ready for implementation; not yet built. Extends `project-structure.md` with a dedicated `index` module and command family.

## Why this needs its own design

Indexes on a partitioned table behave differently from a plain table in ways that directly cause outages if handled naively: a non-concurrent build takes an ACCESS EXCLUSIVE lock on every child in sequence; a concurrent build that gets interrupted partway leaves a child silently invalid while the parent reports invalid too; a unique index that omits the partition column is rejected by Postgres with an error that doesn't explain the rule; and once an index exists on the parent, every future partition inherits it automatically — which is useful, but means a mistake made once at creation time replicates into every partition created afterward. Each of these needs to be designed for up front rather than discovered in production.

## Command surface

Index operations are exposed as their own subcommands for ergonomics, but internally they compile to the same `PlanAction` list and flow through the same `plan → apply` pipeline as every other change — there is exactly one execution path in the codebase, not a fast/unsafe one for indexes and a careful one for everything else.

```
pg-partitioner index create --table <parent> --name <idx_name> --columns <col[,col...]>
    [--unique] [--using btree|gin|gist|brin|hash] [--include <col[,col...]>]
    [--where <predicate>] [--tablespace <name>] [--concurrently | --no-concurrently]
    [--if-not-exists] [--dry-run]

pg-partitioner index drop --table <parent> --name <idx_name>
    [--concurrently] [--cascade] [--dry-run]

pg-partitioner index status --table <parent> [--name <idx_name>]
    # per-child validity/size table; no writes

pg-partitioner index repair --table <parent> --name <idx_name>
    # rebuilds only the invalid child(ren) and re-attaches; safe to re-run
```

`--concurrently` defaults to on, since the minimum supported PostgreSQL version is 14 and there is rarely a good reason to take a blocking lock across every partition just to add an index.

## Types (`types.rs` additions)

```rust
pub struct IndexSpec {
    pub name: String,
    pub table: QualifiedName,
    pub columns: Vec<IndexColumn>,      // supports plain columns and expressions
    pub include: Vec<String>,
    pub method: IndexMethod,            // Btree | Gin | Gist | Brin | Hash
    pub unique: bool,
    pub predicate: Option<String>,      // partial index WHERE clause
    pub tablespace: Option<String>,
    pub storage_params: Vec<(String, String)>,
    pub concurrently: bool,
    pub if_not_exists: bool,
}

pub struct IndexBuildStatus {
    pub relation: QualifiedName,        // parent or a specific child
    pub index_name: String,
    pub indisvalid: bool,
    pub indisready: bool,
    pub size_bytes: i64,
}

pub enum PlanAction {
    // ...existing variants (CreateChild, DetachChild, Backfill, ...)
    CreateIndex(IndexSpec),
    DropIndex { table: QualifiedName, name: String, concurrently: bool },
    RepairIndex { table: QualifiedName, child: QualifiedName, name: String },
}

pub enum DoctorFinding {
    // ...existing variants
    InvalidPartitionIndex { table: QualifiedName, child: QualifiedName, index: String },
    UnusablePartialUniqueIndex { table: QualifiedName, index: String, reason: String },
}
```

## Validation (runs at `plan` time, before anything is written)

- **Unique/PK must include the partition column.** This is a hard PostgreSQL rule, not a preference — `plan` rejects the request immediately with an explanation (which column is missing and why) rather than letting the database error out unexplained. It also notes explicitly that even a compliant unique index only enforces uniqueness *within* each partition, never across the whole set, so the user isn't left assuming they got a guarantee Postgres doesn't provide.
- **Name-length collision check.** The same 63-byte identifier-truncation logic already planned for partition/table naming (`validation.rs`) applies to index names too — an index name that's fine on its own can collide once Postgres has to derive per-child names internally for supporting objects (e.g. the implicit constraint backing a unique index).
- **`CONCURRENTLY` cannot run inside a transaction block.** The orchestrator must special-case `CreateIndex`/`DropIndex` actions with `concurrently: true` so they are never batched into the same transaction as other plan actions — each runs as its own top-level statement.
- **Access method sanity per partition strategy.** Flag (don't silently allow) combinations that are legal but almost certainly a mistake, e.g. a `hash` index requested as the sole means of enforcing uniqueness (hash indexes don't support uniqueness the way btree does in all versions/configs) — surfaced as a warning in the plan output, not a hard block.

## Execution paths

Two build strategies, chosen automatically by partition count (overridable):

**Default — single statement, small-to-medium partition counts.** `CREATE INDEX CONCURRENTLY <name> ON <parent> (...)`. Postgres recurses across every existing child and registers the index so any future child (created by `maintain`'s premake or a manual `plan`/`apply`) gets it automatically at creation time — no extra bookkeeping needed on the tool's side for that part. Simple and correct; the only downside is children are built one at a time internally by Postgres.

**Opt-in — decomposed/parallel, large partition counts (`--parallel N`).** For partition sets large enough that sequential per-child builds are a real wait, decompose into the standard manual recipe: `CREATE INDEX CONCURRENTLY ON ONLY <child> (...)` for each child (run with up to N in flight at once), then `CREATE INDEX ON ONLY <parent> (...)` (fast — the parent itself holds no rows), then `ALTER INDEX <parent_idx> ATTACH PARTITION <child_idx>` per child once each child's build finishes. The parent index becomes valid automatically once every child is attached. This path is more moving parts, so it stays opt-in rather than default.

## Post-build verification and repair

After either path, the tool queries `pg_index` joined through `pg_partition_tree`/`pg_inherits` for `indisvalid`/`indisready` on the parent and every child. Any child left invalid (interrupted build, crash, cancellation) is recorded as `DoctorFinding::InvalidPartitionIndex` — surfaced immediately by `index create` itself, and again by any later `doctor` run so it's never silently forgotten.

Repair (`index repair` or `doctor`'s suggested fix) is narrow and targeted: drop just the invalid child's index, `CREATE INDEX CONCURRENTLY` on that one child, `ALTER INDEX ... ATTACH PARTITION`. This is the same class of problem the sibling `pg-reindexer` tool already solves (REINDEX CONCURRENTLY orchestration, retry/backoff, replica-lag awareness) — rather than reimplementing that machinery, `index repair` should either shell out to `pg-reindexer` or port its retry/backoff/replica-lag logic directly, and `doctor`'s output should say so explicitly when recommending a fix (`"run pg-reindexer against <child> to repair index <name>"`).

## Interaction with the rest of the tool

- **Initial partitioning (`plan`/`apply` converting a plain table).** Existing indexes/constraints on the original table are read from catalog and translated into `IndexSpec` entries as part of the same plan that creates the shadow partitioned table — this isn't a separate feature at conversion time, it reuses this exact machinery.
- **`maintain`'s premake step.** No special handling needed for propagation (Postgres does it), but immediately after creating a new child, `maintain` runs the cheap `indisvalid` catalog check on just that child rather than waiting for the next `doctor` run to notice a problem.
- **Populated-table migration with a unique index.** If a unique index is being added as part of migrating a table that already has data, the batched copy can hit a genuine duplicate-key violation. That's a real data problem, not a tool bug — `apply` should fail loudly on the specific batch/rows involved rather than swallowing or retrying past it.
- **`DROP INDEX CONCURRENTLY` on a partitioned index** is supported PG14+ and recurses to children the same way creation does — worth an explicit test rather than assuming symmetry with create, since drop/create paths have historically not always gotten identical treatment across Postgres versions.

## Testing plan (see also `test_environment.md`)

- Index created on parent recurses correctly to all existing children.
- A partition created *after* the index exists automatically receives it, valid, with no manual step.
- An interrupted/cancelled concurrent build leaves exactly the expected child invalid, is detected by the post-build check, and `index repair` fixes it without touching the other children.
- A unique index request omitting the partition column is rejected at `plan` time with a clear message, before any DDL runs.
- A long table name plus a long requested index name collide under the 63-byte limit and are caught before execution, not after.
- `index drop --concurrently` removes the index from parent and every child cleanly.

# Test Environment — Fixture Catalog

A disposable, scriptable PostgreSQL instance loaded with a numbered "zero to hero" fixture catalog, so every new feature can be developed and tested against a progression from the trivial case up to the hardest known edge case — not just unit-tested in isolation. Mirrors the conventions already established in the sibling `pg-reindexer` project's test container (`docs/TEST_DATABASE.md`), adjusted for this tool's own port/name so both containers can run side by side.

## Container identity

| Field | Value |
|---|---|
| Container name | `pg-partitioner-test` |
| Image | `postgres:16` (match CI's pinned major version) |
| Port mapping | host `5434` → container `5432` (pg-reindexer already uses `5433`; `5434` avoids collision so both test containers can run at once) |
| Database | `partitioner_testdb` |
| User | `partitioner_test` |
| Password | `test123` |
| Data volume | none (anonymous, ephemeral — destroyed with the container, re-provisioned from the fixture catalog on demand) |

```bash
export PG_HOST=localhost
export PG_PORT=5434
export PG_DATABASE=partitioner_testdb
export PG_USER=partitioner_test
export PG_PASSWORD=test123
```

## Provisioning

`scripts/setup_test_env.sh` (see `Scaffolding` below) does the following, in order:

1. `docker run` a fresh `pg-partitioner-test` container (no persistent volume — same disposable philosophy as `pg-reindexer-test`).
2. Wait for `pg_isready`.
3. Load `tests/fixtures/*.sql` in numeric order.
4. Optionally (`--with-pg-partman`), install `pg_partman` alongside so the same scenario can be run through both tools and diffed — useful during development to sanity-check that this tool's output/behavior matches or deliberately improves on the competitor's for a given case.

`scripts/reset_test_env.sh` tears the container down (`docker stop && docker rm`) and re-runs setup — the only supported way to get back to a known-clean state, matching the "re-provision, don't hand-patch" philosophy already used for `pg-reindexer-test`.

Profiles:
- `--profile minimal` loads only `00`–`03` (empty/small/large populated tables) — fast inner loop while iterating on core plan/apply logic.
- `--profile full` (default for CI) loads the entire catalog.

## Fixture catalog ("zero to hero")

Numbered so complexity and risk increase monotonically; each fixture is scoped to specific tool capability so a contributor building a feature knows exactly which fixture(s) to develop against.

| # | Fixture | Scenario | Exercises |
|---|---|---|---|
| — | *(no file — CLI-only)* | Table doesn't exist yet at all | Greenfield `CREATE TABLE ... PARTITION BY` path, no shadow/swap needed |
| 01 | `01_empty_existing_table.sql` | Plain table, zero rows | Fast rename-swap conversion path (no data to copy) |
| 02 | `02_small_populated_table.sql` | Plain table, ~5k rows across several days | End-to-end ATTACH-based cutover conversion (default path, see `migration_design.md`) at a scale where a mistake is easy to spot |
| 03 | `03_large_populated_table.sql` | Plain table, ~750k rows across a wide date range (`generate_series`) | Cutover lock-duration timing under real data volume; fallback bulk-copy resumability (kill mid-copy, confirm checkpoint resume) when ATTACH is deliberately bypassed for testing that path |
| 04 | `04_already_partitioned_range_time.sql` | Native range partition set by month, not created by this tool | `inspect`/`explain`/`import`/`doctor` against a pre-existing, unregistered setup |
| 05 | `05_already_partitioned_list.sql` | Native list partition set by low-cardinality column (tenant/region) | List-strategy discovery and registration |
| 06 | `06_default_partition_with_strays.sql` | Partition set whose default already holds out-of-range rows (both older-than-earliest-child and newer-than-premake) | Risk-signal detection, `reconcile-default` |
| 07 | `07_naming_collision_long_names.sql` | Two long, similar table names whose child suffixes collide under the 63-byte identifier limit | Naming-collision validation (tables *and* indexes) |
| 08 | `08_unique_constraint_cases.sql` | One partition set with a compliant unique index (includes partition key) and a queued request for a non-compliant one | `plan`-time rejection of unique indexes missing the partition column; global-uniqueness advisory |
| 09 | `09_index_repair_scenario.sh` (script, not pure SQL) | A `CREATE INDEX CONCURRENTLY` deliberately cancelled partway via a killed session | Invalid-child-index detection, `index repair` |
| 10 | `10_subpartitioned_time_then_id.sql` | Two-level subpartition set (time → id) | *Phase 3 placeholder* — subpartitioning guidance |
| 11 | `11_foreign_keys_and_dependents.sql` | Table referenced by FKs from other tables, plus a dependent view | Cutover/rename safety checks — what must be detected and warned about before swapping the table out from under its dependents |
| 12 | `12_timezone_dst_boundary.sql` | Rows and session timezone arranged around a DST transition, server in UTC / client not | Timezone-mismatch warning at `plan` time |
| 13 | `13_high_partition_count.sql` | Partition set with a large, generated child count | Planner-risk detection, `max_locks_per_transaction` headroom warning (sized to warn without actually exhausting CI's lock table) |
| 14 | `14_hash_partitioned_table.sql` | Hash-partitioned table | *Phase 3 placeholder* — reserved now so the catalog doesn't need renumbering later |

Fixtures 10 and 14 are intentionally thin placeholders (a comment block describing intent, table skeleton, no full scenario yet) since Hash and subpartitioning guidance are Phase 3 per `roadmap.md` — they exist now so adding the real scenario later doesn't require renumbering everything else.

## Running tests against it

**Manual CLI testing**, once the binary exists:
```bash
export PG_HOST=localhost PG_PORT=5434 PG_DATABASE=partitioner_testdb PG_USER=partitioner_test PG_PASSWORD=test123
./target/release/pg-partitioner inspect --table public.table_02_small_populated
```

**Automated live-DB suite** (`tests/db_validation.rs`, marked `#[ignore]` so plain `cargo test` skips it):
```bash
cargo test --test db_validation -- --ignored --test-threads=1
```
`--test-threads=1` for the same reason as `pg-reindexer`: these tests share one database and interfere with each other under parallel execution.

## Relationship to `pg-reindexer`'s test container

Deliberately kept as a separate container (different name/port) rather than reusing `pg-reindexer-test`, even though that container already has a partition-heavy stress schema (`transaction_log`, `event_stream`) — this tool's fixtures need to be created *before* partitioning (plain tables that get converted) as well as already-partitioned, and need full control over provisioning/teardown timing independent of whatever state `pg-reindexer`'s tests have left behind. Both can run simultaneously without conflict.

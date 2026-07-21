# Implementation Guide — pg-partitioner

Entry point for whoever implements this tool. Read this in full before writing any code or opening the other docs — it says what order to read them in, what's already decided, and what order to build in. Everything referenced here already exists in `docs/`; nothing in this guide is new design, it's a map and a set of guardrails for moving fast without relitigating settled decisions.

## One-line goal

A standalone Rust CLI giving PostgreSQL operators the operational knowledge and safety pg_partman's SQL-only interface doesn't provide: plan-before-apply, proactive guardrails against known footguns, and guided setup for native declarative partitioning — not a reimplementation of pg_partman's core automation, which is already good.

## Reading order

1. **`pg_partman_analysis.md`** (5 min) — why this tool exists, what it deliberately doesn't try to redo.
2. **`roadmap.md`** — the phase sequence and exit criteria being built toward.
3. **`project-structure.md`** — the module map and file layout. Treat as authoritative for where new code goes; don't reorganize without strong reason, and if you do, update this doc to match rather than letting it drift.
4. **`migration_design.md`, `index_management.md`, `locking_and_retries.md`** — read whichever is relevant to the module currently being built, not all three up front.
5. **`test_environment.md`** — how to stand up a real Postgres to test against. Do this *before* writing any code that touches a database.
6. **`desired_features.md`** — reference only, for the reasoning behind an item in `project-structure.md` that isn't self-explanatory.

## Non-negotiable design decisions

These came out of real back-and-forth design work, not a first guess. Don't spend time reconsidering them without a concrete reason — and if one turns out to be wrong, fix the doc it lives in rather than silently diverging from it in code.

- **PostgreSQL 14+ only.** No trigger-based partitioning, no support shims for earlier versions.
- **Cutover-first migration, not bulk-copy-first.** Converting a populated table attaches it as a partition almost immediately (pre-validated `CHECK` constraint + `ATTACH`), making the new table the sole write target within one short transaction. Bulk-copy-then-swap exists only as a fallback for the rare case direct attach isn't possible, and carries a named correctness risk for tables with update activity on existing rows. Full mechanics in `migration_design.md` — don't default to the copy-first approach.
- **Every `PlanAction` executes through `retry.rs`.** No DDL call bypasses the `SET LOCAL lock_timeout/statement_timeout` + backoff-retry wrapper, in `orchestrator.rs`, `migration.rs`, `index.rs`, or anywhere else. One execution path, not a careful one for some actions and a bare one for others.
- **`queries.rs` is the single source of SQL text**, shared between real execution and dry-run/plan preview, so the two can never drift apart from each other (same pattern as `pg-reindexer`'s `index_operations.rs`).
- **Plan/apply drift detection is mandatory.** `apply` refuses to run if the live schema's checksum doesn't match what `plan` computed — never optional, never a warning-only path.
- **Destructive-action confirmation shows real blast radius** (row counts, table names) — never a boolean/string flag as the sole gate.
- **Default partitions are actively reconciled, not just monitored.** `reconcile-default` moves stray rows to where they belong; polling alone was explicitly called out as insufficient relative to pg_partman.
- **`maintain` isolates per-table failures.** One table failing to acquire a lock or hitting an error must not stop the sweep for the rest of the fleet.
- **No metrics endpoint or alerting integrations in this version of the roadmap.** These were deliberately cut by the user from Phase 3 — don't add `metrics.rs`/`alerts.rs` back in without being asked again.
- **Match `pg-reindexer`'s conventions wherever there's no strong reason to differ** — dependency choices, config precedence order, test file naming, the disposable test-container philosophy. When in doubt, check what the sibling tool does before inventing something new.

## Build order

Build each phase's modules, verify against the fixtures named, then move on. Don't jump ahead to a later phase's modules because they seem related — the interfaces from earlier phases need to be real and tested first, or later work ends up rebuilt against a moving target.

### Phase 0 — build first

**Modules:** `connection.rs`, `credentials.rs`, `config.rs`, `schema.rs`, `queries.rs` (read-only queries only at this stage), `types.rs` (core types), `risk.rs`, `inspect.rs`, `explain.rs`, `logging.rs` (basic), `main.rs`/`lib.rs` skeleton.

**Verify against:** fixtures 04 and 05 (already-partitioned, not created by this tool) — `inspect`/`explain` must describe them accurately without ever opening a write transaction. Fixture 06 — risk-signal detection must flag the stray default-partition rows.


**Done when:** `pg-partitioner inspect --table <fixture table>` produces an accurate report against every non-placeholder fixture, and no fixture's data changes as a result of running it.

### Phase 1

**Modules:** `validation.rs`, `plan.rs`, `apply.rs`, `orchestrator.rs`, `retry.rs`, `migration.rs` (ATTACH-based cutover path — read `migration_design.md` in full before writing this one), `retention.rs`, `state.rs`, `save.rs`, `wizard.rs`, `maintain.rs`, `index.rs` (carryover-during-conversion path only; the ad-hoc `index create/drop/repair` commands are Phase 2), `export.rs`.

**Verify against:** fixtures 01/02/03 (empty/small/large populated tables — the ATTACH cutover path is the one being tested, not a bulk-copy path), fixture 11 (FK/view dependents survive the rename-swap cutover), fixture 07 (naming collision rejected at `plan` time, before any DDL runs), fixture 08 (non-compliant unique index rejected at `plan` time with a clear message).

**Done when:** a plain, populated table at fixture 03's scale converts to a working, registered, maintained partition set end-to-end using only the CLI — no direct SQL — and `pg-partitioner maintain` with no arguments correctly premakes/retires across everything registered so far.

### Phase 2

**Modules:** `doctor.rs` (full), the ad-hoc `index.rs` commands, `repartition.rs`, `schema_change.rs`, `tenant.rs`, the full timeout/backoff coverage from `locking_and_retries.md` across every action, and the proactive-warning checks in `validation.rs`.

**Verify against:** fixture 09 (index-repair scenario, run manually per its own script), fixture 12 (timezone-mismatch warning), fixture 13 (lock-budget/planner-risk warning).

**Done when:** the tool actively prevents — not just documents — the three incident classes named in `roadmap.md`'s Phase 2 exit criterion (lock exhaustion, timezone-driven boundary errors, silent naming collisions).

### Phase 3 and later

Don't start `fleet.rs`, `daemon.rs`, `advisor.rs`, or `blue_green.rs` until Phase 2's exit criterion is actually met against the fixtures. These depend on Phase 0–2's interfaces being settled; building them early "for efficiency" tends to produce rework once those interfaces inevitably shift during Phase 0–2 development.

## Working practices for speed without rework

- Stand up the test container (`scripts/setup_test_env.sh --profile minimal`) before writing the first line of `schema.rs` — develop against real Postgres from the start, not mocks. Switch to `--profile full` once Phase 1 modules need the rest of the fixture catalog.
- Land Phase 0 as a real, runnable binary before writing any Phase 1 code. `inspect`/`explain` are useful standalone and de-risk the connection/catalog-query plumbing everything later depends on.
- If a design question comes up that isn't answered anywhere in `docs/`, don't guess silently and move on — every existing doc here was built through explicit question-and-answer, not first drafts. Add an `## Open question` note to the most relevant doc and flag it, rather than picking an answer that might quietly contradict something decided elsewhere.
- Keep `docs/project-structure.md` in sync as the actual source of truth once code exists — if implementation reveals the module boundaries need to shift, update that doc in the same change, not after the fact.

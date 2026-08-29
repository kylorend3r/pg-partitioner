# Development guide — pg-partitioner

Read this before developing a feature in this repository.

---

## 1. Workflow

### Before you start

1. **Read this file first.** Then read any design doc named in the request (`docs/*.md`).
2. **Check `docs/` claims against the code before trusting them.** Several documents in `docs/`
   were written ahead of the code and describe commands that were never built or have since been
   removed. `git log` and the actual `Commands` enum in `src/main.rs` are the authority on what
   exists. This has burned us more than once.
3. Design docs list **open questions**. Take the doc's own recommendation where it gives one; ask
   only when two readings would produce materially different work.

### Before you commit

1. **Ask whether a full verification run is wanted**, and say what each option costs:
   - *quick* — `cargo build && cargo test` (seconds)
   - *full* — the above plus the live-database run in §4 (a minute or two, needs Docker)

   Don't assume. Ask, and wait for the answer.
2. Update `CHANGELOG.md` — see §5.
3. Update `README.md` and `docs/project-structure.md` if the change touched anything they
   describe — see §6.
4. Remove stray files the work generated (plan files, scratch SQL, temp scripts). Scratch work
   belongs in the session scratchpad, never in the repo.

### Committing and pushing

- **Never commit or push to `main`/`master` directly.** Branch first:
  `git checkout -b <short-descriptive-name>`.
- **No co-author or session trailers in commit messages.** No `Co-Authored-By:`, no
  `Claude-Session:`, no `Generated with…` footer. Just the message.
- Write the subject as what the change does, in the imperative: `Add list partitioning support`,
  not `Added…` or `feat:`.
- Commit only when asked. Don't commit as a reflex at the end of a task.

---

## 2. Rust rules

### Errors

- **`anyhow::Result` in application code**, with `.context("what was being attempted")` at every
  boundary where the error would otherwise be unattributable. Errors surface to a CLI user who
  cannot read a backtrace.
- **No `unwrap()` / `expect()` / `panic!` outside `#[cfg(test)]`.** A panic in a CLI that is
  mid-way through DDL is the worst possible failure mode. Return an error and let the caller
  decide.
- **Never swallow an error from a query.** `.unwrap_or_default()`, `.unwrap_or(0)`, and `.ok()`
  around a database call turn a broken query into a plausible-looking zero, which then gets
  reported to the user as fact. This has produced four separate bugs in this codebase, each
  invisible for months. If a value is genuinely optional, model it as `Option` and say so; if it
  isn't, propagate the error.

### Types and data modelling

- **New `MigrationConfig` / `Plan` fields get `#[serde(default)]`**, always. Plan files are
  written to disk and re-read by a later `apply`, possibly by a different binary version. A
  non-defaulted new field breaks every plan file that predates it.
- **Prefer a `match` over `if let` chains on enums**, so adding a variant becomes a compile error
  rather than a silent fallthrough. A non-exhaustive match has already shipped a broken build here.
- Keep newtypes and small enums (`PartitionStrategy`, `RetentionType`) rather than passing bare
  strings between layers. Conversions belong on the type (`as_sql_keyword`, `from_partstrat`,
  `as_registration_str`), not scattered at call sites.

### Style

- `cargo fmt` and `cargo clippy` before committing. Don't introduce new warnings; the existing
  backlog is pre-existing and out of scope unless you're touching that code.
- **Comments explain *why*, never *what*.** The surrounding code already says what. A comment
  earns its place by recording a constraint, a trade-off, or a trap that the next reader would
  otherwise rediscover the hard way. Match the density of the file you're in.
- Function names spell out the domain (`create_partitioned_table_from_template`, not
  `create_table2`). This codebase prefers long, unambiguous names.

---

## 3. PostgreSQL rules

These are specific to this codebase and every one of them is a bug we actually shipped.

### Never bind a Rust string against a parameter carrying a non-text cast

An explicit `$N::sometype` cast is what Postgres's DESCRIBE step uses to infer that parameter's
type. So `$1::regclass` reports the parameter as `regclass`, `$1::interval` as `interval` — and
tokio-postgres's `ToSql` for `&str`/`String` accepts only text-family OIDs. It fails at runtime,
never at compile time. Confirmed with:

```sql
PREPARE p AS SELECT … WHERE relid = $1::regclass;
SELECT parameter_types FROM pg_prepared_statements WHERE name = 'p';   -- {regclass}
```

Two fixes, in order of preference:

1. **`$1::text::regclass`** — the inner cast pins the parameter to text and Postgres does the
   lookup afterwards. Keeps parameter binding, which is the safer default.
2. **Interpolate with a quoting helper** when the query is already built with `format!` (the
   convention for all DDL here).

This has bitten us with `::jsonb`, `::timestamp`, `::interval`, and `::regclass`.

### Use the right quoting helper (`src/queries.rs`)

| Helper | Produces | Use for |
|---|---|---|
| `quote_ident("events")` | `"events"` | An identifier: a table, column, or index name |
| `quote_literal("eu-west")` | `'eu-west'` | A string value |
| `quote_regclass_literal("public.events")` | `'"public"."events"'` | A relation name in a `::regclass` position |

`quote_ident` on a **dotted** name is the trap: `quote_ident("public.events")` gives
`"public.events"` — one quoted identifier *containing a dot* — so `"public.events"::regclass`
parses as a cast of a column reference and fails with `column "public.events" does not exist`.

### `array_agg` over zero rows returns NULL, not `{}`

And a NULL column deserializes into `Vec<String>` by **panicking**. Always
`COALESCE(array_agg(…), ARRAY[]::name[])`. Zero-row aggregates are ordinary: a table with no
constraints, a partition set with no children.

### Declarative partitioning facts worth knowing

- A partition's bound lives in **`pg_class.relpartbound`**, read via
  `pg_get_expr(c.relpartbound, c.oid)`. It is *not* a `pg_constraint` row. Any logic testing
  `contype = 'c'` to identify or describe a declarative partition is wrong — no declarative child
  has a CHECK constraint, so such a test matches everything or nothing.
- The DEFAULT partition is the one whose bound renders exactly `DEFAULT`.
- `pg_partitioned_table.partstrat` is `"char"`; select it as `partstrat::text` or reading it
  panics.
- `pg_total_relation_size` on a partitioned **parent** returns 0 — it excludes children by design.
- Unique indexes on a partitioned table must include every partition-key column. Ordinary OLTP
  tables (surrogate `id` PK) therefore cannot carry their PK onto the parent. Warn, skip the
  index, and leave it enforced on the legacy child; don't block.
- `PARTITION BY LIST` accepts exactly one column. RANGE and HASH accept composite keys.
- A hash-partitioned table cannot have a DEFAULT partition. This is why hash can't use the
  cutover flow at all.

### DDL discipline

- Every DDL statement goes through `retry.rs` (`lock_timeout`/`statement_timeout` + backoff). No
  bare, unguarded locks — see `docs/locking_and_retries.md`.
- Make DDL **idempotent** (`IF NOT EXISTS`, `DROP … IF EXISTS` first) so a retried `apply` after a
  mid-sequence failure is safe.
- `SET LOCAL` only lasts for its transaction. Inside a literal `BEGIN`/`COMMIT` batch it must be
  *in* that batch.

---

## 4. Testing

`cargo test` covers pure logic only — parsing, name building, validation shape. It cannot catch
anything above, because none of it fails until a real server sees it.

**The recurring failure mode in this codebase is code that looks finished and was never run.**
Verify by running against a live database, not by reading.

```bash
./scripts/setup_test_env.sh --profile=minimal        # disposable Postgres 16 on :5434
export PG_HOST=localhost PG_PORT=5434 PG_DATABASE=partitioner_testdb \
       PG_USER=partitioner_test PG_PASSWORD=test123

cargo build
./target/debug/pg-partitioner install
# …then exercise the paths you touched, including the failure paths
./target/debug/pg-partitioner inspect
```

Notes:

- The container ships `max_locks_per_transaction = 64`; `plan` validation requires ≥ 256. Raise it
  with `ALTER SYSTEM` and restart the container.
- **The three primary fixtures generate data up to `now()`, which makes them impossible to cut
  over.** The bounding CHECK is `key < date_trunc(unit, start_date)`, so every existing row must
  predate `--start-date`, and `--start-date` may not be in the future. Build a table with
  historical-only data to test the cutover path with real rows.
- Exercise error paths, not just the happy one. Most of this guide exists because a failure path
  was never run.
- `scripts/test_cli_commands.sh` is currently stale — it calls a removed command and asserts
  behavior that changed. Don't trust its pass/fail.

---

## 5. Changelog

`CHANGELOG.md` follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and
[Semantic Versioning](https://semver.org/).

- Add an entry under `## [Unreleased]` as part of the change itself, not afterwards.
- Sections: `Added`, `Changed`, `Deprecated`, `Removed`, `Fixed`, `Security`.
- Write for someone operating the tool, not for someone reading the diff. Name the user-visible
  symptom, then the cause: *"`inspect` reported `Children: 0` for every partitioned table"* beats
  *"fixed quote_ident call"*.
- A bug fix that changes behavior in a way an operator would notice belongs in `Fixed` **and**
  deserves a sentence saying what they'll see differently now.

---

## 6. Documentation

Update these when the change warrants it — as part of the change, not later:

| File | Update when |
|---|---|
| `CHANGELOG.md` | Always (§5) |
| `README.md` | A command is added/changed/removed, a flag changes, or a "Status" claim becomes false |
| `docs/project-structure.md` | A module gains a responsibility, or the plan/apply data flow changes |
| `CLAUDE.md` | You learn a rule the next person would otherwise rediscover by breaking something |

Two standing rules:

- **Never document a command without running it.** Every command in the README should have been
  executed verbatim against the test database before being written down.
- **Don't let a doc overclaim.** If a section says something is implemented and it is a stub, fix
  the section in the same change. That is exactly how `docs/` became untrustworthy.

Note that `docs/` and `tests/fixtures/` are **gitignored** — they're a local design record, not
shipped content. `README.md`, `CHANGELOG.md`, and this file are tracked and are what a reader
outside this machine actually sees.

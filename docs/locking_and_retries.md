# Lock, Statement, and Deadlock Timeouts — Design

Every DDL action the tool runs (`CreateChild`, `DetachChild`, `CreateIndex`, `DropIndex`, the cutover rename, `ATTACH`/`DETACH PARTITION`) is a lock-acquiring operation running against a live, possibly high-traffic database. Without explicit timeouts, a single blocked statement can hang indefinitely — and worse, while it waits, it sits in the lock queue ahead of ordinary application queries, blocking them too even though it never itself acquires anything. This is the single most common way partition maintenance turns into an incident, and it's exactly the kind of operational knowledge this tool is meant to encode rather than leave to the operator. Ready for implementation; not yet built.

## The three settings and what each actually does

- **`lock_timeout`** — how long a statement will wait to *acquire* a lock before giving up. This is the primary knob: it turns "hang forever" into "fail fast with a clear, specific error" (SQLSTATE `55P03`, lock_not_available). Every DDL action the tool issues should run with a short `lock_timeout` set for that session.
- **`statement_timeout`** — a hard ceiling on total statement execution time, lock-wait included. Coarser than `lock_timeout`: it doesn't distinguish "waiting for a lock" from "the statement itself is just slow" (e.g. a big `ANALYZE` or an unexpectedly large batch). Still worth setting as a backstop, but `lock_timeout` is what should trip first in the lock-contention case this is really about.
- **`deadlock_timeout`** — how long Postgres waits before it bothers checking whether a blocked session is part of a deadlock cycle (default 1s). Deadlock detection itself has a real cost (building the wait-for graph), which is why Postgres doesn't do it on every lock wait — but a maintenance operation that takes an ACCESS EXCLUSIVE lock on a parent table can absolutely deadlock against application traffic taking locks in the opposite order. The tool shouldn't need to change this from Postgres's default in most cases, but should be aware it's in play and treat the resulting error (`40P01`, deadlock_detected) as a distinct, retryable case rather than a hard failure — and log it more loudly than a plain lock-timeout, since a real deadlock indicates actual conflicting access patterns worth knowing about, not just transient contention.

## Where these get set

Per-action, via `SET LOCAL` at the start of the transaction that runs a given `PlanAction` — not as global server or role defaults, since the right value depends on what kind of action is running:

| Action class | `lock_timeout` | `statement_timeout` | Rationale |
|---|---|---|---|
| Fast DDL (`CreateChild`, `DetachChild`, cutover rename, `ATTACH`/`DETACH PARTITION`) | short (default 3s, configurable) | short-to-moderate (default 30s) | These should be near-instant under normal conditions; if they can't get their lock quickly, failing fast and retrying shortly after is far safer than queueing behind long-running app traffic and blocking everything behind the tool in turn. |
| Concurrent builds (`CREATE INDEX CONCURRENTLY`, batched backfill/migration) | short (same default) — these mostly only take brief locks at start/end, not for the duration of the build | long or disabled — the build itself is expected to run long | The long-running part of a concurrent build isn't held under a blocking lock, so it doesn't need a long `lock_timeout`; it does need `statement_timeout` out of the way so the legitimate long-running work isn't cancelled. |
| `maintain` sweep (per table) | short (same default) | moderate | Consistent with the per-table isolation already designed in `project-structure.md` — one table failing to get its lock quickly shouldn't hold up the rest of the sweep. |

All three defaults are configurable (CLI flag, TOML, or per-registration override in `partitioner_registrations` for tables with known contention patterns), and `plan`'s output should show the effective values a given action will run with — so "this step will use lock_timeout=3s, up to 5 retries" is visible before `apply` runs, not just something happening silently underneath.

## Retry policy

```rust
pub struct RetryPolicy {
    pub lock_timeout: Duration,
    pub statement_timeout: Duration,
    pub max_attempts: u32,        // default 5
    pub backoff_base: Duration,   // default 200ms
    pub backoff_max: Duration,    // default 10s
    pub jitter: bool,             // default true
}
```

`retry.rs` (new module) provides the single execution path every action goes through:

1. Set `lock_timeout`/`statement_timeout` via `SET LOCAL` for the current transaction.
2. Run the statement.
3. On success, return.
4. On SQLSTATE `55P03` (lock_not_available) or `40P01` (deadlock_detected), back off (exponential, with jitter, capped at `backoff_max`) and retry, up to `max_attempts`. Deadlocks are logged at a higher severity than plain lock timeouts even though both are retried the same way, since a deadlock means the tool's DDL and application traffic are actually taking conflicting lock orders — worth surfacing, not just quietly retrying into invisibility.
5. On any other error (constraint violation, disk full, connection loss), fail immediately — these are not transient contention and retrying them would just delay a real problem.
6. Every attempt (success, retry, or final failure) is written to `partitioner_logbook`, so a slow `apply` caused by repeated lock contention is visible in the audit trail rather than looking like it silently took longer for no reason.

This mirrors the pattern `pg-reindexer` already uses for replica-lag backoff (`orchestrator.rs` — workers sleep and recheck rather than proceeding or hard-failing), applied here to lock contention instead of replication lag.

## Interaction with the rest of the tool

- **`orchestrator.rs`** calls every `PlanAction` through `retry.rs` rather than executing DDL directly — one execution path, not a careful one for some actions and a bare one for others.
- **`validation.rs`** surfaces the configured policy in `plan`'s preflight output.
- **`maintain.rs`** relies on this directly for the per-table isolation it already needs: a table that can't get its lock inside the timeout is retried a bounded number of times, then marked failed for this sweep and logged, rather than blocking the rest of the fleet.
- **`index.rs`** uses a longer-`statement_timeout` variant of the same policy for concurrent builds, per the table above.

## Testing plan

- A blocked lock (held by a concurrent session for longer than `lock_timeout`) causes the expected number of retries with visibly increasing backoff, then either succeeds once the blocker releases or fails cleanly after `max_attempts`.
- A deliberately constructed deadlock between two sessions is detected within `deadlock_timeout` and retried, with a distinct, higher-severity log entry compared to a plain lock timeout.
- A non-retryable error (e.g. a unique violation during migration) fails immediately on the first attempt, with no retry delay.
- `partitioner_logbook` contains an entry for every attempt, not just the final outcome.

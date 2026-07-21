# Migration Design — Converting an Existing Table to Partitioned

Detailed design for `migration.rs` (and `repartition.rs`, Phase 2, which reuses this same machinery). This corrects and formalizes the naive "always batch-copy" framing implied elsewhere — the default path here is cutover-first via `ATTACH`, not bulk copy. Ready for implementation; not yet built.

## Three starting cases

- **Table doesn't exist yet.** No migration at all: `CREATE TABLE ... PARTITION BY` directly.
- **Table exists, zero rows.** Fast rename-swap (see below), no data-safety concerns since there's nothing to preserve.
- **Table exists, populated.** The real case this doc covers.

## Default path for a populated table: cutover-first via `ATTACH`

`ALTER TABLE parent ATTACH PARTITION existing_table FOR VALUES FROM (...) TO (...)` works on a table that already has data — Postgres just validates that every row satisfies the bounds. That validation is normally a scan under a heavy lock, but it can be skipped: add a `CHECK` constraint expressing the bounds as `NOT VALID`, then `VALIDATE CONSTRAINT` separately (a scan, but under `SHARE UPDATE EXCLUSIVE`, which doesn't block ordinary reads/writes). Once validated, `ATTACH` recognizes the constraint already proves the bounds and skips its own scan.

Sequence:
1. Add and validate the bounding `CHECK` constraint on the existing table (non-blocking scan).
2. Create the new partitioned parent under a temporary name — empty, instant.
3. **One short transaction:** rename the existing table aside (`orders` → `orders_legacy`), rename the new parent into the real name (`orders_new` → `orders`), then `ATTACH PARTITION orders_legacy` (fast, thanks to step 1).
4. Create forward-looking partitions (this period, premake N ahead) as ordinary empty `CREATE TABLE ... PARTITION OF` — instant.
5. Recreate indexes not already covered by inheritance (see `index_management.md`), reapply grants/ownership.

No batched copying is needed for the bulk of existing data — it becomes the first (legacy) partition in place. Everything after cutover (splitting that legacy chunk into finer periods) is optional and deferred; see below.

### Why the cutover step must happen essentially immediately, not after a long copy

Step 3's transaction takes an ACCESS EXCLUSIVE lock, but only for the duration of two renames and one pre-validated attach — normally well under a second. Concurrent writes behave predictably around it:

- Writes already holding a lock on the table when cutover's transaction requests its lock finish normally first (Postgres won't let the exclusive request cut in front of writes already in progress).
- New write attempts that arrive once cutover's request is queued block behind it — Postgres's lock queue is fair, so they can't jump ahead and starve the cutover transaction, but they also don't get served out of order.
- The moment cutover commits, every queued write resumes, re-resolves the table name, and finds the new partitioned structure — routed to the correct child automatically. Nothing fails, nothing is lost, no client-side handling needed.

This is only safe because cutover happens **immediately**, before any lengthy data movement. If instead the tool bulk-copied data out of the old table over an extended window and only swapped at the end (classic ETL ordering), any `UPDATE` the application issues against an older row *during that window* would be running against the old table while the migration might already be moving that same row underneath it — a real correctness gap for any table where existing rows can be updated (most OLTP tables), not a hypothetical. Cutover-first eliminates this because there is only ever one authoritative write target, established almost immediately.

## Fallback path: bulk-copy → catch-up → cutover

Used only when the existing table's data genuinely can't be attached as one valid range in a single shot (data doesn't form a sane contiguous bound, or the bounding constraint can't be validated cleanly — expected to be rare).

1. Bulk-copy rows into the new structure while the old table remains the live write target, untouched and fully functional — batched, each batch its own committed transaction.
2. Track a high-water mark (last processed value of the ordering column) through the copy.
3. Immediately before cutover, run one targeted catch-up pass for anything written after that mark.
4. Perform the same brief atomic rename-swap cutover described above.

**`plan` must flag this path's real limitation explicitly** rather than presenting it as equally safe: if the table has update activity on existing/older rows (not just appends — checked heuristically via `pg_stat_user_tables` update counts, or the presence of an `updated_at`-style column), a row already copied and removed from the source could receive an update against the old table that the migration never sees. This should surface as a named warning in `plan`'s output, not a silent risk.

## Splitting an already-attached coarse partition into finer ones (optional, deferred)

If the legacy data was attached as one big chunk and later needs proper per-period granularity:

1. Ensure a default partition exists on the parent first (safety net — see below).
2. `ALTER TABLE parent DETACH PARTITION legacy CONCURRENTLY` (PG14+) — detaches without blocking concurrent queries against the parent. Two-phase internally; if interrupted, finish with `... DETACH PARTITION legacy FINALIZE`.
3. Any write that would have matched the now-detached range, arriving in the gap before the finer replacement partitions exist, falls through to the default partition instead of erroring — a small, bounded set of rows.
4. Create the finer partitions, then run `reconcile-default` (already designed for stray default-partition data) to sweep up whatever landed there during the gap.
5. Backfill the now-detached, no-longer-live `legacy` table into the finer partitions using the atomic batch-move pattern below — safe because nothing is writing to it anymore.

This is why a default partition is operationally protective even for tables that otherwise look "done": it's the safety net for exactly this kind of maintenance-time gap.

## Atomic batch-move pattern (used everywhere data actually needs to relocate)

```sql
WITH moved AS (
  DELETE FROM source WHERE <batch bounds>
  RETURNING *
)
INSERT INTO destination_child SELECT * FROM moved;
```

Single statement, single transaction: either the whole batch moves or none of it does. There's never a window where a row exists in both places or neither, and a crash mid-batch just rolls back cleanly rather than leaving a partially-moved, duplicated, or lost row. This is what lets the tool avoid needing any external sync/CDC mechanism between source and destination — used identically for legacy re-slicing, default-partition reconciliation, and the fallback bulk-copy path's batches.

## Testing plan (see also `test_environment.md`)

- Fixtures 01/02/03 (empty / small / large populated tables): primary `ATTACH`-based cutover path, including timing/lock-duration checks on fixture 03's larger volume.
- Fixture 06 (default partition with strays): `reconcile-default` using the atomic move pattern.
- Fixture 11 (FK/view dependents): rename-swap cutover doesn't break dependents.
- New test to add: a concurrent-write correctness test that fires writes (including updates to pre-existing rows) throughout cutover and asserts none fail or land incorrectly.
- New test to add: kill-mid-copy resumability specifically for the fallback bulk-copy path, plus a test asserting `plan` surfaces the update-race warning when update activity is detected on the source table.

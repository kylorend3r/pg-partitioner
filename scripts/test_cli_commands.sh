#!/bin/bash
# Smoke-tests the pg-partitioner CLI surface against the disposable Docker
# test database (see scripts/setup_test_env.sh). Builds the release binary,
# provisions the container if it isn't already running, and runs every
# subcommand recording pass/fail + exit code. Not a substitute for
# `cargo test` -- this exercises the compiled binary against a real Postgres
# the same way an operator would from a terminal.
set -uo pipefail

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; BLUE='\033[0;34m'; NC='\033[0m'

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
BIN="$ROOT_DIR/target/release/pg-partitioner"
CONTAINER_NAME="pg-partitioner-test"

export PG_HOST=localhost
export PG_PORT=5434
export PG_DATABASE=partitioner_testdb
export PG_USER=partitioner_test
export PG_PASSWORD=test123

PASS=0
FAIL=0
FAILED_CASES=()

# run <label> <expected_exit_code> -- <cmd...>
run() {
  local label="$1" expected="$2"; shift 2
  [ "$1" = "--" ] && shift
  echo -e "${BLUE}==>${NC} $label"
  local out
  out=$("$@" 2>&1)
  local actual=$?
  if [ "$actual" -eq "$expected" ]; then
    echo -e "${GREEN}PASS${NC} (exit $actual)"
    PASS=$((PASS + 1))
  else
    echo -e "${RED}FAIL${NC} (expected exit $expected, got $actual)"
    echo "$out" | sed 's/^/    /'
    FAIL=$((FAIL + 1))
    FAILED_CASES+=("$label")
  fi
}

echo -e "${BLUE}=== 1. Build ===${NC}"
(cd "$ROOT_DIR" && cargo build --release) || { echo -e "${RED}Build failed${NC}"; exit 1; }
[ -x "$BIN" ] || { echo -e "${RED}Binary not found at $BIN${NC}"; exit 1; }
echo -e "${GREEN}Built $BIN${NC}"

echo -e "${BLUE}=== 2. Provision test database ===${NC}"
if docker ps --format '{{.Names}}' | grep -q "^${CONTAINER_NAME}\$"; then
  echo "Container already running, reusing it."
else
  "$SCRIPT_DIR/setup_test_env.sh" --profile=minimal
fi

echo -e "${BLUE}=== 3. Satisfy plan validation prerequisites ===${NC}"
# plan currently hardcodes partition_key=created_at (see src/main.rs); Postgres
# requires unique constraints on a partitioned table to include the partition
# key, so the fixture's PK(id)-only constraint must be widened first. Real
# conversions hit this exact requirement -- it's not a fixture bug.
docker exec "$CONTAINER_NAME" psql -q -U "$PG_USER" -d "$PG_DATABASE" -c \
  "ALTER TABLE public.table_02_small_populated DROP CONSTRAINT IF EXISTS table_02_small_populated_pkey;" >/dev/null
docker exec "$CONTAINER_NAME" psql -q -U "$PG_USER" -d "$PG_DATABASE" -c \
  "ALTER TABLE public.table_02_small_populated ADD PRIMARY KEY (id, created_at);" >/dev/null 2>&1 || true

CURRENT_LOCKS=$(docker exec "$CONTAINER_NAME" psql -tA -U "$PG_USER" -d "$PG_DATABASE" -c "SHOW max_locks_per_transaction;")
if [ "$CURRENT_LOCKS" -lt 256 ]; then
  echo "Raising max_locks_per_transaction (256) and restarting container..."
  docker exec "$CONTAINER_NAME" psql -q -U "$PG_USER" -d "$PG_DATABASE" -c "ALTER SYSTEM SET max_locks_per_transaction = 256;" >/dev/null
  docker restart "$CONTAINER_NAME" >/dev/null
  until docker exec "$CONTAINER_NAME" pg_isready -U "$PG_USER" -d "$PG_DATABASE" >/dev/null 2>&1; do sleep 1; done
fi
echo -e "${GREEN}Ready.${NC}"

echo -e "${BLUE}=== 4. Command matrix ===${NC}"
run "--help"                 0 -- "$BIN" --help
run "inspect"                 0 -- "$BIN" inspect
run "inspect --format json"   0 -- "$BIN" inspect --format json
run "explain"                 0 -- "$BIN" explain
run "doctor"                  0 -- "$BIN" doctor
run "doctor --table"          0 -- "$BIN" doctor --table table_02_small_populated
run "export --output"         0 -- "$BIN" export --output /tmp/pg-partitioner-export.yaml
run "plan (valid table)"      0 -- "$BIN" plan --schema public --table table_02_small_populated --output /tmp/pg-partitioner-plan.json
run "plan (missing PK cols)"  1 -- "$BIN" plan --schema public --table table_01_empty_existing --output /tmp/pg-partitioner-plan-01.json
run "apply --dry-run"         0 -- "$BIN" apply --plan-file /tmp/pg-partitioner-plan.json --dry-run
run "apply"                   0 -- "$BIN" apply --plan-file /tmp/pg-partitioner-plan.json
run "maintain --dry-run"      0 -- "$BIN" maintain --dry-run
run "bad host fails cleanly"  1 -- "$BIN" --host doesnotexist.invalid --database x --user x inspect

echo -e "${BLUE}=== 5. Known-limitation check ===${NC}"
# apply currently has no real DDL behind it (see src/orchestrator.rs) -- every
# action type just logs and returns Ok(()). This assertion documents that
# gap explicitly so it fails loudly the day apply grows real execution and
# this script needs updating instead of quietly asserting the wrong thing.
RELKIND=$(docker exec "$CONTAINER_NAME" psql -tA -U "$PG_USER" -d "$PG_DATABASE" -c \
  "SELECT relkind FROM pg_class WHERE relname = 'table_02_small_populated';")
if [ "$RELKIND" = "r" ]; then
  echo -e "${YELLOW}Confirmed: apply reported success but table_02_small_populated is still relkind='r' (not partitioned).${NC}"
  echo -e "${YELLOW}This is expected given the current stub orchestrator -- not a script bug.${NC}"
else
  echo -e "${RED}UNEXPECTED: relkind is now '$RELKIND'. apply may have started doing real DDL --${NC}"
  echo -e "${RED}update this script's expectations if that's intentional.${NC}"
fi

echo ""
echo -e "${BLUE}=== Summary ===${NC}"
echo -e "${GREEN}$PASS passed${NC}, ${RED}$FAIL failed${NC}"
if [ "$FAIL" -gt 0 ]; then
  echo "Failed cases:"
  printf '  - %s\n' "${FAILED_CASES[@]}"
  exit 1
fi
exit 0

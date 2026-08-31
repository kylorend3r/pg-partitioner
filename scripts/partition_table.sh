#!/bin/bash
# Partition one table on a remote database, configured entirely by environment
# variables -- no flags to remember and nothing host-specific baked in, so the
# same invocation works from a laptop, a jump host, or a CI job.
#
# It is a thin wrapper over the CLI: install -> plan -> apply, plus one
# add-partition per entry in LIST_PARTITIONS. Every rule it checks before
# connecting is one `plan` enforces too; checking here just means a
# misconfigured run fails in a second rather than after a round trip.
#
#   PARTITION_HELP=1 ./scripts/partition_table.sh    # every variable, explained
set -euo pipefail

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; BLUE='\033[0;34m'; NC='\033[0m'

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

usage() {
  cat <<'EOF'
partition_table.sh -- partition a table on a remote database, via environment variables.

CONNECTION (read by the CLI itself; host/database/user are required by this
script so a missing one fails loudly instead of silently connecting to
localhost/postgres, which is the CLI's own default)
  PG_HOST                 e.g. db.internal.example.com                   [required]
  PG_PORT                 default 5432
  PG_DATABASE                                                            [required]
  PG_USER                                                                [required]
  PG_PASSWORD             omit to fall back to the credential chain (~/.pgpass, keyring)
  PG_SSL_MODE             disable | prefer | require  (default prefer)
                          `require` also validates the certificate chain and the
                          hostname -- closer to libpq's `verify-full` than to its
                          `require`, so an internal-CA or self-signed server
                          certificate is rejected rather than accepted

TARGET
  PARTITION_SCHEMA        default "public"
  PARTITION_TABLE         table to convert (range), or to create (list/hash)  [required]
  PARTITION_STRATEGY      range | list | hash                                 [required]
  PARTITION_KEY           partition key column(s), comma-separated            [required]
                          list accepts exactly one column

RANGE ONLY -- the period window
  PARTITION_START_DATE    YYYY-MM-DD, today or earlier.
                          Converting an existing table: [required], and every
                          existing row must predate it or the cutover's bounding
                          check fails. Creating from a template: optional,
                          defaults to today.
  PARTITION_INTERVAL      default "1 month"
  PARTITION_PREMAKE       default 3

TEMPLATE FLOW -- creates a NEW table whose columns come from a template; the
template is only ever read, never modified, locked exclusively, or dropped.
Required for list and hash, which cannot convert an existing table at all;
optional for range, where omitting it converts the existing table instead.
  TEMPLATE_TABLE          table whose columns the new one is copied from
                          [required for list and hash]
  TEMPLATE_SCHEMA         defaults to PARTITION_SCHEMA
  HASH_PARTITIONS         bucket count; hash only and required there. Fixed at
                          creation -- changing it later means recreating every bucket
  LIST_PARTITIONS         list only, optional: children to add after creation,
                          as "name=v1,v2;name2=v3". A list table starts with no
                          children and no DEFAULT partition, so until it has some
                          it rejects every row. Use the literal NULL as a value
                          for the partition that holds null keys.

RETENTION (recorded on the registration; nothing drops partitions yet)
  RETENTION_TYPE          days | months | years | count -- pair with RETENTION_VALUE
  RETENTION_VALUE

RUN CONTROL
  DRY_RUN=1               plan and print the actions; execute no DDL
  ASSUME_YES=1            skip the confirmation prompt (required non-interactively)
  SKIP_INSTALL=1          don't run `install` (metadata tables already present)
  PLAN_FILE               where to write the plan (default: a temp file)
  PG_PARTITIONER_BIN      binary to use (default: target/release, then target/debug, then $PATH)
  PARTITION_LOG_LEVEL     passed through as --log-level (default info)

EXAMPLES
  # Convert an existing time-series table to monthly range partitions
  PG_HOST=db.internal PG_DATABASE=app PG_USER=deploy PG_PASSWORD=... \
  PARTITION_TABLE=events PARTITION_STRATEGY=range PARTITION_KEY=created_at \
  PARTITION_START_DATE=2026-01-01 ./scripts/partition_table.sh

  # Create a NEW range-partitioned table from a template instead
  PG_HOST=db.internal PG_DATABASE=app PG_USER=deploy PG_PASSWORD=... \
  PARTITION_TABLE=events PARTITION_STRATEGY=range PARTITION_KEY=created_at \
  TEMPLATE_TABLE=events_template PARTITION_PREMAKE=6 ./scripts/partition_table.sh

  # Create a hash-partitioned table with 8 buckets from a template
  PG_HOST=db.internal PG_DATABASE=app PG_USER=deploy PG_PASSWORD=... \
  PARTITION_TABLE=sessions PARTITION_STRATEGY=hash PARTITION_KEY=tenant_id \
  TEMPLATE_TABLE=sessions_template HASH_PARTITIONS=8 ./scripts/partition_table.sh
EOF
}

if [ -n "${PARTITION_HELP:-}" ]; then usage; exit 0; fi

die() { echo -e "${RED}error: $*${NC}" >&2; exit 1; }

# ---------------------------------------------------------------------------
# Binary
# ---------------------------------------------------------------------------

if [ -n "${PG_PARTITIONER_BIN:-}" ]; then
  BIN="$PG_PARTITIONER_BIN"
  [ -x "$BIN" ] || die "PG_PARTITIONER_BIN is not an executable file: $BIN"
elif [ -x "$REPO_DIR/target/release/pg-partitioner" ]; then
  BIN="$REPO_DIR/target/release/pg-partitioner"
elif [ -x "$REPO_DIR/target/debug/pg-partitioner" ]; then
  BIN="$REPO_DIR/target/debug/pg-partitioner"
elif command -v pg-partitioner >/dev/null 2>&1; then
  BIN="$(command -v pg-partitioner)"
else
  die "no pg-partitioner binary found -- run 'cargo build --release', or set PG_PARTITIONER_BIN"
fi

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

PARTITION_SCHEMA="${PARTITION_SCHEMA:-public}"
PARTITION_STRATEGY="$(printf '%s' "${PARTITION_STRATEGY:-}" | tr '[:upper:]' '[:lower:]')"
LOG_LEVEL="${PARTITION_LOG_LEVEL:-info}"

for var in PG_HOST PG_DATABASE PG_USER PARTITION_TABLE PARTITION_STRATEGY PARTITION_KEY; do
  [ -n "${!var:-}" ] || die "$var is required (run PARTITION_HELP=1 $0 for the full list)"
done

case "$PARTITION_STRATEGY" in
  range|list|hash) ;;
  *) die "PARTITION_STRATEGY must be range, list, or hash (got '$PARTITION_STRATEGY')" ;;
esac

# Rejected rather than ignored: a variable that silently does nothing is worse
# than one that errors, because the run then looks like it honoured it.
reject_unless() {
  local applies_to="$1" var="$2"
  if [ -n "${!var:-}" ] && [ "$PARTITION_STRATEGY" != "$applies_to" ]; then
    die "$var applies only to PARTITION_STRATEGY=$applies_to (this run is '$PARTITION_STRATEGY')"
  fi
}

reject_unless hash HASH_PARTITIONS
reject_unless list LIST_PARTITIONS
reject_unless range PARTITION_START_DATE
reject_unless range PARTITION_INTERVAL
reject_unless range PARTITION_PREMAKE

if [ -n "${RETENTION_TYPE:-}" ] && [ -z "${RETENTION_VALUE:-}" ]; then
  die "RETENTION_TYPE needs RETENTION_VALUE (set both, or neither)"
fi
if [ -n "${RETENTION_VALUE:-}" ] && [ -z "${RETENTION_TYPE:-}" ]; then
  die "RETENTION_VALUE needs RETENTION_TYPE (set both, or neither)"
fi

case "$PARTITION_STRATEGY" in
  range)
    # Range is the one strategy with both flows. With a template the target is
    # a new, empty table, so there is no existing data the window has to sit
    # ahead of and the CLI defaults the start date to today; converting an
    # existing table, the date is the boundary between the one legacy partition
    # holding today's rows and the per-period partitions after it.
    if [ -z "${TEMPLATE_TABLE:-}" ] && [ -z "${PARTITION_START_DATE:-}" ]; then
      die "PARTITION_START_DATE is required when converting an existing table to range \
(set TEMPLATE_TABLE instead to create a new range-partitioned table, where it defaults to today)"
    fi
    ;;
  list|hash)
    [ -n "${TEMPLATE_TABLE:-}" ] || die \
      "TEMPLATE_TABLE is required for $PARTITION_STRATEGY: neither strategy can convert an \
existing table, so PARTITION_TABLE names a new table whose columns come from the template"
    if [ "$PARTITION_STRATEGY" = "hash" ]; then
      [ -n "${HASH_PARTITIONS:-}" ] || die \
        "HASH_PARTITIONS is required for hash (how many buckets to create)"
      case "$HASH_PARTITIONS" in
        ''|*[!0-9]*) die "HASH_PARTITIONS must be a positive integer (got '$HASH_PARTITIONS')" ;;
      esac
      [ "$HASH_PARTITIONS" -ge 1 ] || die "HASH_PARTITIONS must be at least 1"
    fi
    if [ "$PARTITION_STRATEGY" = "list" ] && [[ "$PARTITION_KEY" == *,* ]]; then
      die "PARTITION_KEY must be a single column for list -- PARTITION BY LIST accepts \
exactly one column (range and hash accept composite keys)"
    fi
    ;;
esac

# Parsed before any DDL runs: a typo here would otherwise only surface after the
# parent table had already been created, leaving half the work done.
LIST_NAMES=()
LIST_VALUES=()
if [ -n "${LIST_PARTITIONS:-}" ]; then
  IFS=';' read -r -a _list_entries <<< "$LIST_PARTITIONS"
  for entry in "${_list_entries[@]}"; do
    [ -n "$entry" ] || continue
    [[ "$entry" == *=* ]] || die \
      "LIST_PARTITIONS entry '$entry' is not 'name=value[,value...]'"
    name="${entry%%=*}"
    values="${entry#*=}"
    [ -n "$name" ] || die "LIST_PARTITIONS entry '$entry' has an empty partition name"
    [ -n "$values" ] || die "LIST_PARTITIONS entry '$entry' has no values"
    LIST_NAMES+=("$name")
    LIST_VALUES+=("$values")
  done
fi

if [ -n "${PLAN_FILE:-}" ]; then
  PLAN_PATH="$PLAN_FILE"
else
  # No `.json` suffix on the template: BSD mktemp only substitutes trailing
  # X's, so a suffixed template yields a fixed, predictable filename.
  PLAN_PATH="$(mktemp "${TMPDIR:-/tmp}/pg-partitioner-plan.XXXXXX")"
fi

# ---------------------------------------------------------------------------
# Confirmation
# ---------------------------------------------------------------------------

echo -e "${BLUE}=== pg-partitioner ===${NC}"
echo "  binary       $BIN"
echo "  server       ${PG_USER}@${PG_HOST}:${PG_PORT:-5432}/${PG_DATABASE}  (sslmode ${PG_SSL_MODE:-prefer})"
if [ "$PARTITION_STRATEGY" = "range" ]; then
  if [ -n "${TEMPLATE_TABLE:-}" ]; then
    echo "  create       ${PARTITION_SCHEMA}.${PARTITION_TABLE}  ->  PARTITION BY RANGE (${PARTITION_KEY})"
    echo "  template     ${TEMPLATE_SCHEMA:-$PARTITION_SCHEMA}.${TEMPLATE_TABLE}  (read only)"
  else
    echo "  convert      ${PARTITION_SCHEMA}.${PARTITION_TABLE}  ->  PARTITION BY RANGE (${PARTITION_KEY})"
  fi
  echo "  window       from ${PARTITION_START_DATE:-today}, every ${PARTITION_INTERVAL:-1 month}, ${PARTITION_PREMAKE:-3} ahead"
else
  echo "  create       ${PARTITION_SCHEMA}.${PARTITION_TABLE}  ->  PARTITION BY $(printf '%s' "$PARTITION_STRATEGY" | tr '[:lower:]' '[:upper:]') (${PARTITION_KEY})"
  echo "  template     ${TEMPLATE_SCHEMA:-$PARTITION_SCHEMA}.${TEMPLATE_TABLE}  (read only)"
  [ -n "${HASH_PARTITIONS:-}" ] && echo "  buckets      ${HASH_PARTITIONS}"
  [ ${#LIST_NAMES[@]} -gt 0 ] && echo "  partitions   ${#LIST_NAMES[@]} to add after creation"
fi
[ -n "${RETENTION_TYPE:-}" ] && echo "  retention    ${RETENTION_VALUE} ${RETENTION_TYPE} (recorded only; nothing drops partitions yet)"
echo "  plan file    $PLAN_PATH"
echo ""

if [ -n "${DRY_RUN:-}" ]; then
  echo -e "${YELLOW}DRY RUN -- no DDL will be executed.${NC}"
elif [ -z "${ASSUME_YES:-}" ]; then
  if [ ! -t 0 ]; then
    die "refusing to execute DDL non-interactively without ASSUME_YES=1 (or set DRY_RUN=1)"
  fi
  echo -e "${YELLOW}This executes DDL against the server above.${NC}"
  read -r -p "Type 'yes' to continue: " reply
  [ "$reply" = "yes" ] || die "aborted"
fi
echo ""

# ---------------------------------------------------------------------------
# install -> plan -> apply
# ---------------------------------------------------------------------------

run() {
  echo -e "${YELLOW}\$ $BIN $*${NC}"
  "$BIN" --log-level "$LOG_LEVEL" "$@"
}

if [ -z "${SKIP_INSTALL:-}" ] && [ -z "${DRY_RUN:-}" ]; then
  echo -e "${BLUE}--- install (metadata tables) ---${NC}"
  run install
  echo ""
fi

echo -e "${BLUE}--- plan ---${NC}"
PLAN_ARGS=(plan
  --schema "$PARTITION_SCHEMA"
  --table "$PARTITION_TABLE"
  --strategy "$PARTITION_STRATEGY"
  --key "$PARTITION_KEY"
  --output "$PLAN_PATH")

case "$PARTITION_STRATEGY" in
  range)
    if [ -n "${PARTITION_START_DATE:-}" ]; then
      PLAN_ARGS+=(--start-date "$PARTITION_START_DATE")
    fi
    if [ -n "${PARTITION_INTERVAL:-}" ]; then
      PLAN_ARGS+=(--interval "$PARTITION_INTERVAL")
    fi
    if [ -n "${PARTITION_PREMAKE:-}" ]; then
      PLAN_ARGS+=(--premake "$PARTITION_PREMAKE")
    fi
    ;;
  list|hash)
    if [ -n "${HASH_PARTITIONS:-}" ]; then
      PLAN_ARGS+=(--hash-partitions "$HASH_PARTITIONS")
    fi
    ;;
esac

# Shared by all three: the template switch is what selects creation over
# conversion, and range is now the one strategy that can go either way.
if [ -n "${TEMPLATE_TABLE:-}" ]; then
  PLAN_ARGS+=(--template-table "$TEMPLATE_TABLE")
  if [ -n "${TEMPLATE_SCHEMA:-}" ]; then
    PLAN_ARGS+=(--template-schema "$TEMPLATE_SCHEMA")
  fi
fi

if [ -n "${RETENTION_TYPE:-}" ]; then
  PLAN_ARGS+=(--retention-type "$RETENTION_TYPE" --retention-value "$RETENTION_VALUE")
fi

run "${PLAN_ARGS[@]}"
echo ""

echo -e "${BLUE}--- apply ---${NC}"
if [ -n "${DRY_RUN:-}" ]; then
  run apply --plan-file "$PLAN_PATH" --dry-run
else
  run apply --plan-file "$PLAN_PATH"
fi
echo ""

# Each add-partition is its own plan+apply, so a failure part-way through leaves
# the partitions before it in place; re-running skips those and retries the rest.
if [ ${#LIST_NAMES[@]} -gt 0 ]; then
  echo -e "${BLUE}--- add-partition (${#LIST_NAMES[@]}) ---${NC}"
  for i in "${!LIST_NAMES[@]}"; do
    ADD_ARGS=(add-partition
      --schema "$PARTITION_SCHEMA"
      --table "$PARTITION_TABLE"
      --partition-name "${LIST_NAMES[$i]}"
      --values "${LIST_VALUES[$i]}")
    [ -n "${DRY_RUN:-}" ] && ADD_ARGS+=(--dry-run)
    run "${ADD_ARGS[@]}"
  done
  echo ""
fi

if [ -n "${DRY_RUN:-}" ]; then
  echo -e "${GREEN}Dry run complete. Plan written to $PLAN_PATH${NC}"
else
  echo -e "${BLUE}--- result ---${NC}"
  run inspect --table "$PARTITION_TABLE"
  echo -e "${GREEN}Done. Plan kept at $PLAN_PATH${NC}"
fi

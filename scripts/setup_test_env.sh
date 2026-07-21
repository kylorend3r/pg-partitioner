#!/bin/bash
# Provision the disposable pg-partitioner-test container and load the
# zero-to-hero fixture catalog. See docs/test_environment.md.
set -euo pipefail

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; BLUE='\033[0;34m'; NC='\033[0m'

CONTAINER_NAME="pg-partitioner-test"
IMAGE="postgres:16"
HOST_PORT="5434"
PG_DATABASE="partitioner_testdb"
PG_USER="partitioner_test"
PG_PASSWORD="test123"
PROFILE="full"
WITH_PG_PARTMAN="false"

for arg in "$@"; do
  case "$arg" in
    --profile=minimal) PROFILE="minimal" ;;
    --profile=full) PROFILE="full" ;;
    --with-pg-partman) WITH_PG_PARTMAN="true" ;;
    *) echo -e "${RED}Unknown argument: $arg${NC}"; exit 1 ;;
  esac
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FIXTURES_DIR="$SCRIPT_DIR/../tests/fixtures"

echo -e "${BLUE}=== pg-partitioner test environment setup (profile: $PROFILE) ===${NC}"

if docker ps -a --format '{{.Names}}' | grep -q "^${CONTAINER_NAME}\$"; then
  echo -e "${YELLOW}Container ${CONTAINER_NAME} already exists. Use reset_test_env.sh for a clean slate.${NC}"
else
  echo -e "${YELLOW}Starting ${CONTAINER_NAME}...${NC}"
  docker run -d --name "$CONTAINER_NAME" \
    -e POSTGRES_DB="$PG_DATABASE" \
    -e POSTGRES_USER="$PG_USER" \
    -e POSTGRES_PASSWORD="$PG_PASSWORD" \
    -p "${HOST_PORT}:5432" \
    "$IMAGE" >/dev/null
fi

echo -e "${YELLOW}Waiting for PostgreSQL to accept connections...${NC}"
until docker exec "$CONTAINER_NAME" pg_isready -U "$PG_USER" -d "$PG_DATABASE" >/dev/null 2>&1; do
  sleep 1
done
echo -e "${GREEN}Ready.${NC}"

if [ "$WITH_PG_PARTMAN" = "true" ]; then
  echo -e "${YELLOW}Installing pg_partman for side-by-side comparison...${NC}"
  docker exec "$CONTAINER_NAME" psql -U "$PG_USER" -d "$PG_DATABASE" -c "CREATE EXTENSION IF NOT EXISTS pg_partman;" \
    || echo -e "${YELLOW}pg_partman not available in this image; skipping.${NC}"
fi

if [ "$PROFILE" = "minimal" ]; then
  FILE_LIST="00_init.sql 01_empty_existing_table.sql 02_small_populated_table.sql 03_large_populated_table.sql"
else
  FILE_LIST=$(cd "$FIXTURES_DIR" && ls *.sql | sort | tr '\n' ' ')
fi

echo -e "${YELLOW}Loading fixtures...${NC}"
for f in $FILE_LIST; do
  echo "  -> $f"
  docker exec -i "$CONTAINER_NAME" psql -U "$PG_USER" -d "$PG_DATABASE" < "$FIXTURES_DIR/$f"
done

echo -e "${GREEN}Test environment ready.${NC}"
echo ""
echo "  export PG_HOST=localhost"
echo "  export PG_PORT=$HOST_PORT"
echo "  export PG_DATABASE=$PG_DATABASE"
echo "  export PG_USER=$PG_USER"
echo "  export PG_PASSWORD=$PG_PASSWORD"
echo ""
echo "Fixture 09 (index-repair scenario) is not loaded automatically -- run"
echo "  tests/fixtures/09_index_repair_scenario.sh manually once fixture 04 is loaded."

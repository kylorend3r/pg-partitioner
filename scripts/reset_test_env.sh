#!/bin/bash
# Tear down and re-provision the pg-partitioner-test container from scratch.
# Data is not persisted (no volume), so this is safe and matches the
# disposable philosophy already used for pg-reindexer-test.
set -euo pipefail
CONTAINER_NAME="pg-partitioner-test"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

echo "Tearing down ${CONTAINER_NAME}..."
docker stop "$CONTAINER_NAME" >/dev/null 2>&1 || true
docker rm "$CONTAINER_NAME" >/dev/null 2>&1 || true

exec "$SCRIPT_DIR/setup_test_env.sh" "$@"

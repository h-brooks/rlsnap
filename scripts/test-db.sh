#!/usr/bin/env bash
# Start or stop a postgres:16 container for local dev/test, listening on
# 54390 (matches PG_TEST_URL's default in every crate's integration tests).
set -euo pipefail

CONTAINER_NAME="rlsnap-test-pg"
PORT=54390

usage() {
    echo "usage: $0 {start|stop|status}" >&2
    exit 1
}

case "${1:-}" in
    start)
        if docker ps --format '{{.Names}}' | grep -q "^${CONTAINER_NAME}\$"; then
            echo "already running: ${CONTAINER_NAME}"
            exit 0
        fi
        docker run -d \
            --name "${CONTAINER_NAME}" \
            -e POSTGRES_PASSWORD=postgres \
            -p "${PORT}:5432" \
            postgres:16
        echo "started ${CONTAINER_NAME} on port ${PORT}"
        ;;
    stop)
        docker rm -f "${CONTAINER_NAME}" >/dev/null 2>&1 || true
        echo "stopped ${CONTAINER_NAME}"
        ;;
    status)
        docker ps --filter "name=${CONTAINER_NAME}"
        ;;
    *)
        usage
        ;;
esac

#!/bin/sh
# Docker entrypoint for RuvSense Edge master.
#
# Usage patterns:
#
# 1. No arguments - use defaults from environment:
#      docker run -e CSI_SOURCE=esp32 ruvsense-edge:latest
#
# 2. Pass CLI flags directly:
#      docker run ruvsense-edge:latest --source esp32 --tick-ms 500
#      docker run ruvsense-edge:latest --model /app/models/my.rvf
#
# Environment variables:
#   CSI_SOURCE - data source: auto (default), esp32, wifi, simulate
#                simulate requires RUVSENSE_ENABLE_SIMULATION=true
#   MODELS_DIR - directory to scan for .rvf model files (default: data/models)
set -e

case "${1:-}" in
    cog-ha-matter|ha-matter)
        shift
        exec /app/cog-ha-matter \
            --sensing-url "${SENSING_URL:-http://127.0.0.1:3000}" \
            "$@"
        ;;
    homecore|homecore-server)
        shift
        exec /app/homecore-server \
            --bind "${HOMECORE_BIND:-0.0.0.0:8123}" \
            "$@"
        ;;
esac

if [ "${1#-}" != "$1" ] || [ -z "$1" ]; then
    set -- /app/sensing-server \
        --source "${CSI_SOURCE:-auto}" \
        --tick-ms 100 \
        --ui-path /app/ui \
        --http-port 3000 \
        --ws-port 3001 \
        --bind-addr 0.0.0.0 \
        "$@"
fi

exec "$@"

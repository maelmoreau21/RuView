#!/bin/sh
# Entrypoint for the RuvSense Edge master appliance image.
set -e

if [ "${1#-}" != "$1" ] || [ -z "${1:-}" ]; then
    set -- /app/ruvsense-master \
        --source "${CSI_SOURCE:-esp32}" \
        --tick-ms "${RUVSENSE_TICK_MS:-100}" \
        --ui-path /app/ui \
        --http-port "${RUVSENSE_HTTP_PORT:-3000}" \
        --ws-port "${RUVSENSE_WS_PORT:-3001}" \
        --udp-port "${RUVSENSE_UDP_PORT:-5005}" \
        --bind-addr "${RUVSENSE_BIND_ADDR:-0.0.0.0}" \
        --min-nodes "${RUVSENSE_MIN_NODES:-3}" \
        --data-dir "${RUVSENSE_DATA_DIR:-/var/lib/ruvsense}" \
        "$@"
fi

exec "$@"

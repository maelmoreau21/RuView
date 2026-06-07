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
        --min-nodes "${RUVSENSE_MIN_NODES:-1}" \
        --wifi-interface "${RUVSENSE_WIFI_INTERFACE:-wlan0}" \
        --ap-scan-interval-secs "${RUVSENSE_AP_SCAN_INTERVAL_SECS:-10}" \
        --data-dir "${RUVSENSE_DATA_DIR:-/var/lib/ruvsense}" \
        "$@"
fi

exec "$@"

# RuvSense Edge Troubleshooting

This guide covers the failure modes most often seen while running the Docker
master, ESP32-C6 nodes, simulation mode, health monitoring, and model loading.

## 1. False positives in a non-empty room

Symptom: presence stays true after the target area should be empty, or a room
with people outside the target zone keeps triggering occupancy.

Cause: the room is not quiet enough for an empty-room baseline, multipath is
strong, or the coherence gate has not been enabled before calibration.

Fix:

```bash
python -m pip install requests
python scripts/setup_health_monitoring.py
python scripts/calibrate_room.py
```

If the room remains occupied, use the occupied-room threshold path:

```bash
PRESENCE_THRESHOLD_MULTIPLIER=1.4 docker compose -f docker/compose.yml up -d --build --force-recreate ruvsense-master
```

## 2. JSONL RVF model is not recognized

Symptom: loading `model.rvf.jsonl` fails with an invalid magic error such as
`expected 0x52564653`.

Cause: the sensing server currently expects the binary RVF container format.
The Hugging Face JSONL RVF export is usable for Python inspection and training,
but not yet as the live `--model` input.

Fix: run `ruvsense-master` without `--model` for live Docker sensing until a
JSONL adapter or binary RVF republish is available. Use `model.safetensors` or
the JSONL file from Python tooling only.

## 3. Not enough nodes for localization

Symptom: `/api/v1/location` returns low confidence, empty persons, or a
single-node fallback even though `/health/ready` is 200.

Cause: one ESP32-C6 node is enough to bring the console online, but geometric
localization and multistatic modules need more spatial diversity.

Fix: provision at least 3 positioned ESP32-C6 nodes for high-confidence
location estimates. Verify:

```bash
curl http://127.0.0.1:3000/api/v1/topology
```

## 4. Health modules are not loaded

Symptom: apnea, respiration trend, fall, or cardiac alerts never activate.

Cause: the runtime module catalog has not been enabled through the API, or the
monitoring script cannot reach the master.

Fix:

```bash
python -m pip install requests
python scripts/setup_health_monitoring.py
curl http://127.0.0.1:3000/api/v1/modules
```

Look for enabled `respiration_tracking`, `fall_detection`,
`sleep_apnea_screening`, and `cardiac_arrhythmia`.

## 5. Calibration is incorrect

Symptom: calibration completes but presence, count, or confidence gets worse.

Cause: calibration was captured while the room was occupied, moving, or below
live-node quorum.

Fix: clear the target area, wait for motion to settle, confirm live nodes, then
rerun:

```bash
curl http://127.0.0.1:3000/health/ready
python scripts/calibrate_room.py
```

If the room cannot be emptied, use the occupied-room mode and raise the
presence threshold instead of saving a false empty-room baseline.

## 6. Docker stack never becomes ready

Symptom: `/health/live` works but `/health/ready` returns 503.

Cause: production defaults are live-only. The master will fail closed until the
configured live-node quorum is present.

Fix: either provision a real ESP32-C6 node or start explicit simulation for dev
and CI only:

```bash
CSI_SOURCE=simulate RUVSENSE_ENABLE_SIMULATION=true docker compose -f docker/compose.yml up -d --build
```

## 7. Simulation source is rejected

Symptom: startup logs say simulation was requested without being enabled.

Cause: `CSI_SOURCE=simulate` is guarded by `RUVSENSE_ENABLE_SIMULATION=true` so
production cannot silently fall back to synthetic data.

Fix:

```bash
CSI_SOURCE=simulate RUVSENSE_ENABLE_SIMULATION=true docker compose -f docker/compose.yml up -d
```

Do not use this in production validation.

## 8. API calls return 401

Symptom: `/health/*` works, but `/api/v1/*` returns unauthorized.

Cause: `RUVIEW_API_TOKEN` is set, so bearer-token auth is enforced for API
routes.

Fix:

```bash
curl -H "Authorization: Bearer $RUVIEW_API_TOKEN" http://127.0.0.1:3000/api/v1/info
```

For local LAN mode, unset `RUVIEW_API_TOKEN` and recreate the container.

## 9. LAN browser access is blocked

Symptom: the console works on localhost but fails from another machine.

Cause: host-header validation defaults to loopback names only.

Fix: set the allowed host list before starting Docker:

```bash
SENSING_ALLOWED_HOSTS=192.168.1.20,ruvsense-master docker compose -f docker/compose.yml up -d
```

Use the actual Pi or Docker host IP/hostname.

## 10. RSSI-only hardware gives weak vitals or pose

Symptom: a laptop WiFi adapter shows coarse motion but vital signs, pose, or
through-wall sensing have low confidence.

Cause: consumer WiFi scans provide RSSI only, not CSI. Full RuvSense Edge
features require ESP32 CSI frames.

Fix: use ESP32-C6 CSI nodes for production sensing. RSSI-only mode is useful
for coarse presence checks, not contactless vitals or pose validation.

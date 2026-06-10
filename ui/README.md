# RuvSense Console UI

Production web UI for RuvSense Edge. The main entry point is `index.html`: one simple console for the Pi master, 3 ESP32-C6 nodes, 2 mesh AP anchors, setup checklist, modules, calibration, logs, and a small advanced diagnostics area.

## Production Surfaces

- `index.html` / `app.js` / `style.css` - live operations console and first page operators should use.
- `observatory.html` - advanced 3D RF environment diagnostic view.
- `pose-fusion.html` - advanced camera + live CSI fusion diagnostic view.
- `viz.html` - legacy redirect to `observatory.html`.

## Data Policy

The production UI does not generate synthetic sensing frames. If the master, WebSocket, or hardware is unavailable, screens show `offline`, `reconnecting`, or `degraded` states and keep waiting for real data.

## APIs Used

- `GET /health/live`
- `GET /health/ready`
- `GET /api/v1/fleet`
- `GET /api/v1/nodes`
- `GET /api/v1/environment`
- `PUT /api/v1/environment`
- `GET /api/v1/modules`
- `GET /api/v1/calibration`
- `WS /ws/sensing`

## Local Static Preview

```bash
cd ui
python -m http.server 3000
```

Open `http://localhost:3000/index.html`. Without a running master the console remains offline by design.

## Docker Preview

```bash
docker compose -f docker/compose.yml up -d --build
```

Open `http://localhost:3000/`; the root route redirects to the console.

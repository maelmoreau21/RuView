# RuvSense Mobile

Companion React Native / Expo app for RuvSense Edge.

The mobile app is live-first: when the server is unreachable it shows disconnected/offline state and keeps reconnecting. It does not switch silently to generated sensing data.

## Screens

- Live: 3D viewer and HUD for live sensing frames.
- Vitals: clinical-adjacent screening metrics from live CSI frames.
- Zones: occupancy grid and room zones.
- MAT: incident and survivor workflow.
- Settings: server URL, theme, RSSI option, alert sound.

## Quick Start

```bash
cd ui/mobile
npm install
npx expo start --web
```

Configure the server URL in Settings. For a Pi Docker deployment, point it at the Pi master HTTP origin, for example `http://192.168.1.20:3000`.

## Expected Server

- HTTP: `/health/live`, `/health/ready`, `/api/v1/fleet`, `/api/v1/environment`
- WebSocket: `/ws/sensing`

## Test Notes

Unit tests may use mocked WebSocket or HTTP objects, but those mocks are test doubles only. Runtime code reports offline instead of generating production data.

# UI realtime architecture

## Live data

- `index.html` and `observatory.html` each connect directly through `websocket.js` (`RuvSenseWS` -> `/ws/pose`).
- The shared socket helper updates `window.RS` and notifies each view through `RuvSenseWS.onUpdate`.
- `BroadcastChannel("ruvsense-config")` is reserved for room-config and display-setting changes.
- Live sensing frames are never copied between tabs.

## Alerts

- `alerts.js` is shared by both views.
- It detects apnea, cardiac arrest, falls, and abnormal vitals from the shared state.
- Active alerts are stored in `localStorage` under `ruvsense:alerts` and remain visible until manual acknowledgement.
- Browser notifications use the native Notification API; audio uses a small Web Audio beep.

## Health status

- Both views expose `#global-health-badge`.
- `alerts.js` updates it to `NORMAL`, `ANOMALIE`, or flashing `CRITIQUE` from the latest live frame.
- 3D silhouettes render vitals on canvas textures. Critical apnea/cardiac states tint the silhouette red.

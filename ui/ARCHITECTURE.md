# UI realtime architecture

## Shared state

- `console.html` is the 2D live view and owns the only browser WebSocket connection (`app.js` -> `/ws/sensing`).
- After REST refreshes or WebSocket frames, `app.js` serializes the public UI state and publishes it on `BroadcastChannel("ruvsense")`.
- The same snapshot is cached in `localStorage` under `ruvsense:shared-state` so a newly opened 3D tab can render immediately.
- `observatory.html` does not open its own live WebSocket. `observatory/js/main.js` listens to the shared channel, rebuilds a sensing frame, and feeds the existing Three.js normalization/rendering path.

## Alerts

- `alerts.js` is shared by both views.
- It detects apnea, cardiac arrest, falls, and abnormal vitals from the shared state.
- Active alerts are stored in `localStorage` under `ruvsense:alerts` and remain visible until manual acknowledgement.
- Alert upserts and acknowledgements are also broadcast on `BroadcastChannel("ruvsense")` so both tabs stay aligned.
- Browser notifications use the native Notification API; audio uses a small Web Audio beep.

## Health status

- Both views expose `#global-health-badge`.
- `alerts.js` updates it to `NORMAL`, `ANOMALIE`, or flashing `CRITIQUE` from the latest shared state.
- 3D silhouettes render vitals on canvas textures. Critical apnea/cardiac states tint the silhouette red.

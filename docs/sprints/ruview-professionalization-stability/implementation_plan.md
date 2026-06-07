# RuView Professionalization & Stability Sprint Plan

## Scope

Implement the five approved sprint items while preserving production live-only behavior:

- Adaptive UDP jitter buffer and per-node packet re-sequencer for ESP32 CSI frames.
- ADR-135 calibration drift detector with recommendation-only recalibration decisions.
- Strict runtime configuration schema/semantic validation on startup and save.
- Prometheus metrics for jitter, packet loss, pose latency, and active tracking zones.
- Feature-gated serial hot-plug reconnect supervisor in `wifi-densepose-hardware`.

## Design Decisions

- Interpolated CSI frames are derived only from adjacent live CSI frames and are never used for node liveness, readiness quorum, or calibration baselines.
- Calibration drift detection emits `RecommendRecalibration` only after sustained threshold breaches; it never silently replaces a baseline.
- Missing `data/config.json` uses defaults. Malformed JSON, unknown fields, unsupported module config versions, invalid topology, or invalid values fail startup.
- `/metrics` remains the single Prometheus endpoint. Dynamic labels are bounded to numeric node IDs and static enums.
- Serial reconnect logic is testable through a port factory abstraction; real `serialport` integration is optional so no-default workspace tests remain viable.

## Verification Plan

- Run focused Rust checks for `ruvsense-master`, `wifi-densepose-signal`, and `wifi-densepose-hardware`.
- Run `cd v2 && cargo test --workspace --no-default-features`.
- Run `python archive/v1/data/proof/verify.py`.
- Run `bash scripts/generate-witness-bundle.sh`, then `bash VERIFY.sh` inside the generated bundle.
- Run `npx @Codex-flow/cli@latest security scan` after network/config changes.

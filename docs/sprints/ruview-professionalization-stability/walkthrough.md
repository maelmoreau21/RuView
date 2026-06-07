# RuView Professionalization & Stability Walkthrough

## Implemented Changes

- `ruvsense-master` now routes ESP32 CSI frames through a per-node UDP jitter buffer before the existing DSP path. Short out-of-order gaps are held and re-sequenced, small missing gaps are interpolated from neighboring live frames, large gaps are skipped, and duplicate/late frames are dropped.
- Interpolated frames stabilize smoothing/history, but live readiness, node liveness, and calibration baselines are updated only by live frames.
- ADR-135 calibration drift detection is available as `CalibrationDriftDetector`, `CalibrationDriftConfig`, and `CalibrationDriftDecision`, with the default 300-frame window, threshold `> 4.0`, and 3-window confirmation.
- Runtime config now derives JSON Schema with `schemars`, denies unknown fields, validates semantic constraints, fails closed on invalid startup files, and persists through a temp-file/sync/rename flow.
- `/metrics` now exposes jitter counters, packet drop ratios, reordering counters, jitter depth/hold gauges, latest/P95 pose latency, and active tracking zones.
- `wifi-densepose-hardware` now has a feature-gated serial reconnect supervisor with exponential backoff, read-error reconnect, zero-read disconnect detection, and fake port/factory tests.

## Operational Notes

- Missing `data/config.json` still starts with defaults.
- Bad `data/config.json` is fatal at startup and must be corrected by the operator.
- Recalibration recommendations are advisory. Operators remain responsible for starting a new empty-room baseline capture.
- Serial reconnect support is enabled with the `serial-reconnect` feature.

## Validation Commands

```bash
cd v2
cargo test --workspace --no-default-features
cd ..
python archive/v1/data/proof/verify.py
bash scripts/generate-witness-bundle.sh
cd dist/witness-bundle-ADR028-*
bash VERIFY.sh
```

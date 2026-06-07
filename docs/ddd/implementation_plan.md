# CSI Ingest Stability and Hardware Resilience Implementation Plan

## Scope

Stabilize live RuvSense Edge ingestion without adding demo, mock, or simulation fallback. The work stays in the live CSI and hardware-resilience path:

- UDP CSI frame ordering, bounded hold, loss accounting, and small-gap interpolation.
- Adaptive calibration health monitoring for field-model drift and stale baselines.
- Serial baud reconnect for host-side UART resilience.
- Tests that prove live-only behavior fails closed when hardware or quorum is absent.

## 1. UDP Jitter Buffer

Target code surfaces:

- `v2/crates/wifi-densepose-sensing-server/src/udp_jitter.rs`
- `v2/crates/wifi-densepose-sensing-server/src/main.rs`
- `v2/crates/wifi-densepose-hardware/src/esp32_parser.rs`

Algorithm:

1. Keep one jitter state per `node_id`: `expected_sequence`, `last_emitted_live`, `BTreeMap<sequence, frame>`, and counters.
2. Emit the first valid frame immediately and initialize `expected_sequence = sequence + 1`.
3. For each next frame:
   - If `sequence == expected_sequence`, emit immediately, then drain contiguous buffered frames.
   - If `sequence` is behind `expected_sequence`, count it as late/duplicate and drop it.
   - If `sequence` is ahead by `<= max_reorder_gap`, buffer it and wait for the missing head.
   - If `sequence` is ahead by `> max_reorder_gap`, count the missing range as dropped, clear that node's buffer, and resync on the new live frame.
4. Flush a buffered head when either `max_hold` expires or `max_buffered_frames` is exceeded.
5. On flush, interpolate only when all of these are true:
   - missing gap is `<= max_interpolate_gap`;
   - previous and next frames share node, antenna count, frequency, and compatible subcarrier layout;
   - at least one real frame exists on both sides.
6. Mark every emitted frame as `Live` or `Interpolated`.
7. Feed calibration, proof/witness paths, and hardware health only from `Live` frames. Interpolated frames may smooth UI and short-window features but must not create calibration evidence.
8. Export per-node metrics: live received, live emitted, reordered, interpolated, missing dropped, late/duplicate, buffer depth, last/max hold, and drop ratio.

Implementation notes:

- Keep defaults conservative: `max_hold` near 75 ms, `max_reorder_gap` near 8, `max_interpolate_gap` near 3.
- Use wrapping sequence arithmetic for `u32` rollover.
- Treat parse failures and sibling packet magic as parser errors, not jitter-buffer input.
- Preserve production fail-closed behavior: no automatic fallback source when UDP quorum is absent.

## 2. Adaptive Calibration Monitor

Target code surfaces:

- `v2/crates/wifi-densepose-sensing-server/src/main.rs`
- `v2/crates/wifi-densepose-signal/src/ruvsense/calibration.rs`
- `v2/crates/wifi-densepose-signal/src/ruvsense/calibration_drift.rs`

State machine:

- `CollectingInitial`: no promoted baseline yet; accepts only live, presence-free, low-variance CSI frames.
- `Monitoring`: an active baseline is used for drift scoring.
- `CollectingCandidate`: sustained drift was confirmed and a quiet-room replacement candidate is being captured.
- `CandidateRejected`: a candidate failed stability gates and collection restarts only after a fresh quiet window.

Monitor inputs:

- `CalibrationDeviationScore` amplitude z median, phase drift median, and drift score.
- Per-subcarrier candidate amplitude variance from `CalibrationRecorder`.
- Per-subcarrier von Mises phase dispersion from the circular phase recorder.
- Motion/empty-room signals from the runtime classifier and short-window CSI variance.
- Live ESP32 CSI only; interpolated CSI is excluded before it reaches the monitor.

Decision rules:

1. Build the initial baseline only after the configured minimum frame count, defaulting to 600 live quiet frames.
2. Continue scoring each live frame against the active baseline using `CalibrationDriftDetector`.
3. Start a replacement candidate only after confirmed sustained drift and a quiet-room gate: no presence and low short-window variance.
4. Promote the replacement baseline automatically only when minimum live frames are captured and candidate amplitude variance plus phase dispersion pass thresholds.
5. Reject unstable candidates, reset the drift detector, and leave the previous baseline active.
6. Never auto-calibrate from interpolated frames, simulated frames, or unknown source frames.

API and telemetry:

- Extend calibration status output with per-node adaptive monitor state, last decision, candidate frames, drift windows, drift score, promotions, rejections, and active baseline frame count.
- Add structured logs for state transitions.
- Add Prometheus metrics for monitor state, candidate frames, baseline promotions, and candidate rejections.

## 3. Serial Baud Reconnect

Target code surfaces:

- `v2/crates/wifi-densepose-hardware/src/serial_reconnect.rs`
- `v2/crates/wifi-densepose-hardware/src/serial_reconnect_tests.rs`

Plan:

1. Keep `SerialReconnectSupervisor` as the owner of one blocking serial handle.
2. Validate `port_name`, `baud_rate`, timeouts, and buffer sizes before opening.
3. Probe baud candidates in default order `115200`, `460800`, `921600`, de-duplicating with the configured baud first.
4. On repeated open failure, report `ReconnectScheduled` with exponential backoff capped by `max_backoff`.
5. On timeout or would-block, report `Idle` without dropping the handle.
6. On zero-length read or non-timeout read error, drop the handle, increment disconnect counters, and schedule reconnect.
7. Surface the current active baud and ordered candidate list for runtime status and tests.

## 4. Test Strategy

Rust unit tests:

- Expand `udp_jitter.rs` tests for contiguous drain, timeout flush, depth flush, sequence rollover, duplicate drop, large-gap resync, and metadata-mismatch interpolation refusal.
- Add calibration monitor tests with synthetic live frames: quiet-room initial promotion, sustained quiet drift promotion, presence rejection, high phase-dispersion rejection, and drift reset after promotion.
- Keep `serial_reconnect` fake-factory tests for baud candidate ordering, candidate cycling after failures, successful reconnect reset, timeout idle behavior, zero-length disconnect, read-error disconnect, config validation, and stats.

Rust integration tests:

- Feed encoded ADR-018 frames through the parser and jitter buffer with deterministic reorder/drop patterns.
- Verify `main.rs` calibration feed paths skip `FrameKind::Interpolated`.
- Verify production source selection does not fall back to simulation when `CSI_SOURCE=esp32` and node quorum is absent.

Validation commands:

```bash
cd v2
cargo test -p ruvsense-master udp_jitter --no-default-features
cargo test -p wifi-densepose-signal calibration --no-default-features
cargo test -p wifi-densepose-hardware --no-default-features --features serial-reconnect-testkit
cargo test --workspace --no-default-features
cd ..
python archive/v1/data/proof/verify.py
```

Hardware acceptance:

- One ESP32-C6 streams live CSI over UDP for at least 30 minutes with stable node liveness and bounded jitter metrics.
- Inject packet reorder/loss on the network and verify bounded latency, bounded interpolation, and no calibration pollution.
- Unplug/replug the serial sensor and verify reconnect counters increase and live readings resume without process restart.
- Remove all nodes and verify the runtime reports unavailable/degraded live ingress instead of switching to demo, mock, or simulation.

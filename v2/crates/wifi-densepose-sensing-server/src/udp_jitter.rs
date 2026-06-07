//! Per-node UDP CSI jitter buffer and packet re-sequencer.
//!
//! The buffer is live-data only: interpolated frames are derived from adjacent
//! real CSI frames to bridge small packet gaps, never from synthetic/demo input.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use super::Esp32Frame;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameKind {
    Live,
    Interpolated,
}

#[derive(Debug, Clone)]
pub(crate) struct DeliveredFrame {
    pub(crate) frame: Esp32Frame,
    pub(crate) kind: FrameKind,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct UdpJitterConfig {
    pub(crate) max_reorder_gap: u32,
    pub(crate) max_interpolate_gap: u32,
    pub(crate) max_buffered_frames: usize,
    pub(crate) max_hold: Duration,
}

impl Default for UdpJitterConfig {
    fn default() -> Self {
        Self {
            max_reorder_gap: 8,
            max_interpolate_gap: 3,
            max_buffered_frames: 12,
            max_hold: Duration::from_millis(75),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct NodeJitterSnapshot {
    pub(crate) received_live_total: u64,
    pub(crate) emitted_live_total: u64,
    pub(crate) interpolated_total: u64,
    pub(crate) reordered_total: u64,
    pub(crate) missing_dropped_total: u64,
    pub(crate) late_or_duplicate_total: u64,
    pub(crate) buffer_depth: usize,
    pub(crate) last_hold_ms: u64,
    pub(crate) max_hold_ms: u64,
}

impl NodeJitterSnapshot {
    pub(crate) fn dropped_ratio(self) -> f64 {
        let dropped = self.missing_dropped_total + self.late_or_duplicate_total;
        let denom = self.received_live_total + self.missing_dropped_total;
        if denom == 0 {
            0.0
        } else {
            dropped as f64 / denom as f64
        }
    }
}

#[derive(Debug)]
pub(crate) struct UdpJitterBuffer {
    config: UdpJitterConfig,
    nodes: HashMap<u8, NodeJitter>,
}

impl Default for UdpJitterBuffer {
    fn default() -> Self {
        Self::new(UdpJitterConfig::default())
    }
}

impl UdpJitterBuffer {
    pub(crate) fn new(config: UdpJitterConfig) -> Self {
        Self {
            config,
            nodes: HashMap::new(),
        }
    }

    pub(crate) fn max_hold(&self) -> Duration {
        self.config.max_hold
    }

    pub(crate) fn push(&mut self, frame: Esp32Frame, now: Instant) -> Vec<DeliveredFrame> {
        self.nodes
            .entry(frame.node_id)
            .or_insert_with(|| NodeJitter::new(self.config))
            .push(frame, now)
    }

    pub(crate) fn flush_due(&mut self, now: Instant) -> Vec<DeliveredFrame> {
        let mut out = Vec::new();
        for node in self.nodes.values_mut() {
            out.extend(node.flush_due(now));
        }
        out
    }

    pub(crate) fn snapshot(&self, node_id: u8) -> Option<NodeJitterSnapshot> {
        self.nodes.get(&node_id).map(NodeJitter::snapshot)
    }
}

#[derive(Debug, Clone)]
struct BufferedFrame {
    frame: Esp32Frame,
    inserted_at: Instant,
}

#[derive(Debug)]
struct NodeJitter {
    config: UdpJitterConfig,
    expected: Option<u32>,
    last_emitted: Option<Esp32Frame>,
    buffered: BTreeMap<u32, BufferedFrame>,
    stats: NodeJitterSnapshot,
}

impl NodeJitter {
    fn new(config: UdpJitterConfig) -> Self {
        Self {
            config,
            expected: None,
            last_emitted: None,
            buffered: BTreeMap::new(),
            stats: NodeJitterSnapshot::default(),
        }
    }

    fn push(&mut self, frame: Esp32Frame, now: Instant) -> Vec<DeliveredFrame> {
        self.stats.received_live_total = self.stats.received_live_total.saturating_add(1);
        let seq = frame.sequence;
        let expected = match self.expected {
            Some(expected) => expected,
            None => return self.emit_live(frame, now),
        };

        let ahead = seq.wrapping_sub(expected);
        if ahead == 0 {
            let mut out = self.emit_live(frame, now);
            out.extend(self.drain_contiguous(now));
            return out;
        }
        if ahead > i32::MAX as u32 {
            self.stats.late_or_duplicate_total =
                self.stats.late_or_duplicate_total.saturating_add(1);
            return Vec::new();
        }
        if self.buffered.contains_key(&seq) {
            self.stats.late_or_duplicate_total =
                self.stats.late_or_duplicate_total.saturating_add(1);
            return Vec::new();
        }
        if ahead <= self.config.max_reorder_gap {
            self.buffered.insert(
                seq,
                BufferedFrame {
                    frame,
                    inserted_at: now,
                },
            );
            self.stats.buffer_depth = self.buffered.len();
            return self.flush_due(now);
        }

        self.stats.missing_dropped_total =
            self.stats.missing_dropped_total.saturating_add(ahead as u64);
        self.buffered.clear();
        self.emit_live(frame, now)
    }

    fn emit_live(&mut self, frame: Esp32Frame, now: Instant) -> Vec<DeliveredFrame> {
        self.expected = Some(frame.sequence.wrapping_add(1));
        self.last_emitted = Some(frame.clone());
        self.stats.emitted_live_total = self.stats.emitted_live_total.saturating_add(1);
        self.stats.buffer_depth = self.buffered.len();
        self.stats.last_hold_ms = 0;
        let _ = now;
        vec![DeliveredFrame {
            frame,
            kind: FrameKind::Live,
        }]
    }

    fn emit_buffered(&mut self, buffered: BufferedFrame, now: Instant) -> DeliveredFrame {
        let hold_ms = now.saturating_duration_since(buffered.inserted_at).as_millis() as u64;
        self.stats.last_hold_ms = hold_ms;
        self.stats.max_hold_ms = self.stats.max_hold_ms.max(hold_ms);
        self.stats.reordered_total = self.stats.reordered_total.saturating_add(1);
        self.stats.emitted_live_total = self.stats.emitted_live_total.saturating_add(1);
        self.expected = Some(buffered.frame.sequence.wrapping_add(1));
        self.last_emitted = Some(buffered.frame.clone());
        DeliveredFrame {
            frame: buffered.frame,
            kind: FrameKind::Live,
        }
    }

    fn drain_contiguous(&mut self, now: Instant) -> Vec<DeliveredFrame> {
        let mut out = Vec::new();
        while let Some(expected) = self.expected {
            let Some(buffered) = self.buffered.remove(&expected) else {
                break;
            };
            out.push(self.emit_buffered(buffered, now));
        }
        self.stats.buffer_depth = self.buffered.len();
        out
    }

    fn flush_due(&mut self, now: Instant) -> Vec<DeliveredFrame> {
        let Some(expected) = self.expected else {
            return Vec::new();
        };
        let Some((&head_seq, head)) = self.buffered.iter().next() else {
            self.stats.buffer_depth = 0;
            return Vec::new();
        };
        let held = now.saturating_duration_since(head.inserted_at);
        if held < self.config.max_hold && self.buffered.len() <= self.config.max_buffered_frames {
            self.stats.buffer_depth = self.buffered.len();
            return Vec::new();
        }

        let gap = head_seq.wrapping_sub(expected);
        let Some(head) = self.buffered.remove(&head_seq) else {
            return Vec::new();
        };

        let mut out = Vec::new();
        if gap <= self.config.max_interpolate_gap {
            if let Some(prev) = self.last_emitted.clone() {
                if !can_interpolate(&prev, &head.frame) {
                    self.stats.missing_dropped_total =
                        self.stats.missing_dropped_total.saturating_add(gap as u64);
                    out.push(self.emit_buffered(head, now));
                    out.extend(self.drain_contiguous(now));
                    self.stats.buffer_depth = self.buffered.len();
                    return out;
                }
                for offset in 0..gap {
                    let seq = expected.wrapping_add(offset);
                    let frac = (offset + 1) as f64 / (gap + 1) as f64;
                    let interp = interpolate_frame(&prev, &head.frame, seq, frac);
                    self.stats.interpolated_total =
                        self.stats.interpolated_total.saturating_add(1);
                    self.last_emitted = Some(interp.clone());
                    out.push(DeliveredFrame {
                        frame: interp,
                        kind: FrameKind::Interpolated,
                    });
                }
            } else {
                self.stats.missing_dropped_total =
                    self.stats.missing_dropped_total.saturating_add(gap as u64);
            }
        } else {
            self.stats.missing_dropped_total =
                self.stats.missing_dropped_total.saturating_add(gap as u64);
        }

        out.push(self.emit_buffered(head, now));
        out.extend(self.drain_contiguous(now));
        self.stats.buffer_depth = self.buffered.len();
        out
    }

    fn snapshot(&self) -> NodeJitterSnapshot {
        let mut stats = self.stats;
        stats.buffer_depth = self.buffered.len();
        stats
    }
}

fn interpolate_frame(prev: &Esp32Frame, next: &Esp32Frame, sequence: u32, frac: f64) -> Esp32Frame {
    let mut frame = next.clone();
    frame.sequence = sequence;
    frame.rssi = lerp_i8(prev.rssi, next.rssi, frac);
    frame.noise_floor = lerp_i8(prev.noise_floor, next.noise_floor, frac);
    frame.amplitudes = lerp_vec(&prev.amplitudes, &next.amplitudes, frac);
    frame.phases = lerp_phase_vec(&prev.phases, &next.phases, frac);
    frame.n_subcarriers = frame.amplitudes.len().min(u8::MAX as usize) as u8;
    frame
}

fn can_interpolate(prev: &Esp32Frame, next: &Esp32Frame) -> bool {
    prev.node_id == next.node_id
        && prev.n_antennas == next.n_antennas
        && prev.freq_mhz == next.freq_mhz
        && prev.n_subcarriers == next.n_subcarriers
        && !prev.amplitudes.is_empty()
        && prev.amplitudes.len() == next.amplitudes.len()
        && prev.phases.len() == next.phases.len()
        && prev.amplitudes.len() == prev.phases.len()
}

fn lerp_i8(a: i8, b: i8, frac: f64) -> i8 {
    (a as f64 + (b as f64 - a as f64) * frac).round() as i8
}

fn lerp_vec(a: &[f64], b: &[f64], frac: f64) -> Vec<f64> {
    let len = a.len().max(b.len());
    (0..len)
        .map(|idx| {
            let av = a.get(idx).copied().unwrap_or_else(|| b[idx]);
            let bv = b.get(idx).copied().unwrap_or(av);
            av + (bv - av) * frac
        })
        .collect()
}

fn lerp_phase_vec(a: &[f64], b: &[f64], frac: f64) -> Vec<f64> {
    let len = a.len().max(b.len());
    (0..len)
        .map(|idx| {
            let av = a.get(idx).copied().unwrap_or_else(|| b[idx]);
            let bv = b.get(idx).copied().unwrap_or(av);
            wrap_phase(av + shortest_phase_delta(av, bv) * frac)
        })
        .collect()
}

fn shortest_phase_delta(a: f64, b: f64) -> f64 {
    wrap_phase(b - a)
}

fn wrap_phase(mut phase: f64) -> f64 {
    let two_pi = std::f64::consts::TAU;
    phase = (phase + std::f64::consts::PI).rem_euclid(two_pi) - std::f64::consts::PI;
    if phase == -std::f64::consts::PI {
        std::f64::consts::PI
    } else {
        phase
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(seq: u32) -> Esp32Frame {
        Esp32Frame {
            magic: wifi_densepose_hardware::ESP32_CSI_MAGIC,
            node_id: 7,
            n_antennas: 1,
            n_subcarriers: 2,
            freq_mhz: 2437,
            sequence: seq,
            rssi: -50,
            noise_floor: -95,
            amplitudes: vec![seq as f64, seq as f64 + 10.0],
            phases: vec![seq as f64 * 0.1, seq as f64 * 0.2],
        }
    }

    #[test]
    fn emits_ordered_frames_immediately() {
        let mut b = UdpJitterBuffer::default();
        let t = Instant::now();
        assert_eq!(b.push(frame(1), t)[0].frame.sequence, 1);
        assert_eq!(b.push(frame(2), t)[0].frame.sequence, 2);
        assert_eq!(b.snapshot(7).unwrap().emitted_live_total, 2);
    }

    #[test]
    fn reorders_short_out_of_order_gap() {
        let mut b = UdpJitterBuffer::default();
        let t = Instant::now();
        let _ = b.push(frame(1), t);
        assert!(b.push(frame(3), t).is_empty());
        let out = b.push(frame(2), t + Duration::from_millis(10));
        let seqs: Vec<u32> = out.iter().map(|f| f.frame.sequence).collect();
        assert_eq!(seqs, vec![2, 3]);
        assert_eq!(b.snapshot(7).unwrap().reordered_total, 1);
    }

    #[test]
    fn interpolates_small_missing_gap_after_hold() {
        let mut b = UdpJitterBuffer::new(UdpJitterConfig {
            max_hold: Duration::from_millis(10),
            ..UdpJitterConfig::default()
        });
        let t = Instant::now();
        let _ = b.push(frame(10), t);
        assert!(b.push(frame(12), t).is_empty());
        let out = b.push(frame(13), t + Duration::from_millis(20));
        let seqs: Vec<(u32, FrameKind)> = out.iter().map(|f| (f.frame.sequence, f.kind)).collect();
        assert_eq!(seqs[0], (11, FrameKind::Interpolated));
        assert_eq!(seqs[1], (12, FrameKind::Live));
        assert_eq!(seqs[2], (13, FrameKind::Live));
        assert!((out[0].frame.amplitudes[0] - 11.0).abs() < 1e-9);
    }

    #[test]
    fn skips_large_gap_without_interpolation() {
        let mut b = UdpJitterBuffer::default();
        let t = Instant::now();
        let _ = b.push(frame(1), t);
        let out = b.push(frame(40), t);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].frame.sequence, 40);
        assert!(b.snapshot(7).unwrap().missing_dropped_total > 0);
    }

    #[test]
    fn drops_late_duplicate_frame() {
        let mut b = UdpJitterBuffer::default();
        let t = Instant::now();
        let _ = b.push(frame(1), t);
        assert!(b.push(frame(1), t).is_empty());
        assert_eq!(b.snapshot(7).unwrap().late_or_duplicate_total, 1);
    }

    #[test]
    fn handles_sequence_wraparound() {
        let mut b = UdpJitterBuffer::default();
        let t = Instant::now();
        let _ = b.push(frame(u32::MAX), t);
        let out = b.push(frame(0), t);
        assert_eq!(out[0].frame.sequence, 0);
    }

    #[test]
    fn timer_flush_emits_due_buffered_gap_without_new_packet() {
        let mut b = UdpJitterBuffer::new(UdpJitterConfig {
            max_hold: Duration::from_millis(10),
            ..UdpJitterConfig::default()
        });
        let t = Instant::now();
        let _ = b.push(frame(20), t);
        assert!(b.push(frame(22), t).is_empty());

        let out = b.flush_due(t + Duration::from_millis(11));
        let seqs: Vec<(u32, FrameKind)> = out.iter().map(|f| (f.frame.sequence, f.kind)).collect();
        assert_eq!(seqs, vec![(21, FrameKind::Interpolated), (22, FrameKind::Live)]);
        assert_eq!(b.snapshot(7).unwrap().buffer_depth, 0);
    }

    #[test]
    fn timer_flush_waits_until_hold_expires() {
        let mut b = UdpJitterBuffer::new(UdpJitterConfig {
            max_hold: Duration::from_millis(10),
            ..UdpJitterConfig::default()
        });
        let t = Instant::now();
        let _ = b.push(frame(20), t);
        assert!(b.push(frame(22), t).is_empty());

        assert!(b.flush_due(t + Duration::from_millis(9)).is_empty());
        let snap = b.snapshot(7).unwrap();
        assert_eq!(snap.buffer_depth, 1);
        assert_eq!(snap.interpolated_total, 0);
    }

    #[test]
    fn metadata_mismatch_drops_gap_without_interpolation() {
        let mut b = UdpJitterBuffer::new(UdpJitterConfig {
            max_hold: Duration::from_millis(10),
            ..UdpJitterConfig::default()
        });
        let t = Instant::now();
        let _ = b.push(frame(20), t);
        let mut changed_channel = frame(22);
        changed_channel.freq_mhz = 5180;
        assert!(b.push(changed_channel, t).is_empty());

        let out = b.flush_due(t + Duration::from_millis(11));
        let seqs: Vec<(u32, FrameKind)> = out.iter().map(|f| (f.frame.sequence, f.kind)).collect();
        assert_eq!(seqs, vec![(22, FrameKind::Live)]);
        let snap = b.snapshot(7).unwrap();
        assert_eq!(snap.interpolated_total, 0);
        assert_eq!(snap.missing_dropped_total, 1);
    }

    #[test]
    fn phase_interpolation_uses_shortest_arc_across_wrap() {
        let mut prev = frame(1);
        let mut next = frame(3);
        prev.phases = vec![3.10];
        next.phases = vec![-3.10];

        let interp = interpolate_frame(&prev, &next, 2, 0.5);
        assert!(
            interp.phases[0].abs() > 3.0,
            "phase should stay near +/-pi across wrap, got {}",
            interp.phases[0]
        );
    }
}

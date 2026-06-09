//! Skeleton derivation, pose estimation, and temporal smoothing.

use crate::types::*;

/// Expected bone lengths in pixel-space for the COCO-17 skeleton.
pub const POSE_BONE_PAIRS: &[(usize, usize)] = &[
    (5, 7),
    (7, 9),
    (6, 8),
    (8, 10),
    (5, 11),
    (6, 12),
    (11, 13),
    (13, 15),
    (12, 14),
    (14, 16),
    (5, 6),
    (11, 12),
];

const TORSO_KP: [usize; 4] = [5, 6, 11, 12];
const EXTREMITY_KP: [usize; 4] = [9, 10, 15, 16];
const UNLOCALIZED_POSITION_SOURCE: &str = "unlocalized";
const SENSOR_GEOMETRY_POSE_SOURCE: &str = "sensor_geometry";
const RSSI_CSI_POSE_SOURCE: &str = "rssi_csi_trilateration";
const RSSI_CSI_SINGLE_NODE_SOURCE: &str = "rssi_csi_single_node";
const SYNTHETIC_DEV_POSE_SOURCE: &str = "synthetic_dev";
const HUMAN_HEIGHT_M: f64 = 1.70;
const FOOT_CLEARANCE_M: f64 = 0.04;

fn is_unlocalized_origin(position: &[f64; 3]) -> bool {
    position.iter().all(|v| v.abs() < 1e-9)
}

fn is_localized_position(position: &[f64; 3]) -> bool {
    position.iter().all(|v| v.is_finite()) && !is_unlocalized_origin(position)
}

fn is_synthetic_dev_source(source: &str) -> bool {
    matches!(source, "simulated" | "simulate" | "synthetic")
}

fn estimate_person_world_position(
    update: &SensingUpdate,
    person_idx: usize,
    total_persons: usize,
) -> (Option<[f64; 3]>, &'static str) {
    if let Some(tracking) = update.state.as_ref() {
        if let Some(person) = tracking.persons.get(person_idx) {
            let source = match person.source.as_str() {
                SYNTHETIC_DEV_POSE_SOURCE => SYNTHETIC_DEV_POSE_SOURCE,
                RSSI_CSI_SINGLE_NODE_SOURCE => RSSI_CSI_SINGLE_NODE_SOURCE,
                RSSI_CSI_POSE_SOURCE => RSSI_CSI_POSE_SOURCE,
                _ => SENSOR_GEOMETRY_POSE_SOURCE,
            };
            return (Some(person.position_m), source);
        }
    }

    let nodes: Vec<&NodeInfo> = update
        .nodes
        .iter()
        .filter(|node| is_localized_position(&node.position))
        .collect();
    if nodes.is_empty() {
        return (None, UNLOCALIZED_POSITION_SOURCE);
    }

    let mut weight_sum = 0.0;
    let mut centroid = [0.0, 0.0, 0.0];
    let mut min_x = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut min_z = f64::INFINITY;
    let mut max_z = f64::NEG_INFINITY;

    for node in &nodes {
        let weight = ((node.rssi_dbm + 100.0) / 60.0).clamp(0.2, 1.0);
        weight_sum += weight;
        centroid[0] += node.position[0] * weight;
        centroid[1] += node.position[1] * weight;
        centroid[2] += node.position[2] * weight;
        min_x = min_x.min(node.position[0]);
        max_x = max_x.max(node.position[0]);
        min_z = min_z.min(node.position[2]);
        max_z = max_z.max(node.position[2]);
    }

    centroid[0] /= weight_sum.max(1e-9);
    centroid[1] /= weight_sum.max(1e-9);
    centroid[2] /= weight_sum.max(1e-9);

    let half = (total_persons as f64 - 1.0) / 2.0;
    let spread_index = person_idx as f64 - half;
    let spread_x =
        ((max_x - min_x).abs() / (total_persons.max(1) as f64 + 1.0)).clamp(0.45, 1.2);
    let spread_z = ((max_z - min_z).abs() * 0.08).clamp(0.0, 0.35);
    let mut position = [
        centroid[0] + spread_index * spread_x,
        0.9,
        centroid[2] + spread_index.signum() * spread_z,
    ];

    if nodes.len() == 1 {
        position[2] += 0.8;
    }

    let source = if is_synthetic_dev_source(&update.source) {
        SYNTHETIC_DEV_POSE_SOURCE
    } else {
        SENSOR_GEOMETRY_POSE_SOURCE
    };

    (Some(position), source)
}

fn mean_keypoint_x(keypoints: &[PoseKeypoint], indexes: &[usize]) -> Option<f64> {
    let mut sum = 0.0;
    let mut count = 0usize;
    for &idx in indexes {
        if let Some(kp) = keypoints.get(idx) {
            if kp.x.is_finite() {
                sum += kp.x;
                count += 1;
            }
        }
    }
    (count > 0).then_some(sum / count as f64)
}

fn metric_keypoints_from_legacy(
    keypoints: &[PoseKeypoint],
    position_m: [f64; 3],
) -> Option<Vec<PoseKeypoint>> {
    if keypoints.len() < 17 || !is_localized_position(&position_m) {
        return None;
    }

    let min_y = keypoints
        .iter()
        .map(|kp| kp.y)
        .filter(|v| v.is_finite())
        .fold(f64::INFINITY, f64::min);
    let max_y = keypoints
        .iter()
        .map(|kp| kp.y)
        .filter(|v| v.is_finite())
        .fold(f64::NEG_INFINITY, f64::max);
    let height_px = max_y - min_y;
    if !height_px.is_finite() || height_px < 80.0 {
        return None;
    }

    let hip_x = mean_keypoint_x(keypoints, &[11, 12])
        .or_else(|| mean_keypoint_x(keypoints, &[5, 6]))
        .unwrap_or_else(|| keypoints.iter().map(|kp| kp.x).sum::<f64>() / keypoints.len() as f64);
    let center_z = keypoints.iter().map(|kp| kp.z).sum::<f64>() / keypoints.len() as f64;
    let y_scale = (HUMAN_HEIGHT_M - FOOT_CLEARANCE_M) / height_px;

    let keypoints_m: Vec<PoseKeypoint> = keypoints
        .iter()
        .map(|kp| PoseKeypoint {
            name: kp.name.clone(),
            x: position_m[0] + (kp.x - hip_x) * y_scale,
            y: FOOT_CLEARANCE_M + (max_y - kp.y) * y_scale,
            z: position_m[2] + (kp.z - center_z) * y_scale,
            confidence: kp.confidence,
        })
        .collect();

    let foot_y = keypoints_m
        .get(15)
        .zip(keypoints_m.get(16))
        .map(|(l, r)| l.y.min(r.y))
        .unwrap_or(f64::INFINITY);
    let derived_height = keypoints_m
        .iter()
        .map(|kp| kp.y)
        .fold(f64::NEG_INFINITY, f64::max)
        - foot_y;
    if (1.45..=1.95).contains(&derived_height) && (0.0..=0.12).contains(&foot_y) {
        Some(keypoints_m)
    } else {
        None
    }
}

pub fn derive_single_person_pose(
    update: &SensingUpdate,
    person_idx: usize,
    total_persons: usize,
) -> PersonDetection {
    let cls = &update.classification;
    let feat = &update.features;

    let phase_offset = person_idx as f64 * 2.094;
    let half = (total_persons as f64 - 1.0) / 2.0;
    let person_x_offset = (person_idx as f64 - half) * 120.0;
    let conf_decay = 1.0 - person_idx as f64 * 0.15;

    let motion_score = (feat.motion_band_power / 15.0).clamp(0.0, 1.0);
    let is_walking = motion_score > 0.55;
    let breath_amp = (feat.breathing_band_power * 4.0).clamp(0.0, 12.0);

    let breath_phase = if let Some(ref vs) = update.vital_signs {
        let bpm = vs.breathing_rate_bpm.unwrap_or(15.0);
        let freq = (bpm / 60.0).clamp(0.1, 0.5);
        (update.tick as f64 * freq * 0.02 * std::f64::consts::TAU + phase_offset).sin()
    } else {
        (update.tick as f64 * 0.02 + phase_offset).sin()
    };

    let lean_x = (feat.dominant_freq_hz / 5.0 - 1.0).clamp(-1.0, 1.0) * 18.0;
    let stride_x = if is_walking {
        let stride_phase =
            (feat.motion_band_power * 0.7 + update.tick as f64 * 0.06 + phase_offset).sin();
        stride_phase * 20.0 * motion_score
    } else {
        0.0
    };

    let burst = (feat.change_points as f64 / 20.0).clamp(0.0, 0.3);
    let noise_seed = person_idx as f64 * 97.1;
    let noise_val = (noise_seed.sin() * 43758.545).fract();
    let snr_factor = ((feat.variance - 0.5) / 10.0).clamp(0.0, 1.0);
    let base_confidence = cls.confidence * (0.6 + 0.4 * snr_factor) * conf_decay;

    let base_x = 320.0 + stride_x + lean_x * 0.5 + person_x_offset;
    let base_y = 240.0 - motion_score * 8.0;

    let kp_names = [
        "nose",
        "left_eye",
        "right_eye",
        "left_ear",
        "right_ear",
        "left_shoulder",
        "right_shoulder",
        "left_elbow",
        "right_elbow",
        "left_wrist",
        "right_wrist",
        "left_hip",
        "right_hip",
        "left_knee",
        "right_knee",
        "left_ankle",
        "right_ankle",
    ];

    let kp_offsets: [(f64, f64); 17] = [
        (0.0, -80.0),
        (-8.0, -88.0),
        (8.0, -88.0),
        (-16.0, -82.0),
        (16.0, -82.0),
        (-30.0, -50.0),
        (30.0, -50.0),
        (-45.0, -15.0),
        (45.0, -15.0),
        (-50.0, 20.0),
        (50.0, 20.0),
        (-20.0, 20.0),
        (20.0, 20.0),
        (-22.0, 70.0),
        (22.0, 70.0),
        (-24.0, 120.0),
        (24.0, 120.0),
    ];

    let keypoints: Vec<PoseKeypoint> = kp_names
        .iter()
        .zip(kp_offsets.iter())
        .enumerate()
        .map(|(i, (name, (dx, dy)))| {
            let breath_dx = if TORSO_KP.contains(&i) {
                let sign = if *dx < 0.0 { -1.0 } else { 1.0 };
                sign * breath_amp * breath_phase * 0.5
            } else {
                0.0
            };
            let breath_dy = if TORSO_KP.contains(&i) {
                let sign = if *dy < 0.0 { -1.0 } else { 1.0 };
                sign * breath_amp * breath_phase * 0.3
            } else {
                0.0
            };

            let extremity_jitter = if EXTREMITY_KP.contains(&i) {
                let phase = noise_seed + i as f64 * 2.399;
                (
                    phase.sin() * burst * motion_score * 4.0,
                    (phase * 1.31).cos() * burst * motion_score * 3.0,
                )
            } else {
                (0.0, 0.0)
            };

            let kp_noise_x = ((noise_seed + i as f64 * 1.618).sin() * 43758.545).fract()
                * feat.variance.sqrt().clamp(0.0, 3.0)
                * motion_score;
            let kp_noise_y = ((noise_seed + i as f64 * std::f64::consts::E).cos() * 31415.926)
                .fract()
                * feat.variance.sqrt().clamp(0.0, 3.0)
                * motion_score
                * 0.6;

            let swing_dy = if is_walking {
                let stride_phase =
                    (feat.motion_band_power * 0.7 + update.tick as f64 * 0.12 + phase_offset).sin();
                match i {
                    7 | 9 => -stride_phase * 20.0 * motion_score,
                    8 | 10 => stride_phase * 20.0 * motion_score,
                    13 | 15 => stride_phase * 25.0 * motion_score,
                    14 | 16 => -stride_phase * 25.0 * motion_score,
                    _ => 0.0,
                }
            } else {
                0.0
            };

            let final_x = base_x + dx + breath_dx + extremity_jitter.0 + kp_noise_x;
            let final_y = base_y + dy + breath_dy + extremity_jitter.1 + kp_noise_y + swing_dy;

            let kp_conf = if EXTREMITY_KP.contains(&i) {
                base_confidence * (0.7 + 0.3 * snr_factor) * (0.85 + 0.15 * noise_val)
            } else {
                base_confidence * (0.88 + 0.12 * ((i as f64 * 0.7 + noise_seed).cos()))
            };

            PoseKeypoint {
                name: name.to_string(),
                x: final_x,
                y: final_y,
                z: lean_x * 0.02,
                confidence: kp_conf.clamp(0.1, 1.0),
            }
        })
        .collect();

    let xs: Vec<f64> = keypoints.iter().map(|k| k.x).collect();
    let ys: Vec<f64> = keypoints.iter().map(|k| k.y).collect();
    let min_x = xs.iter().cloned().fold(f64::MAX, f64::min) - 10.0;
    let min_y = ys.iter().cloned().fold(f64::MAX, f64::min) - 10.0;
    let max_x = xs.iter().cloned().fold(f64::MIN, f64::max) + 10.0;
    let max_y = ys.iter().cloned().fold(f64::MIN, f64::max) + 10.0;
    let (position_m, position_source) =
        estimate_person_world_position(update, person_idx, total_persons);
    let keypoints_m =
        position_m.and_then(|position| metric_keypoints_from_legacy(&keypoints, position));
    let pose_source = if keypoints_m.is_some() {
        position_source
    } else if is_synthetic_dev_source(&update.source) {
        SYNTHETIC_DEV_POSE_SOURCE
    } else {
        position_source
    };

    PersonDetection {
        id: (person_idx + 1) as u32,
        confidence: cls.confidence * conf_decay,
        keypoints,
        keypoints_m,
        bbox: BoundingBox {
            x: min_x,
            y: min_y,
            width: (max_x - min_x).max(80.0),
            height: (max_y - min_y).max(160.0),
        },
        zone: format!("zone_{}", person_idx + 1),
        position_m,
        position: position_m,
        position_source: Some(position_source.to_string()),
        pose_source: Some(pose_source.to_string()),
    }
}

pub fn derive_pose_from_sensing(update: &SensingUpdate) -> Vec<PersonDetection> {
    let cls = &update.classification;
    if !cls.presence {
        return vec![];
    }
    let person_count = update.estimated_persons.unwrap_or(1).max(1);
    (0..person_count)
        .map(|idx| derive_single_person_pose(update, idx, person_count))
        .collect()
}

/// Apply temporal EMA smoothing and bone-length clamping to person detections.
pub fn apply_temporal_smoothing(persons: &mut [PersonDetection], ns: &mut NodeState) {
    if persons.is_empty() {
        return;
    }

    let alpha = ns.ema_alpha();
    let person = &mut persons[0];

    let current_kps: Vec<[f64; 3]> = person
        .keypoints
        .iter()
        .map(|kp| [kp.x, kp.y, kp.z])
        .collect();

    let smoothed = if let Some(ref prev) = ns.prev_keypoints {
        let mut out = Vec::with_capacity(current_kps.len());
        for (cur, prv) in current_kps.iter().zip(prev.iter()) {
            out.push([
                alpha * cur[0] + (1.0 - alpha) * prv[0],
                alpha * cur[1] + (1.0 - alpha) * prv[1],
                alpha * cur[2] + (1.0 - alpha) * prv[2],
            ]);
        }
        clamp_bone_lengths_f64(&mut out, prev);
        out
    } else {
        current_kps.clone()
    };

    for (kp, s) in person.keypoints.iter_mut().zip(smoothed.iter()) {
        kp.x = s[0];
        kp.y = s[1];
        kp.z = s[2];
    }
    ns.prev_keypoints = Some(smoothed);
}

fn clamp_bone_lengths_f64(pose: &mut [[f64; 3]], prev: &[[f64; 3]]) {
    for &(p, c) in POSE_BONE_PAIRS {
        if p >= pose.len() || c >= pose.len() {
            continue;
        }
        let prev_len = dist_f64(&prev[p], &prev[c]);
        if prev_len < 1e-6 {
            continue;
        }
        let cur_len = dist_f64(&pose[p], &pose[c]);
        if cur_len < 1e-6 {
            continue;
        }
        let ratio = cur_len / prev_len;
        let lo = 1.0 - MAX_BONE_CHANGE_RATIO;
        let hi = 1.0 + MAX_BONE_CHANGE_RATIO;
        if ratio < lo || ratio > hi {
            let target = prev_len * ratio.clamp(lo, hi);
            let scale = target / cur_len;
            for dim in 0..3 {
                let diff = pose[c][dim] - pose[p][dim];
                pose[c][dim] = pose[p][dim] + diff * scale;
            }
        }
    }
}

fn dist_f64(a: &[f64; 3], b: &[f64; 3]) -> f64 {
    let dx = b[0] - a[0];
    let dy = b[1] - a[1];
    let dz = b[2] - a[2];
    (dx * dx + dy * dy + dz * dz).sqrt()
}

//! RSSI-based room localization for ESP32 CSI nodes.

use serde::{Deserialize, Serialize};
use std::sync::{Arc, OnceLock, RwLock};

const TX_POWER_DBM: f32 = -30.0;
const PATH_LOSS_EXPONENT: f32 = 2.5;
const MIN_DISTANCE_M: f32 = 0.20;
const MAX_SPEED_MPS: f32 = 2.5;
const SMOOTHER_RESET_GAP_MS: u64 = 2_500;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct NodePosition {
    pub node_id: u8,
    pub x: f32,
    pub y: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Point2D {
    pub x: f32,
    pub y: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoomShape {
    pub shape: String,
    pub boundary: Vec<Point2D>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeConfig {
    pub id: u32,
    pub x: f32,
    pub y: f32,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoomConfig {
    pub version: u32,
    pub room: RoomShape,
    pub nodes: Vec<NodeConfig>,
}

impl Default for RoomConfig {
    fn default() -> Self {
        Self {
            version: 2,
            room: RoomShape {
                shape: "polygon".to_string(),
                boundary: Vec::new(),
            },
            nodes: Vec::new(),
        }
    }
}

pub type SharedRoomConfig = Arc<RwLock<RoomConfig>>;

static ROOM_CONFIG_STATE: OnceLock<SharedRoomConfig> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PersonLocation {
    pub x: f32,
    pub y: f32,
    pub confidence: f32,
    pub timestamp_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct NodeSignalReading {
    pub node_id: u8,
    pub rssi_dbm: f32,
    pub csi_quality: f32,
}

#[derive(Debug, Clone, Copy)]
struct RangedNode {
    position: NodePosition,
    distance_m: f32,
    weight: f32,
}

#[derive(Debug, Clone, Copy)]
struct LocationFilterState {
    x: f32,
    y: f32,
    timestamp_ms: u64,
}

#[derive(Debug, Clone, Default)]
pub struct LocationSmoother {
    state: Option<LocationFilterState>,
}

impl LocationSmoother {
    pub fn reset(&mut self) {
        self.state = None;
    }

    pub fn update(&mut self, measurement: PersonLocation) -> PersonLocation {
        if !measurement.x.is_finite() || !measurement.y.is_finite() {
            self.reset();
            return measurement;
        }

        let Some(prev) = self.state else {
            self.state = Some(LocationFilterState {
                x: measurement.x,
                y: measurement.y,
                timestamp_ms: measurement.timestamp_ms,
            });
            return measurement;
        };

        let elapsed_ms = measurement.timestamp_ms.saturating_sub(prev.timestamp_ms);
        if elapsed_ms == 0 || elapsed_ms > SMOOTHER_RESET_GAP_MS {
            self.state = Some(LocationFilterState {
                x: measurement.x,
                y: measurement.y,
                timestamp_ms: measurement.timestamp_ms,
            });
            return measurement;
        }

        let dt = (elapsed_ms as f32 / 1000.0).clamp(0.02, 0.5);
        let mut dx = measurement.x - prev.x;
        let mut dy = measurement.y - prev.y;
        let distance = (dx * dx + dy * dy).sqrt();
        let max_step = MAX_SPEED_MPS * dt;
        if distance > max_step && distance.is_finite() {
            let scale = max_step / distance;
            dx *= scale;
            dy *= scale;
        }
        let confidence = measurement.confidence.clamp(0.0, 1.0);
        let alpha = 0.22 + 0.48 * confidence;
        let smoothed = LocationFilterState {
            x: prev.x + alpha * dx,
            y: prev.y + alpha * dy,
            timestamp_ms: measurement.timestamp_ms,
        };
        self.state = Some(smoothed);

        PersonLocation {
            x: smoothed.x,
            y: smoothed.y,
            confidence: measurement.confidence,
            timestamp_ms: measurement.timestamp_ms,
        }
    }
}

pub fn new_room_config_state(config: RoomConfig) -> SharedRoomConfig {
    Arc::new(RwLock::new(config))
}

pub fn install_room_config_state(state: SharedRoomConfig) {
    if ROOM_CONFIG_STATE.set(Arc::clone(&state)).is_err() {
        if let Ok(config) = state.read().map(|guard| guard.clone()) {
            let _ = update_room_config_state(&room_config_state(), config);
        }
    }
}

pub fn room_config_state() -> SharedRoomConfig {
    Arc::clone(ROOM_CONFIG_STATE.get_or_init(|| new_room_config_state(RoomConfig::default())))
}

pub fn update_room_config_state(
    state: &SharedRoomConfig,
    config: RoomConfig,
) -> Result<(), String> {
    let mut guard = state
        .write()
        .map_err(|_| "room config state lock poisoned".to_string())?;
    *guard = config;
    Ok(())
}

pub fn node_positions_from_room_config(config: &RoomConfig) -> Vec<NodePosition> {
    let mut positions: Vec<NodePosition> = config
        .nodes
        .iter()
        .filter(|node| node.active)
        .filter(|node| node.id != 0 && node.id <= u8::MAX as u32)
        .filter(|node| node.x.is_finite() && node.y.is_finite())
        .map(|node| NodePosition {
            node_id: node.id as u8,
            x: node.x,
            y: node.y,
        })
        .collect();
    positions.sort_by_key(|position| position.node_id);
    positions
}

pub fn node_positions_from_room_config_state(state: &SharedRoomConfig) -> Vec<NodePosition> {
    state
        .read()
        .map(|guard| node_positions_from_room_config(&guard))
        .unwrap_or_default()
}

pub fn parse_node_positions(input: &str) -> Result<Vec<NodePosition>, String> {
    let mut positions = Vec::new();
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(positions);
    }

    for entry in trimmed.split(',') {
        let parts: Vec<_> = entry.split(':').map(str::trim).collect();
        if parts.len() != 3 {
            return Err(format!(
                "invalid node position '{entry}', expected node_id:x_meters:y_meters"
            ));
        }

        let node_id = parts[0]
            .parse::<u8>()
            .map_err(|_| format!("invalid node_id '{}'", parts[0]))?;
        let x = parts[1]
            .parse::<f32>()
            .map_err(|_| format!("invalid x coordinate '{}'", parts[1]))?;
        let y = parts[2]
            .parse::<f32>()
            .map_err(|_| format!("invalid y coordinate '{}'", parts[2]))?;

        if node_id == 0 {
            return Err("node_id 0 is reserved".to_string());
        }
        if !x.is_finite() || !y.is_finite() {
            return Err(format!("node {node_id} has non-finite coordinates"));
        }

        positions.push(NodePosition { node_id, x, y });
    }

    Ok(positions)
}

pub fn rssi_to_distance_m(rssi_dbm: f32) -> f32 {
    10.0_f32.powf((TX_POWER_DBM - rssi_dbm) / (10.0 * PATH_LOSS_EXPONENT))
}

pub fn confidence_for_node_count(node_count: usize) -> f32 {
    match node_count {
        0 | 1 => 0.0,
        2 => 0.5,
        _ => 1.0,
    }
}

pub fn locatable_node_count(node_rssi: &[(u8, f32)], positions: &[NodePosition]) -> usize {
    node_rssi
        .iter()
        .filter(|&&(node_id, rssi)| {
            rssi.is_finite() && find_position(positions, node_id).is_some()
        })
        .count()
}

pub fn locate_person(node_rssi: &[(u8, f32)]) -> Option<(f32, f32)> {
    let positions = node_positions_from_room_config_state(&room_config_state());
    locate_person_with_positions(node_rssi, &positions)
}

pub fn locate_person_with_positions(
    node_rssi: &[(u8, f32)],
    positions: &[NodePosition],
) -> Option<(f32, f32)> {
    let readings: Vec<NodeSignalReading> = node_rssi
        .iter()
        .map(|(node_id, rssi_dbm)| NodeSignalReading {
            node_id: *node_id,
            rssi_dbm: *rssi_dbm,
            csi_quality: 1.0,
        })
        .collect();
    locate_person_with_signal_readings(&readings, positions)
}

pub fn locatable_signal_count(
    readings: &[NodeSignalReading],
    positions: &[NodePosition],
) -> usize {
    readings
        .iter()
        .filter(|reading| {
            reading.rssi_dbm.is_finite() && find_position(positions, reading.node_id).is_some()
        })
        .count()
}

pub fn locate_person_with_signal_readings(
    readings: &[NodeSignalReading],
    positions: &[NodePosition],
) -> Option<(f32, f32)> {
    let ranged = ranged_nodes(readings, positions);
    if ranged.len() < 2 {
        return None;
    }

    let estimate = if ranged.len() == 2 {
        two_node_projection(&ranged[0], &ranged[1])
    } else {
        trilaterate_weighted(&ranged)
            .or_else(|| Some(proximity_centroid(&ranged)))
            .map(|point| clamp_to_anchor_bounds(point, &ranged))
    }?;

    Some(estimate)
}

pub fn estimate_person_location_with_signal_readings(
    readings: &[NodeSignalReading],
    positions: &[NodePosition],
    timestamp_ms: u64,
) -> Option<PersonLocation> {
    let node_count = locatable_signal_count(readings, positions);
    if node_count == 0 {
        return None;
    }

    let (x, y) = if node_count == 1 {
        let position = readings.iter().find_map(|reading| {
            reading
                .rssi_dbm
                .is_finite()
                .then(|| find_position(positions, reading.node_id))
                .flatten()
        })?;
        (position.x, position.y)
    } else {
        locate_person_with_signal_readings(readings, positions)?
    };

    Some(PersonLocation {
        x,
        y,
        confidence: confidence_for_node_count(node_count),
        timestamp_ms,
    })
}

pub fn estimate_person_location(
    node_rssi: &[(u8, f32)],
    timestamp_ms: u64,
) -> Option<PersonLocation> {
    let positions = node_positions_from_room_config_state(&room_config_state());
    estimate_person_location_with_positions(node_rssi, &positions, timestamp_ms)
}

pub fn estimate_person_location_with_positions(
    node_rssi: &[(u8, f32)],
    positions: &[NodePosition],
    timestamp_ms: u64,
) -> Option<PersonLocation> {
    let readings: Vec<NodeSignalReading> = node_rssi
        .iter()
        .map(|(node_id, rssi_dbm)| NodeSignalReading {
            node_id: *node_id,
            rssi_dbm: *rssi_dbm,
            csi_quality: 1.0,
        })
        .collect();
    estimate_person_location_with_signal_readings(&readings, positions, timestamp_ms)
}

fn find_position(positions: &[NodePosition], node_id: u8) -> Option<NodePosition> {
    positions
        .iter()
        .copied()
        .find(|position| position.node_id == node_id)
}

fn ranged_nodes(readings: &[NodeSignalReading], positions: &[NodePosition]) -> Vec<RangedNode> {
    let mut nodes: Vec<RangedNode> = readings
        .iter()
        .filter_map(|reading| {
            if !reading.rssi_dbm.is_finite() {
                return None;
            }
            let position = find_position(positions, reading.node_id)?;
            let raw_distance = rssi_to_distance_m(reading.rssi_dbm).max(MIN_DISTANCE_M);
            let rssi_quality = ((reading.rssi_dbm + 95.0) / 45.0).clamp(0.15, 1.0);
            let csi_quality = reading.csi_quality.clamp(0.20, 1.0);
            Some(RangedNode {
                position,
                distance_m: raw_distance,
                weight: (rssi_quality * csi_quality).max(0.05),
            })
        })
        .collect();

    normalize_ranges_to_layout(&mut nodes);
    for node in &mut nodes {
        node.weight /= 0.25 + node.distance_m * node.distance_m;
    }
    nodes.sort_by_key(|node| node.position.node_id);
    nodes
}

fn normalize_ranges_to_layout(nodes: &mut [RangedNode]) {
    if nodes.len() < 2 {
        return;
    }
    let span = layout_span(nodes);
    if !span.is_finite() || span < 0.5 {
        return;
    }
    let mut distances: Vec<f32> = nodes
        .iter()
        .map(|node| node.distance_m)
        .filter(|distance| distance.is_finite())
        .collect();
    if distances.is_empty() {
        return;
    }
    distances.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = distances[distances.len() / 2].max(MIN_DISTANCE_M);
    let target_median = (span * 0.70).max(MIN_DISTANCE_M);
    let scale = (target_median / median).clamp(0.12, 2.5);
    for node in nodes {
        node.distance_m = (node.distance_m * scale).max(MIN_DISTANCE_M);
    }
}

fn layout_span(nodes: &[RangedNode]) -> f32 {
    let mut span = 0.0_f32;
    for i in 0..nodes.len() {
        for j in i + 1..nodes.len() {
            let dx = nodes[i].position.x - nodes[j].position.x;
            let dy = nodes[i].position.y - nodes[j].position.y;
            span = span.max((dx * dx + dy * dy).sqrt());
        }
    }
    span
}

fn proximity_centroid(nodes: &[RangedNode]) -> (f32, f32) {
    let mut weighted_x = 0.0_f32;
    let mut weighted_y = 0.0_f32;
    let mut total_weight = 0.0_f32;
    for node in nodes {
        let weight = node.weight.max(0.0);
        weighted_x += node.position.x * weight;
        weighted_y += node.position.y * weight;
        total_weight += weight;
    }
    if total_weight > 0.0 {
        (weighted_x / total_weight, weighted_y / total_weight)
    } else {
        (nodes[0].position.x, nodes[0].position.y)
    }
}

fn two_node_projection(a: &RangedNode, b: &RangedNode) -> Option<(f32, f32)> {
    let dx = b.position.x - a.position.x;
    let dy = b.position.y - a.position.y;
    let baseline = (dx * dx + dy * dy).sqrt();
    if !baseline.is_finite() || baseline < 1.0e-4 {
        return None;
    }
    let along =
        (a.distance_m * a.distance_m - b.distance_m * b.distance_m + baseline * baseline)
            / (2.0 * baseline);
    let t = (along / baseline).clamp(-0.10, 1.10);
    Some((a.position.x + dx * t, a.position.y + dy * t))
}

fn trilaterate_weighted(nodes: &[RangedNode]) -> Option<(f32, f32)> {
    let reference = nodes[0];
    let mut ata00 = 0.0_f32;
    let mut ata01 = 0.0_f32;
    let mut ata11 = 0.0_f32;
    let mut atb0 = 0.0_f32;
    let mut atb1 = 0.0_f32;

    for node in nodes.iter().skip(1) {
        let ax = 2.0 * (node.position.x - reference.position.x);
        let ay = 2.0 * (node.position.y - reference.position.y);
        let b = reference.distance_m * reference.distance_m
            - node.distance_m * node.distance_m
            + node.position.x * node.position.x
            - reference.position.x * reference.position.x
            + node.position.y * node.position.y
            - reference.position.y * reference.position.y;
        let w = (reference.weight * node.weight).sqrt().max(1.0e-4);
        ata00 += w * ax * ax;
        ata01 += w * ax * ay;
        ata11 += w * ay * ay;
        atb0 += w * ax * b;
        atb1 += w * ay * b;
    }

    let det = ata00 * ata11 - ata01 * ata01;
    if !det.is_finite() || det.abs() < 1.0e-6 {
        return None;
    }
    let x = (atb0 * ata11 - atb1 * ata01) / det;
    let y = (ata00 * atb1 - ata01 * atb0) / det;
    (x.is_finite() && y.is_finite()).then_some((x, y))
}

fn clamp_to_anchor_bounds(point: (f32, f32), nodes: &[RangedNode]) -> (f32, f32) {
    let mut min_x = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    for node in nodes {
        min_x = min_x.min(node.position.x);
        max_x = max_x.max(node.position.x);
        min_y = min_y.min(node.position.y);
        max_y = max_y.max(node.position.y);
    }
    let margin = layout_span(nodes) * 0.05;
    (
        point.0.clamp(min_x - margin, max_x + margin),
        point.1.clamp(min_y - margin, max_y + margin),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn room_config(nodes: Vec<NodeConfig>) -> RoomConfig {
        RoomConfig {
            version: 2,
            room: RoomShape {
                shape: "polygon".to_string(),
                boundary: vec![
                    Point2D { x: 0.0, y: 0.0 },
                    Point2D { x: 5.0, y: 0.0 },
                    Point2D { x: 5.0, y: 4.0 },
                    Point2D { x: 0.0, y: 4.0 },
                ],
            },
            nodes,
        }
    }

    fn node(id: u32, x: f32, y: f32) -> NodeConfig {
        NodeConfig {
            id,
            x,
            y,
            active: true,
        }
    }

    #[test]
    fn locate_person_uses_shared_room_config_state() {
        let state = room_config_state();
        update_room_config_state(
            &state,
            room_config(vec![node(1, 0.0, 0.0), node(2, 4.0, 0.0)]),
        )
        .expect("initial room config");

        let first = locate_person(&[(1, -42.0), (2, -42.0)]).expect("initial location");
        assert!((first.0 - 2.0).abs() < 0.25);

        update_room_config_state(&state, room_config(vec![node(1, 10.0, 0.0), node(2, 14.0, 0.0)]))
            .expect("room config update");
        let second = locate_person(&[(1, -42.0), (2, -42.0)]).expect("updated location");
        assert!((second.0 - 12.0).abs() < 0.25);

        update_room_config_state(&state, RoomConfig::default()).expect("reset room config");
    }

    #[test]
    fn room_config_positions_ignore_inactive_nodes() {
        let mut inactive = node(3, 2.0, 2.0);
        inactive.active = false;
        let positions = node_positions_from_room_config(&room_config(vec![
            node(2, 4.0, 0.0),
            inactive,
            node(1, 0.0, 0.0),
        ]));

        assert_eq!(
            positions,
            vec![
                NodePosition {
                    node_id: 1,
                    x: 0.0,
                    y: 0.0,
                },
                NodePosition {
                    node_id: 2,
                    x: 4.0,
                    y: 0.0,
                },
            ]
        );
    }
}

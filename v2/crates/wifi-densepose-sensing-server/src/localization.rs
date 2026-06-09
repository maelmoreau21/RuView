//! RSSI-based room localization for ESP32 CSI nodes.

use serde::{Deserialize, Serialize};

pub const ROOM_NODE_POSITIONS_ENV: &str = "ROOM_NODE_POSITIONS";

const TX_POWER_DBM: f32 = -30.0;
const PATH_LOSS_EXPONENT: f32 = 2.5;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct NodePosition {
    pub node_id: u8,
    pub x: f32,
    pub y: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PersonLocation {
    pub x: f32,
    pub y: f32,
    pub confidence: f32,
    pub timestamp_ms: u64,
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

pub fn node_positions_from_env() -> Vec<NodePosition> {
    std::env::var(ROOM_NODE_POSITIONS_ENV)
        .ok()
        .and_then(|value| parse_node_positions(&value).ok())
        .unwrap_or_default()
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
    let positions = node_positions_from_env();
    locate_person_with_positions(node_rssi, &positions)
}

pub fn locate_person_with_positions(
    node_rssi: &[(u8, f32)],
    positions: &[NodePosition],
) -> Option<(f32, f32)> {
    if locatable_node_count(node_rssi, positions) < 2 {
        return None;
    }

    let mut weighted_x = 0.0_f32;
    let mut weighted_y = 0.0_f32;
    let mut total_weight = 0.0_f32;

    for (node_id, rssi) in node_rssi {
        if !rssi.is_finite() {
            continue;
        }
        let Some(position) = find_position(positions, *node_id) else {
            continue;
        };
        let distance = rssi_to_distance_m(*rssi).max(0.05);
        let weight = 1.0 / (distance * distance);
        if !weight.is_finite() || weight <= 0.0 {
            continue;
        }

        weighted_x += position.x * weight;
        weighted_y += position.y * weight;
        total_weight += weight;
    }

    (total_weight > 0.0).then_some((weighted_x / total_weight, weighted_y / total_weight))
}

pub fn estimate_person_location(
    node_rssi: &[(u8, f32)],
    timestamp_ms: u64,
) -> Option<PersonLocation> {
    let positions = node_positions_from_env();
    estimate_person_location_with_positions(node_rssi, &positions, timestamp_ms)
}

pub fn estimate_person_location_with_positions(
    node_rssi: &[(u8, f32)],
    positions: &[NodePosition],
    timestamp_ms: u64,
) -> Option<PersonLocation> {
    let node_count = locatable_node_count(node_rssi, positions);
    if node_count == 0 {
        return None;
    }

    let (x, y) = if node_count == 1 {
        let position = node_rssi.iter().find_map(|(node_id, rssi)| {
            rssi.is_finite()
                .then(|| find_position(positions, *node_id))
                .flatten()
        })?;
        (position.x, position.y)
    } else {
        locate_person_with_positions(node_rssi, positions)?
    };

    Some(PersonLocation {
        x,
        y,
        confidence: confidence_for_node_count(node_count),
        timestamp_ms,
    })
}

fn find_position(positions: &[NodePosition], node_id: u8) -> Option<NodePosition> {
    positions
        .iter()
        .copied()
        .find(|position| position.node_id == node_id)
}

#[cfg(test)]
mod tests {
    use super::{
        estimate_person_location_with_positions, locate_person_with_positions, NodePosition,
    };

    fn triangle_positions() -> [NodePosition; 3] {
        [
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
            NodePosition {
                node_id: 3,
                x: 2.0,
                y: 3.5,
            },
        ]
    }

    fn approx_eq(left: f32, right: f32) {
        assert!(
            (left - right).abs() < 1.0e-4,
            "expected {left} to be close to {right}"
        );
    }

    fn inside_triangle(point: (f32, f32), triangle: &[NodePosition; 3]) -> bool {
        let sign = |a: (f32, f32), b: (f32, f32), c: (f32, f32)| {
            (a.0 - c.0) * (b.1 - c.1) - (b.0 - c.0) * (a.1 - c.1)
        };
        let p = point;
        let a = (triangle[0].x, triangle[0].y);
        let b = (triangle[1].x, triangle[1].y);
        let c = (triangle[2].x, triangle[2].y);
        let d1 = sign(p, a, b);
        let d2 = sign(p, b, c);
        let d3 = sign(p, c, a);
        let has_neg = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
        let has_pos = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
        !(has_neg && has_pos)
    }

    #[test]
    fn one_node_location_has_zero_confidence() {
        let positions = triangle_positions();
        let location = estimate_person_location_with_positions(&[(1, -48.0)], &positions, 123)
            .expect("one positioned node still yields a zero-confidence location");

        approx_eq(location.x, 0.0);
        approx_eq(location.y, 0.0);
        approx_eq(location.confidence, 0.0);
    }

    #[test]
    fn three_triangle_nodes_place_location_inside_triangle() {
        let positions = triangle_positions();
        let location = estimate_person_location_with_positions(
            &[(1, -49.0), (2, -54.0), (3, -58.0)],
            &positions,
            456,
        )
        .expect("three positioned nodes should locate a person");

        approx_eq(location.confidence, 1.0);
        assert!(
            inside_triangle((location.x, location.y), &positions),
            "location ({}, {}) should stay inside the node triangle",
            location.x,
            location.y
        );
    }

    #[test]
    fn equal_rssi_returns_geometric_centroid() {
        let positions = triangle_positions();
        let (x, y) = locate_person_with_positions(
            &[(1, -55.0), (2, -55.0), (3, -55.0)],
            &positions,
        )
        .expect("equal RSSI from three positioned nodes should locate a person");

        approx_eq(x, 2.0);
        approx_eq(y, 3.5 / 3.0);
    }
}

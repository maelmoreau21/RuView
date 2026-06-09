use wifi_densepose_sensing_server::localization::{
    estimate_person_location_with_positions, locate_person_with_positions, LocationSmoother,
    NodePosition, PersonLocation,
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
    let a = (triangle[0].x, triangle[0].y);
    let b = (triangle[1].x, triangle[1].y);
    let c = (triangle[2].x, triangle[2].y);
    let d1 = sign(point, a, b);
    let d2 = sign(point, b, c);
    let d3 = sign(point, c, a);
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
fn equal_rssi_returns_near_geometric_center() {
    let positions = triangle_positions();
    let (x, y) =
        locate_person_with_positions(&[(1, -55.0), (2, -55.0), (3, -55.0)], &positions)
            .expect("equal RSSI from three positioned nodes should locate a person");

    approx_eq(x, 2.0);
    assert!(
        (y - 3.5 / 3.0).abs() < 0.03,
        "equal RSSI should stay near the room center, got y={y}"
    );
}

#[test]
fn stronger_rssi_moves_location_toward_that_node() {
    let positions = triangle_positions();
    let (x, y) =
        locate_person_with_positions(&[(1, -42.0), (2, -62.0), (3, -64.0)], &positions)
            .expect("three positioned nodes should locate a person");

    assert!(x < 1.4, "strong node 1 should pull x left, got {x}");
    assert!(y < 1.0, "strong node 1 should pull y down, got {y}");
}

#[test]
fn smoother_limits_jitter_without_freezing_motion() {
    let mut smoother = LocationSmoother::default();
    let first = smoother.update(PersonLocation {
        x: 1.0,
        y: 1.0,
        confidence: 1.0,
        timestamp_ms: 1_000,
    });
    let second = smoother.update(PersonLocation {
        x: 2.0,
        y: 1.0,
        confidence: 1.0,
        timestamp_ms: 1_100,
    });

    approx_eq(first.x, 1.0);
    assert!(second.x > 1.0, "smoother must follow motion");
    assert!(second.x < 2.0, "smoother must damp sudden jumps");
}

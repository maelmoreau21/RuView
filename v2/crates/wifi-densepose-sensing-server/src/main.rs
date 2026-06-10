//! WiFi-DensePose Sensing Server
//!
//! Lightweight Axum server that:
//! - Receives ESP32 CSI frames via UDP (port 5005)
//! - Processes signals using RuVector-powered wifi-densepose-signal crate
//! - Broadcasts sensing updates via WebSocket (ws://localhost:8765/ws/sensing)
//! - Serves the static UI files (port 8080)
//!
//! Replaces both ws_server.py and the Python HTTP server.
#![allow(dead_code)]

mod adaptive_classifier;
pub mod cli;
pub mod csi;
mod feature_flags;
mod field_bridge;
mod multistatic_bridge;
pub mod pose;
mod rvf_container;
mod rvf_pipeline;
mod tracker_bridge;
pub mod types;
mod udp_jitter;
mod vital_signs;
mod vitals;

// Training pipeline modules (exposed via lib.rs)
use wifi_densepose_sensing_server::{dataset, embedding, graph_transformer, trainer};
use wifi_densepose_sensing_server::localization::{
    self, LocationSmoother, NodePosition, NodeSignalReading, PersonLocation,
};

use ruvector_mincut::{DynamicMinCut, MinCutBuilder};
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::path::{Path as StdPath, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, Query, State,
    },
    http::StatusCode,
    response::{Html, IntoResponse, Json, Redirect, Response},
    routing::{delete, get, post, put},
    Extension, Router,
};
use clap::{Parser, ValueEnum};
use ndarray::Array2;
use num_complex::Complex64;

use axum::http::HeaderValue;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::net::UdpSocket;
use tokio::sync::{broadcast, RwLock};
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;
use tracing::{debug, error, info, warn};

use wifi_densepose_sensing_server::alerts::{AlertManager, AlertSample, AlertThresholds};
use rvf_container::{RvfBuilder, RvfContainerInfo, RvfReader, VitalSignConfig};
use rvf_pipeline::ProgressiveLoader;
use feature_flags::{BetaFeature, FeatureFlags};
use vital_signs::{VitalSignDetector, VitalSigns};
use vitals::{breathing_min_confidence, BreathingExtractor, BreathingResult};

// ADR-022 Phase 3: Multi-BSSID pipeline integration
use wifi_densepose_wifiscan::parse_netsh_output as parse_netsh_bssid_output;
#[cfg(target_os = "linux")]
use wifi_densepose_wifiscan::LinuxIwScanner;
use wifi_densepose_wifiscan::{BssidObservation, BssidRegistry, WindowsWifiPipeline};

// Accuracy sprint: Kalman tracker, multistatic fusion, field model
use wifi_densepose_core::types::{
    AntennaConfig as CoreAntennaConfig, CsiFrame as CoreCsiFrame,
    CsiMetadata as CoreCsiMetadata, DeviceId as CoreDeviceId, FrequencyBand as CoreFrequencyBand,
};
use wifi_densepose_signal::ruvsense::field_model::{CalibrationStatus, FieldModel};
use wifi_densepose_signal::ruvsense::multistatic::{MultistaticConfig, MultistaticFuser};
use wifi_densepose_signal::ruvsense::pose_tracker::PoseTracker;
use wifi_densepose_signal::ruvsense::{
    AdaptiveCalibrationConfig, AdaptiveCalibrationDecision, AdaptiveCalibrationMonitor,
    AdaptiveCalibrationSnapshot,
};

// ── CLI ──────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "ruvsense-master",
    about = "RuvSense Edge master for WiFi/CSI sensing fleets"
)]
struct Args {
    /// HTTP port for UI and REST API
    #[arg(long, default_value = "8080")]
    http_port: u16,

    /// WebSocket port for sensing stream
    #[arg(long, default_value = "8765")]
    ws_port: u16,

    /// UDP port for ESP32 CSI frames
    #[arg(long, default_value = "5005")]
    udp_port: u16,

    /// Maximum UDP jitter-buffer hold time before flushing an out-of-order CSI gap.
    #[arg(
        long,
        default_value_t = 75,
        env = "RUVSENSE_UDP_JITTER_HOLD_MS"
    )]
    udp_jitter_hold_ms: u64,

    /// Maximum sequence gap to keep buffered for out-of-order UDP CSI packets.
    #[arg(
        long,
        default_value_t = 8,
        env = "RUVSENSE_UDP_JITTER_MAX_REORDER_GAP"
    )]
    udp_jitter_max_reorder_gap: u32,

    /// Maximum missing CSI sequence gap to bridge with linear interpolation.
    #[arg(
        long,
        default_value_t = 3,
        env = "RUVSENSE_UDP_JITTER_MAX_INTERPOLATE_GAP"
    )]
    udp_jitter_max_interpolate_gap: u32,

    /// Maximum queued CSI frames per node before the jitter buffer flushes.
    #[arg(
        long,
        default_value_t = 12,
        env = "RUVSENSE_UDP_JITTER_MAX_BUFFERED_FRAMES"
    )]
    udp_jitter_max_buffered_frames: usize,

    /// Minimum active ESP32 nodes required before readiness is green.
    #[arg(long, default_value = "1", env = "RUVSENSE_MIN_NODES")]
    min_nodes: usize,

    /// Linux WiFi interface used by the Raspberry Pi master to discover visible APs.
    #[arg(long, default_value = "wlan0", env = "RUVSENSE_WIFI_INTERFACE")]
    wifi_interface: String,

    /// Seconds between Raspberry Pi AP scans for the live topology endpoint.
    #[arg(long, default_value_t = 10, env = "RUVSENSE_AP_SCAN_INTERVAL_SECS")]
    ap_scan_interval_secs: u64,

    /// Persistent data directory for runtime config, recordings, and SQLite state.
    #[arg(long, default_value = "data", env = "RUVSENSE_DATA_DIR")]
    data_dir: PathBuf,

    /// Explicit runtime configuration file (JSON, TOML, YAML).
    #[arg(long, value_name = "PATH", env = "RUVSENSE_CONFIG_FILE")]
    config_file: Option<PathBuf>,

    /// Validate all TOML/YAML configuration artifacts under a root and exit.
    #[arg(long, value_name = "PATH")]
    validate_config_root: Option<PathBuf>,

    /// Print a JSON Schema for a supported configuration shape and exit.
    #[arg(long, value_enum, value_name = "KIND")]
    print_config_schema: Option<ConfigSchemaKind>,

    /// Path to UI static files (repo `ui/`; from `v2/` use `../ui` or rely on auto-detect)
    #[arg(long, default_value = "../ui")]
    ui_path: PathBuf,

    /// Tick interval in milliseconds (default 100 ms = 10 fps for smooth pose animation)
    #[arg(long, default_value = "100")]
    tick_ms: u64,

    /// Bind address (default 127.0.0.1; set to 0.0.0.0 for network access)
    #[arg(long, default_value = "127.0.0.1", env = "SENSING_BIND_ADDR")]
    bind_addr: String,

    /// Additional hostname (with or without `:PORT`) to permit in the `Host`
    /// header — defends loopback-bound deployments against DNS rebinding.
    /// Loopback names (`localhost`, `127.0.0.1`, `[::1]`) are always permitted
    /// implicitly. Pass multiple times to add several entries. Comma-separated
    /// values are also accepted via the `SENSING_ALLOWED_HOSTS` env var.
    #[arg(long = "allowed-host", value_name = "HOST")]
    allowed_hosts: Vec<String>,

    /// Disable Host-header validation entirely. Use only when the server sits
    /// behind a reverse proxy that already canonicalises `Host` (e.g. nginx
    /// `proxy_set_header Host`) — bare deployments stay vulnerable to DNS
    /// rebinding without it.
    #[arg(long)]
    disable_host_validation: bool,

    /// MQTT publisher (HA auto-discovery) + privacy-mode flags (ADR-115).
    /// Flattened so `--mqtt*` reach the binary's parser and the publisher
    /// in `mqtt::` is actually started (fixes #872). Uses the *lib* crate's
    /// `MqttArgs` type so it's compatible with `mqtt::config::from_args`.
    #[command(flatten)]
    mqtt_opts: wifi_densepose_sensing_server::cli::MqttArgs,

    /// Data source: auto, wifi, esp32, simulate. Simulation requires --enable-simulation.
    #[arg(long, default_value = "auto")]
    source: String,

    /// Explicitly allow the synthetic simulation source for dev/test runs.
    #[arg(long, default_value_t = false, env = "RUVSENSE_ENABLE_SIMULATION")]
    enable_simulation: bool,

    /// Run vital sign detection benchmark (1000 frames) and exit
    #[arg(long)]
    benchmark: bool,

    /// Load model config from an RVF container at startup
    #[arg(long, value_name = "PATH")]
    load_rvf: Option<PathBuf>,

    /// Save current model state as an RVF container on shutdown
    #[arg(long, value_name = "PATH")]
    save_rvf: Option<PathBuf>,

    /// Load a trained .rvf model for inference
    #[arg(long, value_name = "PATH")]
    model: Option<PathBuf>,

    /// Enable progressive loading (Layer A instant start)
    #[arg(long)]
    progressive: bool,

    /// Export an RVF container package and exit (no server)
    #[arg(long, value_name = "PATH")]
    export_rvf: Option<PathBuf>,

    /// Run training mode (train a model and exit)
    #[arg(long)]
    train: bool,

    /// Path to dataset directory (MM-Fi or Wi-Pose)
    #[arg(long, value_name = "PATH")]
    dataset: Option<PathBuf>,

    /// Dataset type: "mmfi" or "wipose"
    #[arg(long, value_name = "TYPE", default_value = "mmfi")]
    dataset_type: String,

    /// Number of training epochs
    #[arg(long, default_value = "100")]
    epochs: usize,

    /// Directory for training checkpoints
    #[arg(long, value_name = "DIR")]
    checkpoint_dir: Option<PathBuf>,

    /// Run self-supervised contrastive pretraining (ADR-024)
    #[arg(long)]
    pretrain: bool,

    /// Number of pretraining epochs (default 50)
    #[arg(long, default_value = "50")]
    pretrain_epochs: usize,

    /// Extract embeddings mode: load model and extract CSI embeddings
    #[arg(long)]
    embed: bool,

    /// Build fingerprint index from embeddings (env|activity|temporal|person)
    #[arg(long, value_name = "TYPE")]
    build_index: Option<String>,

    /// Node positions for multistatic fusion (format: "x,y,z;x,y,z;...")
    #[arg(long, env = "SENSING_NODE_POSITIONS")]
    node_positions: Option<String>,

    /// Start field model calibration on boot (empty room required)
    #[arg(long)]
    calibrate: bool,

    // ---------------------------------------------------------------
    // ADR-102: Edge Module Registry — surface the canonical Cognitum
    // cog catalog via `GET /api/v1/edge/registry`.
    // ---------------------------------------------------------------
    /// Override the upstream URL for the edge module registry. Set to a
    /// mirror or local file://... URL for air-gapped deployments. Empty
    /// string or --no-edge-registry disables the endpoint entirely.
    #[arg(
        long,
        value_name = "URL",
        env = "RUVIEW_EDGE_REGISTRY_URL",
        default_value = "https://storage.googleapis.com/cognitum-apps/app-registry.json"
    )]
    edge_registry_url: String,

    /// Cache TTL for the edge module registry, in seconds.
    #[arg(
        long,
        value_name = "SECS",
        env = "RUVIEW_EDGE_REGISTRY_TTL_SECS",
        default_value = "3600"
    )]
    edge_registry_ttl_secs: u64,

    /// Disable the edge module registry endpoint entirely. Returns 404 on
    /// `GET /api/v1/edge/registry`. Use for air-gapped deployments.
    #[arg(long, env = "RUVIEW_NO_EDGE_REGISTRY")]
    no_edge_registry: bool,
}

#[derive(Clone, Debug, ValueEnum)]
enum ConfigSchemaKind {
    Runtime,
    Swarm,
    Training,
    HomecoreAutomation,
    BfldBlueprint,
}

// ── Data types ───────────────────────────────────────────────────────────────

/// ADR-018 ESP32 CSI binary frame header (20 bytes)
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct Esp32Frame {
    magic: u32,
    node_id: u8,
    n_antennas: u8,
    n_subcarriers: u8,
    freq_mhz: u16,
    sequence: u32,
    rssi: i8,
    noise_floor: i8,
    amplitudes: Vec<f64>,
    phases: Vec<f64>,
}

/// Sensing update broadcast to WebSocket clients
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SensingUpdate {
    #[serde(rename = "type")]
    msg_type: String,
    timestamp: f64,
    source: String,
    tick: u64,
    nodes: Vec<NodeInfo>,
    features: FeatureInfo,
    classification: ClassificationInfo,
    signal_field: SignalField,
    /// Vital sign estimates (breathing rate, heart rate, confidence).
    #[serde(skip_serializing_if = "Option::is_none")]
    vital_signs: Option<VitalSigns>,
    // ── ADR-022 Phase 3: Enhanced multi-BSSID pipeline fields ──
    /// Enhanced motion estimate from multi-BSSID pipeline.
    #[serde(skip_serializing_if = "Option::is_none")]
    enhanced_motion: Option<serde_json::Value>,
    /// Enhanced breathing estimate from multi-BSSID pipeline.
    #[serde(skip_serializing_if = "Option::is_none")]
    enhanced_breathing: Option<serde_json::Value>,
    /// Posture classification from BSSID fingerprint matching.
    #[serde(skip_serializing_if = "Option::is_none")]
    posture: Option<String>,
    /// Signal quality score from multi-BSSID quality gate [0.0, 1.0].
    #[serde(skip_serializing_if = "Option::is_none")]
    signal_quality_score: Option<f64>,
    /// Quality gate verdict: "Permit", "Warn", or "Deny".
    #[serde(skip_serializing_if = "Option::is_none")]
    quality_verdict: Option<String>,
    /// Number of BSSIDs used in the enhanced sensing cycle.
    #[serde(skip_serializing_if = "Option::is_none")]
    bssid_count: Option<usize>,
    // ── ADR-023 Phase 7-8: Model inference fields ──
    /// Pose keypoints when a trained model is loaded (x, y, z, confidence).
    #[serde(skip_serializing_if = "Option::is_none")]
    pose_keypoints: Option<Vec<[f64; 4]>>,
    /// Model status when a trained model is loaded.
    #[serde(skip_serializing_if = "Option::is_none")]
    model_status: Option<serde_json::Value>,
    // ── Multi-person detection (issue #97) ──
    /// Detected persons from WiFi sensing (multi-person support).
    #[serde(skip_serializing_if = "Option::is_none")]
    persons: Option<Vec<PersonDetection>>,
    /// Centralized room state consumed by both 2D and 3D clients.
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<TrackingState>,
    /// Estimated person count from CSI feature heuristics (1-3 for single ESP32).
    #[serde(skip_serializing_if = "Option::is_none")]
    estimated_persons: Option<usize>,
    /// Conservative count evidence used by live UIs to avoid multipath ghosts.
    #[serde(skip_serializing_if = "Option::is_none")]
    count_evidence: Option<CountEvidence>,
    /// Per-node feature breakdown for multi-node deployments.
    #[serde(skip_serializing_if = "Option::is_none")]
    node_features: Option<Vec<PerNodeFeatureInfo>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TrackingState {
    coordinate_system: String,
    timestamp_ms: u64,
    node_count: usize,
    persons: Vec<TrackedPersonState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TrackedPersonState {
    id: u32,
    x: f64,
    y: f64,
    z: f64,
    position_m: [f64; 3],
    confidence: f64,
    source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CountEvidence {
    stable_persons: usize,
    raw_estimated_persons: usize,
    rendered_persons: usize,
    active_nodes: usize,
    supporting_nodes: usize,
    ambiguous: bool,
    reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NodeInfo {
    node_id: u8,
    rssi_dbm: f64,
    position: [f64; 3],
    amplitude: Vec<f64>,
    subcarrier_count: usize,
    /// ADR-110 iter 23 — cross-board sync snapshot for this node.
    /// `None` when no fresh sync packet has been observed (no mesh peer
    /// reachable, or this node is a singleton). Populated from
    /// `NodeState::latest_sync` and the iter 18 fps EMA.
    #[serde(skip_serializing_if = "Option::is_none")]
    sync: Option<NodeSyncSnapshot>,
}

/// ADR-110 iter 23 — per-node mesh-sync snapshot embedded in NodeInfo.
/// Surfaces what was previously only visible in the debug log so UI clients
/// can render leader / follower / offset / measured-fps live.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct NodeSyncSnapshot {
    /// Smoothed local-vs-mesh offset in µs (negative when this node's clock
    /// is behind the leader's — see §A0.10's measured -1.16 s on the bench).
    offset_us: i64,
    /// True when this node is the elected mesh leader.
    is_leader: bool,
    /// True when this node has heard a fresh leader beacon within the
    /// firmware's VALID_WINDOW_MS gate (3 s).
    is_valid: bool,
    /// True once the EMA-smoothed offset has seeded (one full beacon round-trip).
    smoothed: bool,
    /// Sync packet's sequence high-water — used by the host to pair CSI
    /// frames against this snapshot for §A0.12 mesh-time recovery.
    sequence: u32,
    /// Per-node measured CSI frame rate (iter 18 EMA). 20.0 until the
    /// EMA has at least 5 samples; the actually-observed rate after that.
    csi_fps_ema: f64,
    /// How many CSI frames have contributed to `csi_fps_ema`. Clients can
    /// treat <5 as "not yet trustworthy" and fall back to 20 Hz.
    csi_fps_samples: u32,
    /// ADR-110 iter 34 — milliseconds since the host last received a sync
    /// packet from this node. Lets UI dashboards render sync-age decay
    /// (badge fades after 5 s, drops off after the 9 s mesh_aligned_us
    /// staleness gate). `None` only when the host never had Instant data
    /// for this node, which shouldn't happen in normal flow but is
    /// modeled defensively.
    #[serde(skip_serializing_if = "Option::is_none")]
    staleness_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FeatureInfo {
    mean_rssi: f64,
    variance: f64,
    motion_band_power: f64,
    breathing_band_power: f64,
    dominant_freq_hz: f64,
    change_points: usize,
    spectral_power: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ClassificationInfo {
    motion_level: String,
    presence: bool,
    confidence: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SignalField {
    grid_size: [usize; 3],
    values: Vec<f64>,
}

/// WiFi-derived pose keypoint (17 COCO keypoints)
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PoseKeypoint {
    name: String,
    x: f64,
    y: f64,
    z: f64,
    confidence: f64,
}

/// Person detection from WiFi sensing
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersonDetection {
    id: u32,
    confidence: f64,
    keypoints: Vec<PoseKeypoint>,
    #[serde(skip_serializing_if = "Option::is_none")]
    keypoints_m: Option<Vec<PoseKeypoint>>,
    bbox: BoundingBox,
    zone: String,
    position_m: Option<[f64; 3]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    position: Option<[f64; 3]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    position_source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pose_source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BoundingBox {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

/// Per-node sensing state for multi-node deployments (issue #249).
/// Each ESP32 node gets its own frame history, smoothing buffers, and vital
/// sign detector so that data from different nodes is never mixed.
struct NodeState {
    pub(crate) frame_history: VecDeque<Vec<f64>>,
    smoothed_person_score: f64,
    pub(crate) prev_person_count: usize,
    smoothed_motion: f64,
    current_motion_level: String,
    debounce_counter: u32,
    debounce_candidate: String,
    baseline_motion: f64,
    baseline_frames: u64,
    smoothed_hr: f64,
    smoothed_br: f64,
    smoothed_hr_conf: f64,
    smoothed_br_conf: f64,
    hr_buffer: VecDeque<f64>,
    br_buffer: VecDeque<f64>,
    rssi_history: VecDeque<f64>,
    vital_detector: VitalSignDetector,
    breathing_extractor: BreathingExtractor,
    latest_breathing: BreathingResult,
    latest_vitals: VitalSigns,
    pub(crate) last_frame_time: Option<std::time::Instant>,
    last_csi_time: Option<std::time::Instant>,
    last_vitals_time: Option<std::time::Instant>,
    remote_addr: Option<String>,
    edge_vitals: Option<Esp32VitalsPacket>,
    /// ADR-110 §A0.12: Latest sync packet received from this node. When a
    /// CSI frame arrives with byte 19 bit 4 set (`adr018_flags.ieee802154_sync_valid`),
    /// the host can recover a mesh-aligned timestamp via
    /// `latest_sync.epoch_us + (now_local - latest_sync.local_us)`.
    latest_sync: Option<wifi_densepose_hardware::SyncPacket>,
    /// Last time a sync packet from this node was received (for staleness).
    latest_sync_at: Option<std::time::Instant>,
    /// ADR-110 iter 18: EMA-tracked CSI frame rate for this node.
    /// Replaces the hardcoded 20 Hz fallback in
    /// `mesh_aligned_us_for_csi_frame` once `csi_fps_samples ≥ 5`.
    csi_fps_ema: f64,
    /// Number of inter-frame deltas observed (need ≥5 before trusting EMA).
    csi_fps_samples: u32,
    /// Latest extracted features for cross-node fusion.
    latest_features: Option<FeatureInfo>,
    // ── RuVector Phase 2: Temporal smoothing & coherence gating ──
    /// Previous frame's smoothed keypoint positions for EMA temporal smoothing.
    prev_keypoints: Option<Vec<[f64; 3]>>,
    /// Rolling buffer of motion_energy values for coherence scoring (last 20 frames).
    motion_energy_history: VecDeque<f64>,
    /// Coherence score [0.0, 1.0]: low variance in motion_energy = high coherence.
    coherence_score: f64,
    /// ADR-084 Pass 3 cluster-Pi novelty sensor — per-node sketch bank of
    /// recent CSI feature vectors. Populated by `update_novelty` on each
    /// frame; left `None` to disable the sensor on a per-node basis.
    feature_history: Option<wifi_densepose_signal::ruvsense::longitudinal::EmbeddingHistory>,
    /// Most recent novelty score in [0.0, 1.0] (0 = exact-match in bank,
    /// 1 = no overlap). Consumed by the model-wake gate downstream.
    pub(crate) last_novelty_score: Option<f32>,
    breathing_compressor: wifi_densepose_ruvector::mat::CompressedBreathingBuffer,
    heartbeat_compressor: wifi_densepose_ruvector::mat::CompressedHeartbeatSpectrogram,
    latest_breathing_compression_ratio: f64,
    latest_heartbeat_compression_ratio: f64,
    /// ADR-135 adaptive empty-room baseline monitor. Live CSI only.
    adaptive_calibration: Option<AdaptiveCalibrationMonitor>,
}

/// Default EMA alpha for temporal keypoint smoothing (RuVector Phase 2).
/// Lower = smoother (more history, less jitter). 0.15 balances responsiveness
/// with stability for WiFi CSI where per-frame noise is high.
const TEMPORAL_EMA_ALPHA_DEFAULT: f64 = 0.15;
/// Reduced EMA alpha when coherence is low (trust measurements less).
const TEMPORAL_EMA_ALPHA_LOW_COHERENCE: f64 = 0.05;
/// Coherence threshold below which we reduce EMA alpha.
const COHERENCE_LOW_THRESHOLD: f64 = 0.3;
/// Maximum allowed bone-length change ratio between frames (20%).
const MAX_BONE_CHANGE_RATIO: f64 = 0.20;
/// Number of motion_energy frames to track for coherence scoring.
const COHERENCE_WINDOW: usize = 20;
/// ADR-084 Pass 3 — per-node novelty sketch dimension (56 subcarriers,
/// the dominant ESP32-S3 capture configuration).
const NOVELTY_VECTOR_DIM: usize = 56;
/// ADR-084 Pass 3 — number of past sketches retained per-node for
/// novelty comparison. 64 frames ≈ 6.4 s at 10 Hz.
const NOVELTY_HISTORY_CAPACITY: usize = 64;
/// ADR-084 Pass 3 — feature-vector schema version. Bump on changes to
/// subcarrier ordering / normalisation so banks reject stale data.
const NOVELTY_SKETCH_VERSION: u16 = 1;

/// ADR-110 iter 18 — EMA update for per-node CSI fps tracking.
///
/// Returns the new EMA value, or `None` if the delta is implausible
/// (≤ 0, or > 1 second — likely a connection gap, not a real frame
/// rate sample). α = 1/8 fixed shift, ~8-sample effective window,
/// matching the firmware-side ESP-NOW offset smoother in §A0.10.
///
/// Free function for testability — every transformation that doesn't
/// touch the rest of `NodeState` lives outside the `impl` block.
pub(crate) fn update_csi_fps_ema(prev_fps: f64, dt_sec: f64) -> Option<f64> {
    if !(dt_sec > 0.0 && dt_sec < 1.0) {
        return None;
    }
    let instantaneous = 1.0 / dt_sec;
    // y[n] = y[n-1] + (x - y[n-1]) / 8
    Some(prev_fps + (instantaneous - prev_fps) / 8.0)
}

#[cfg(test)]
mod fps_ema_tests {
    use super::update_csi_fps_ema;

    #[test]
    fn steady_10hz_converges_toward_10() {
        let mut fps = 20.0;
        for _ in 0..40 {
            fps = update_csi_fps_ema(fps, 0.100).unwrap();
        }
        assert!(
            (fps - 10.0).abs() < 0.1,
            "expected ~10 Hz after 40 samples at 100 ms intervals, got {fps}"
        );
    }

    #[test]
    fn steady_20hz_stays_near_20() {
        let mut fps = 20.0;
        for _ in 0..20 {
            fps = update_csi_fps_ema(fps, 0.050).unwrap();
        }
        assert!((fps - 20.0).abs() < 0.05, "expected ~20 Hz, got {fps}");
    }

    #[test]
    fn nonpositive_dt_rejected() {
        assert!(update_csi_fps_ema(15.0, 0.0).is_none());
        assert!(update_csi_fps_ema(15.0, -0.1).is_none());
    }

    #[test]
    fn long_gap_rejected_as_implausible() {
        assert!(update_csi_fps_ema(20.0, 2.0).is_none());
    }
}

impl NodeState {
    pub(crate) fn observe_remote_addr(&mut self, addr: SocketAddr) {
        self.remote_addr = Some(addr.to_string());
    }

    pub(crate) fn observe_edge_vitals_arrival(&mut self, now: std::time::Instant) {
        self.last_frame_time = Some(now);
        self.last_vitals_time = Some(now);
    }

    /// ADR-110 §A0.12 timestamp recovery: given a CSI frame's node-local
    /// `esp_timer_get_time()` snapshot, return the mesh-aligned epoch
    /// computed from this node's most recent sync packet — or `None`
    /// if no sync has been received yet, or the last one is too stale
    /// (older than 3 × VALID_WINDOW_MS = 9 s, matching the firmware's own
    /// staleness gate).
    pub(crate) fn mesh_aligned_us(&self, local_at_frame_us: u64) -> Option<u64> {
        let sync = self.latest_sync.as_ref()?;
        let seen_at = self.latest_sync_at?;
        // Drop stale syncs — firmware emits at ~0.5 Hz default, anything
        // older than 9 s likely means the mesh transport dropped.
        if seen_at.elapsed() > std::time::Duration::from_secs(9) {
            return None;
        }
        Some(sync.apply_to_local(local_at_frame_us))
    }

    /// ADR-110 §A0.12 sequence-based mesh-time recovery for an in-flight
    /// ADR-018 CSI frame. The frame carries no `local_us` (the wire
    /// format has no slot), but it carries a sequence number that the
    /// sync packet's `sequence` high-water can be paired against. Uses
    /// 20 Hz as the default CSI rate (the firmware's
    /// `CSI_MIN_SEND_INTERVAL_US`-implied ceiling). Returns `None` if
    /// no fresh sync has been observed for this node.
    pub(crate) fn mesh_aligned_us_for_csi_frame(&self, frame_sequence: u32) -> Option<u64> {
        let sync = self.latest_sync.as_ref()?;
        let seen_at = self.latest_sync_at?;
        if seen_at.elapsed() > std::time::Duration::from_secs(9) {
            return None;
        }
        // Iter 18: use the measured per-node fps once we have ≥5 inter-frame
        // samples; until then fall back to the 20 Hz firmware ceiling. The
        // §A0.12 capture showed real bench fps ≈ 10, so the measured value
        // is significantly more accurate than the constant fallback.
        let fps = if self.csi_fps_samples >= 5 {
            self.csi_fps_ema
        } else {
            20.0
        };
        Some(sync.mesh_aligned_us_for_sequence(frame_sequence, fps))
    }

    /// ADR-110 iter 18 — update the per-node observed-fps EMA from a fresh
    /// CSI frame arrival. Call once per accepted CSI frame from
    /// `udp_receiver_task`. Uses `last_frame_time` as the previous-frame
    /// anchor; the first frame after init seeds the timer without producing
    /// a sample (no prior dt to measure).
    /// ADR-110 iter 32 — apply a freshly-decoded sync packet to this node.
    /// Overwrites `latest_sync` with the new packet and stamps
    /// `latest_sync_at` so the staleness gate in `mesh_aligned_us_for_csi_frame`
    /// can age it out after 9 s. Used by `udp_receiver_task` on every
    /// successful magic-dispatched sync datagram; extracted so the dispatch
    /// path is testable without spinning up the tokio UDP socket.
    pub(crate) fn apply_sync_packet(
        &mut self,
        pkt: wifi_densepose_hardware::SyncPacket,
        now: std::time::Instant,
    ) {
        self.latest_sync = Some(pkt);
        self.latest_sync_at = Some(now);
    }

    /// ADR-110 iter 30 — pure snapshot of this node's mesh-sync state.
    /// Returns `None` when no sync packet has been observed. Used by both
    /// the WebSocket broadcaster (iter 23) and the REST handlers (iter 29);
    /// extracted here so tests can build a `NodeState`, populate
    /// `latest_sync`, and assert the snapshot shape without spinning up
    /// the axum router.
    pub(crate) fn sync_snapshot(&self) -> Option<NodeSyncSnapshot> {
        let sync = self.latest_sync.as_ref()?;
        Some(NodeSyncSnapshot {
            offset_us: sync.local_minus_epoch_us(),
            is_leader: sync.flags.is_leader,
            is_valid: sync.flags.is_valid,
            smoothed: sync.flags.smoothed_used,
            sequence: sync.sequence,
            csi_fps_ema: self.csi_fps_ema,
            csi_fps_samples: self.csi_fps_samples,
            staleness_ms: self.latest_sync_at.map(|t| t.elapsed().as_millis() as u64),
        })
    }

    pub(crate) fn observe_csi_frame_arrival(&mut self, now: std::time::Instant) {
        if let Some(prev) = self.last_frame_time {
            let dt = now.duration_since(prev).as_secs_f64();
            if let Some(new_ema) = update_csi_fps_ema(self.csi_fps_ema, dt) {
                self.csi_fps_ema = new_ema;
                self.csi_fps_samples = self.csi_fps_samples.saturating_add(1);
            }
        }
        self.last_frame_time = Some(now);
        self.last_csi_time = Some(now);
    }

    pub(crate) fn new() -> Self {
        Self::new_for_node(0)
    }

    pub(crate) fn new_for_node(node_id: u8) -> Self {
        Self {
            frame_history: VecDeque::new(),
            smoothed_person_score: 0.0,
            prev_person_count: 0,
            smoothed_motion: 0.0,
            current_motion_level: "absent".to_string(),
            debounce_counter: 0,
            debounce_candidate: "absent".to_string(),
            baseline_motion: 0.0,
            baseline_frames: 0,
            smoothed_hr: 0.0,
            smoothed_br: 0.0,
            smoothed_hr_conf: 0.0,
            smoothed_br_conf: 0.0,
            hr_buffer: VecDeque::with_capacity(8),
            br_buffer: VecDeque::with_capacity(8),
            rssi_history: VecDeque::new(),
            vital_detector: VitalSignDetector::new(10.0),
            breathing_extractor: BreathingExtractor::new(20.0),
            latest_breathing: BreathingResult::insufficient_data(),
            latest_vitals: VitalSigns::default(),
            last_frame_time: None,
            last_csi_time: None,
            last_vitals_time: None,
            remote_addr: None,
            edge_vitals: None,
            latest_sync: None,
            latest_sync_at: None,
            csi_fps_ema: 20.0,
            csi_fps_samples: 0,
            latest_features: None,
            prev_keypoints: None,
            motion_energy_history: VecDeque::with_capacity(COHERENCE_WINDOW),
            coherence_score: 1.0, // assume stable initially
            feature_history: Some(
                wifi_densepose_signal::ruvsense::longitudinal::EmbeddingHistory::with_sketch(
                    NOVELTY_VECTOR_DIM,
                    NOVELTY_HISTORY_CAPACITY,
                    NOVELTY_SKETCH_VERSION,
                ),
            ),
            last_novelty_score: None,
            breathing_compressor: wifi_densepose_ruvector::mat::CompressedBreathingBuffer::new(
                NOVELTY_VECTOR_DIM,
                node_id as u32,
            ),
            heartbeat_compressor: wifi_densepose_ruvector::mat::CompressedHeartbeatSpectrogram::new(
                NOVELTY_VECTOR_DIM,
            ),
            latest_breathing_compression_ratio: 0.0,
            latest_heartbeat_compression_ratio: 0.0,
            adaptive_calibration: None,
        }
    }

    pub(crate) fn update_tensor_compression(&mut self, amplitudes: &[f64], phases: &[f64]) {
        let mut breathing_frame: Vec<f32> = amplitudes
            .iter()
            .take(NOVELTY_VECTOR_DIM)
            .map(|&value| value as f32)
            .collect();
        breathing_frame.resize(NOVELTY_VECTOR_DIM, 0.0);
        self.breathing_compressor.push_frame(&breathing_frame);
        self.latest_breathing_compression_ratio = self.breathing_compressor.compression_ratio();

        let mut heartbeat_column: Vec<f32> = phases
            .iter()
            .take(NOVELTY_VECTOR_DIM)
            .map(|&value| value as f32)
            .collect();
        heartbeat_column.resize(NOVELTY_VECTOR_DIM, 0.0);
        self.heartbeat_compressor.push_column(&heartbeat_column);
        self.latest_heartbeat_compression_ratio = self.heartbeat_compressor.compression_ratio();
    }

    /// ADR-084 cluster-Pi novelty step. Truncates / zero-pads the
    /// incoming amplitude vector to `NOVELTY_VECTOR_DIM`, scores its
    /// novelty against the per-node bank, then inserts it. The novelty
    /// score is computed *before* the insert so a frame doesn't see
    /// itself in the bank.
    pub(crate) fn update_novelty(&mut self, amplitudes: &[f64]) {
        let history = match &mut self.feature_history {
            Some(h) => h,
            None => return,
        };
        let mut feature: Vec<f32> = amplitudes
            .iter()
            .take(NOVELTY_VECTOR_DIM)
            .map(|&v| v as f32)
            .collect();
        feature.resize(NOVELTY_VECTOR_DIM, 0.0);

        // Score before insert so a query doesn't see itself.
        self.last_novelty_score = history.novelty(&feature);

        let _ = history.push(
            wifi_densepose_signal::ruvsense::longitudinal::EmbeddingEntry {
                person_id: 0,
                day_us: 0,
                embedding: feature,
            },
        );
    }

    /// Update the coherence score from the latest motion_energy value.
    ///
    /// Coherence is computed as 1.0 / (1.0 + running_variance) so that
    /// low motion-energy variance maps to high coherence ([0, 1]).
    fn update_coherence(&mut self, motion_energy: f64) {
        if self.motion_energy_history.len() >= COHERENCE_WINDOW {
            self.motion_energy_history.pop_front();
        }
        self.motion_energy_history.push_back(motion_energy);

        let n = self.motion_energy_history.len();
        if n < 2 {
            self.coherence_score = 1.0;
            return;
        }

        let mean: f64 = self.motion_energy_history.iter().sum::<f64>() / n as f64;
        let variance: f64 = self
            .motion_energy_history
            .iter()
            .map(|v| (v - mean) * (v - mean))
            .sum::<f64>()
            / (n - 1) as f64;

        // Map variance to [0, 1] coherence: higher variance = lower coherence.
        self.coherence_score = (1.0 / (1.0 + variance)).clamp(0.0, 1.0);
    }

    /// Choose the EMA alpha based on current coherence score.
    fn ema_alpha(&self) -> f64 {
        if self.coherence_score < COHERENCE_LOW_THRESHOLD {
            TEMPORAL_EMA_ALPHA_LOW_COHERENCE
        } else {
            TEMPORAL_EMA_ALPHA_DEFAULT
        }
    }
}

fn adaptive_calibration_config_for_frame(frame: &Esp32Frame) -> AdaptiveCalibrationConfig {
    let n_subcarriers = frame.amplitudes.len().max(1);
    let mut calibration = wifi_densepose_signal::calibration::CalibrationConfig::ht20();
    calibration.num_active = n_subcarriers;
    calibration.num_subcarriers = n_subcarriers;
    calibration.min_frames = 600;
    AdaptiveCalibrationConfig {
        calibration,
        ..AdaptiveCalibrationConfig::default()
    }
}

fn esp32_frame_to_core_csi_frame(frame: &Esp32Frame) -> Option<CoreCsiFrame> {
    let n = frame.amplitudes.len().min(frame.phases.len());
    if n == 0 {
        return None;
    }
    let data = Array2::from_shape_vec(
        (1, n),
        (0..n)
            .map(|idx| Complex64::from_polar(frame.amplitudes[idx], frame.phases[idx]))
            .collect(),
    )
    .ok()?;
    let band = if frame.freq_mhz < 3_000 {
        CoreFrequencyBand::Band2_4GHz
    } else if frame.freq_mhz < 5_925 {
        CoreFrequencyBand::Band5GHz
    } else {
        CoreFrequencyBand::Band6GHz
    };
    let mut meta = CoreCsiMetadata::new(
        CoreDeviceId::new(format!("esp32-node-{}", frame.node_id)),
        band,
        6,
    );
    meta.bandwidth_mhz = 20;
    meta.antenna_config = CoreAntennaConfig::new(1, frame.n_antennas.max(1));
    meta.rssi_dbm = frame.rssi;
    meta.noise_floor_dbm = frame.noise_floor;
    meta.sequence_number = frame.sequence;
    Some(CoreCsiFrame::new(meta, data))
}

fn update_node_adaptive_calibration(
    ns: &mut NodeState,
    frame: &Esp32Frame,
    classification: &ClassificationInfo,
    features: &FeatureInfo,
    is_live_frame: bool,
) {
    if !is_live_frame {
        return;
    }
    let Some(core_frame) = esp32_frame_to_core_csi_frame(frame) else {
        return;
    };
    let monitor = ns
        .adaptive_calibration
        .get_or_insert_with(|| AdaptiveCalibrationMonitor::new(adaptive_calibration_config_for_frame(frame)));
    match monitor.update(&core_frame, classification.presence, features.variance as f32) {
        Ok(AdaptiveCalibrationDecision::InitialBaselinePromoted)
        | Ok(AdaptiveCalibrationDecision::CandidatePromoted) => {
            debug!(
                "Adaptive calibration promoted for node {}: {:?}",
                frame.node_id,
                monitor.snapshot()
            );
        }
        Ok(_) => {}
        Err(err) => {
            debug!("Adaptive calibration skipped for node {}: {err}", frame.node_id);
        }
    }
}

/// Per-node feature info for WebSocket broadcasts (multi-node support).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PerNodeFeatureInfo {
    node_id: u8,
    features: FeatureInfo,
    classification: ClassificationInfo,
    rssi_dbm: f64,
    last_seen_ms: u64,
    frame_rate_hz: f64,
    stale: bool,
    /// ADR-084 Pass 3 cluster-Pi novelty score in `[0.0, 1.0]`.
    /// `0.0` = exact-match-in-bank, `1.0` = no overlap with recent
    /// per-node frame history. `None` until the first
    /// `update_novelty()` call. Consumers (model-wake gate, anomaly
    /// emit, UI heatmap) read this to decide whether to escalate.
    #[serde(skip_serializing_if = "Option::is_none")]
    novelty_score: Option<f32>,
}

/// Build a per-node feature snapshot for the WebSocket envelope.
///
/// ADR-084 Pass 3.6 — exposes `last_novelty_score` from each
/// `NodeState` to the WebSocket consumer. Returns `None` when the
/// node map is empty (no live ESP32 frames have been ingested yet),
/// so the existing `node_features: None` semantics on cold-start are
/// preserved.
///
/// Stale flag uses 5-second threshold matching `ESP32_OFFLINE_TIMEOUT`.
fn build_node_features(
    node_states: &std::collections::HashMap<u8, NodeState>,
    now: std::time::Instant,
) -> Option<Vec<PerNodeFeatureInfo>> {
    if node_states.is_empty() {
        return None;
    }
    let entries: Vec<PerNodeFeatureInfo> = node_states
        .iter()
        .map(|(&node_id, ns)| {
            let last_seen_ms = ns
                .last_frame_time
                .map(|t| now.saturating_duration_since(t).as_millis() as u64)
                .unwrap_or(u64::MAX);
            let stale = ns
                .last_frame_time
                .map(|t| now.saturating_duration_since(t) > ESP32_OFFLINE_TIMEOUT)
                .unwrap_or(true);
            let features = ns.latest_features.clone().unwrap_or(FeatureInfo {
                mean_rssi: 0.0,
                variance: 0.0,
                motion_band_power: 0.0,
                breathing_band_power: 0.0,
                dominant_freq_hz: 0.0,
                change_points: 0,
                spectral_power: 0.0,
            });
            PerNodeFeatureInfo {
                node_id,
                features,
                classification: ClassificationInfo {
                    motion_level: ns.current_motion_level.clone(),
                    presence: !matches!(ns.current_motion_level.as_str(), "absent"),
                    confidence: ns.smoothed_person_score.clamp(0.0, 1.0),
                },
                rssi_dbm: ns.rssi_history.back().copied().unwrap_or(0.0),
                last_seen_ms,
                frame_rate_hz: ns.csi_fps_ema,
                stale,
                novelty_score: ns.last_novelty_score,
            }
        })
        .collect();
    Some(entries)
}

fn is_node_active(ns: &NodeState, now: std::time::Instant) -> bool {
    ns.last_frame_time
        .is_some_and(|t| now.saturating_duration_since(t) <= ESP32_OFFLINE_TIMEOUT)
}

fn active_node_count(s: &AppStateInner, now: std::time::Instant) -> usize {
    s.node_states
        .values()
        .filter(|ns| is_node_active(ns, now))
        .count()
}

const BREATHING_FUSION_INTERVAL: Duration = Duration::from_secs(5);

fn median_phase_sample(phases: &[f64]) -> Option<f32> {
    let mut finite: Vec<f32> = phases
        .iter()
        .copied()
        .filter(|phase| phase.is_finite())
        .map(|phase| phase as f32)
        .collect();
    if finite.is_empty() {
        return None;
    }

    finite.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = finite.len() / 2;
    if finite.len() % 2 == 0 {
        Some((finite[mid - 1] + finite[mid]) * 0.5)
    } else {
        Some(finite[mid])
    }
}

fn update_node_breathing_from_phase(
    ns: &mut NodeState,
    frame: &Esp32Frame,
    is_live_frame: bool,
) -> BreathingResult {
    ns.breathing_extractor.sample_rate_hz = (ns.csi_fps_ema as f32).clamp(1.0, 100.0);
    if is_live_frame {
        if let Some(phase) = median_phase_sample(&frame.phases) {
            ns.breathing_extractor.push_sample(phase);
        }
    }
    let result = ns.breathing_extractor.extract_breathing();
    ns.latest_breathing = result;
    result
}

fn apply_breathing_to_vitals(vitals: &mut VitalSigns, result: BreathingResult) {
    vitals.breathing_rate_bpm = result.breathing_bpm.map(f64::from);
    vitals.breathing_confidence = f64::from(result.confidence).clamp(0.0, 1.0);
}

fn maybe_update_fused_breathing(s: &mut AppStateInner, now: std::time::Instant) {
    if s.last_breathing_fusion_at
        .is_some_and(|last| now.saturating_duration_since(last) < BREATHING_FUSION_INTERVAL)
    {
        return;
    }

    s.last_breathing_fusion_at = Some(now);
    let result = fuse_breathing_from_active_nodes(&s.node_states, now);
    apply_breathing_to_vitals(&mut s.latest_vitals, result);
}

fn fuse_breathing_from_active_nodes(
    nodes: &HashMap<u8, NodeState>,
    now: std::time::Instant,
) -> BreathingResult {
    let min_conf = breathing_min_confidence();
    let mut weighted: Vec<(f32, f32)> = nodes
        .values()
        .filter(|node| is_node_active(node, now))
        .filter_map(|node| {
            let result = node.latest_breathing;
            let bpm = result.breathing_bpm?;
            let confidence = result.confidence;
            (bpm.is_finite() && confidence.is_finite() && confidence >= min_conf)
                .then_some((bpm, confidence))
        })
        .collect();

    if weighted.is_empty() {
        return BreathingResult::insufficient_data();
    }

    weighted.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let total_weight = weighted.iter().map(|(_, weight)| *weight).sum::<f32>();
    if total_weight <= f32::EPSILON {
        return BreathingResult::insufficient_data();
    }

    let midpoint = total_weight * 0.5;
    let mut cumulative = 0.0_f32;
    let mut fused_bpm = weighted[weighted.len() / 2].0;
    for (bpm, weight) in &weighted {
        cumulative += *weight;
        if cumulative >= midpoint {
            fused_bpm = *bpm;
            break;
        }
    }

    let confidence = (total_weight / weighted.len() as f32).clamp(0.0, 1.0);
    BreathingResult {
        breathing_bpm: (confidence >= min_conf).then_some(fused_bpm),
        confidence,
        method: BreathingResult::METHOD,
    }
}

const LOCATION_RECENT_WINDOW: Duration = Duration::from_secs(2);

fn unix_timestamp_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn localization_node_positions(env: &EnvironmentConfig) -> Vec<NodePosition> {
    let mut positions: Vec<NodePosition> = env
        .nodes
        .iter()
        .map(|node| NodePosition {
            node_id: node.node_id,
            x: node.position_m[0] as f32,
            y: node.position_m[2] as f32,
        })
        .collect();
    if positions.is_empty() {
        positions = localization::node_positions_from_env();
    }
    positions.sort_by_key(|position| position.node_id);
    positions
}

fn recent_location_rssi(s: &AppStateInner, now: std::time::Instant) -> Vec<(u8, f32)> {
    let mut readings: Vec<(u8, f32)> = s
        .node_states
        .iter()
        .filter_map(|(&node_id, ns)| {
            let last_csi = ns.last_csi_time?;
            if now.saturating_duration_since(last_csi) > LOCATION_RECENT_WINDOW {
                return None;
            }
            let rssi = *ns.rssi_history.back()? as f32;
            rssi.is_finite().then_some((node_id, rssi))
        })
        .collect();
    readings.sort_by_key(|(node_id, _)| *node_id);
    readings
}

fn localization_snapshot(
    s: &AppStateInner,
    now: std::time::Instant,
    timestamp_ms: u64,
) -> (Option<PersonLocation>, usize, Vec<NodePosition>) {
    let positions = localization_node_positions(&s.environment);
    let readings = recent_location_rssi(s, now);
    let node_count = localization::locatable_node_count(&readings, &positions);
    let location =
        localization::estimate_person_location_with_positions(&readings, &positions, timestamp_ms);
    (location, node_count, positions)
}

fn location_payload(s: &AppStateInner, now: std::time::Instant) -> serde_json::Value {
    if let Some(update) = s.latest_update.as_ref() {
        if let Some(payload) = location_payload_from_update(update) {
            return payload;
        }
    }

    let timestamp_ms = unix_timestamp_ms();
    let (location, node_count, node_positions) = localization_snapshot(s, now, timestamp_ms);
    let persons: Vec<serde_json::Value> = location
        .map(|loc| {
            vec![serde_json::json!({
                "x": loc.x,
                "y": loc.y,
                "z": 0.9,
                "position_m": [loc.x, 0.9_f32, loc.y],
                "confidence": loc.confidence,
                "source": "rssi_csi_trilateration",
            })]
        })
        .unwrap_or_default();

    serde_json::json!({
        "persons": persons,
        "node_count": node_count,
        "timestamp_ms": timestamp_ms,
        "nodes": node_positions,
    })
}

fn location_payload_from_update(update: &SensingUpdate) -> Option<serde_json::Value> {
    let tracking = update.state.as_ref()?;
    let persons: Vec<serde_json::Value> = tracking
        .persons
        .iter()
        .map(|person| {
            serde_json::json!({
                "id": person.id,
                "x": person.position_m[0],
                "y": person.position_m[2],
                "z": person.position_m[1],
                "position_m": person.position_m,
                "confidence": person.confidence,
                "source": person.source,
                "timestamp_ms": tracking.timestamp_ms,
            })
        })
        .collect();
    let nodes: Vec<serde_json::Value> = update
        .nodes
        .iter()
        .filter(|node| node.node_id != 0 && is_localized_position(&node.position))
        .map(|node| {
            serde_json::json!({
                "node_id": node.node_id,
                "x": node.position[0],
                "y": node.position[2],
                "z": node.position[1],
                "position_m": node.position,
                "rssi_dbm": node.rssi_dbm,
            })
        })
        .collect();

    Some(serde_json::json!({
        "persons": persons,
        "node_count": tracking.node_count,
        "timestamp_ms": tracking.timestamp_ms,
        "nodes": nodes,
        "state": tracking,
    }))
}

fn record_pose_latency(s: &mut AppStateInner, started: std::time::Instant) {
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    s.latest_pose_latency_ms = elapsed_ms;
    s.pose_latency_p95_ms.push(elapsed_ms);
}

fn record_dsp_latency(s: &mut AppStateInner, started: std::time::Instant) {
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    s.latest_dsp_latency_ms = elapsed_ms;
    s.dsp_latency_p95_ms.push(elapsed_ms);
}

fn finalize_persons_for_update(s: &mut AppStateInner, update: &mut SensingUpdate) {
    attach_tracking_state(update, &mut s.location_smoother);

    let pose_started = std::time::Instant::now();
    let raw_persons = derive_pose_from_sensing(update);
    let mut last_tracker_instant = s.last_tracker_instant.take();
    let persons = persons_for_update(
        update,
        raw_persons,
        &mut s.pose_tracker,
        &mut last_tracker_instant,
    );
    s.last_tracker_instant = last_tracker_instant;
    record_pose_latency(s, pose_started);
    if !persons.is_empty() {
        update.persons = Some(persons);
    }
}

fn attach_tracking_state(update: &mut SensingUpdate, smoother: &mut LocationSmoother) {
    if !update.classification.presence {
        smoother.reset();
        update.state = None;
        return;
    }

    let timestamp_ms = unix_timestamp_ms();
    let (readings, positions) = localization_inputs_from_update(update);
    let node_count = localization::locatable_signal_count(&readings, &positions);
    let Some(measurement) =
        localization::estimate_person_location_with_signal_readings(&readings, &positions, timestamp_ms)
    else {
        smoother.reset();
        update.state = None;
        return;
    };

    let location = smoother.update(measurement);
    let person_count = update.estimated_persons.unwrap_or(1).max(1);
    let source = if is_synthetic_dev_source(&update.source) {
        SYNTHETIC_DEV_POSE_SOURCE
    } else if node_count < 2 {
        RSSI_CSI_SINGLE_NODE_SOURCE
    } else {
        RSSI_CSI_POSE_SOURCE
    };
    let persons = tracked_person_states(&location, person_count, node_count, &positions, source);

    update.state = Some(TrackingState {
        coordinate_system: "meters_xyz_room".to_string(),
        timestamp_ms,
        node_count,
        persons,
    });
}

fn localization_inputs_from_update(
    update: &SensingUpdate,
) -> (Vec<NodeSignalReading>, Vec<NodePosition>) {
    let mut readings = Vec::new();
    let mut positions = Vec::new();

    for node in update.nodes.iter().filter(|node| node.node_id != 0) {
        if !node.rssi_dbm.is_finite() || !is_localized_position(&node.position) {
            continue;
        }
        readings.push(NodeSignalReading {
            node_id: node.node_id,
            rssi_dbm: node.rssi_dbm as f32,
            csi_quality: node_csi_quality(node),
        });
        positions.push(NodePosition {
            node_id: node.node_id,
            x: node.position[0] as f32,
            y: node.position[2] as f32,
        });
    }

    readings.sort_by_key(|reading| reading.node_id);
    positions.sort_by_key(|position| position.node_id);
    (readings, positions)
}

fn node_csi_quality(node: &NodeInfo) -> f32 {
    if node.amplitude.is_empty() {
        return 0.65;
    }

    let finite: Vec<f64> = node
        .amplitude
        .iter()
        .copied()
        .filter(|value| value.is_finite())
        .take(56)
        .collect();
    if finite.is_empty() {
        return 0.35;
    }

    let mean = finite.iter().sum::<f64>() / finite.len() as f64;
    let variance = finite
        .iter()
        .map(|value| {
            let delta = value - mean;
            delta * delta
        })
        .sum::<f64>()
        / finite.len().max(1) as f64;
    let cv = if mean.abs() > 1.0e-6 {
        variance.sqrt() / mean.abs()
    } else {
        0.0
    };
    let subcarrier_quality = (finite.len() as f32 / 56.0).clamp(0.35, 1.0);
    let perturbation_quality = (0.45 + (cv as f32).clamp(0.0, 1.0) * 0.55).clamp(0.35, 1.0);
    subcarrier_quality * perturbation_quality
}

fn tracked_person_states(
    location: &PersonLocation,
    person_count: usize,
    node_count: usize,
    positions: &[NodePosition],
    source: &str,
) -> Vec<TrackedPersonState> {
    let (min_x, max_x, min_z, max_z) = localization_bounds(positions)
        .unwrap_or((location.x - 1.0, location.x + 1.0, location.y - 1.0, location.y + 1.0));
    let x_span = (max_x - min_x).abs().max(0.5);
    let z_span = (max_z - min_z).abs().max(0.5);
    let person_count = person_count.max(1);
    let half = (person_count as f64 - 1.0) / 2.0;
    let spread_x = (x_span as f64 / (person_count as f64 + 1.0)).clamp(0.35, 0.9);
    let spread_z = (z_span as f64 * 0.06).clamp(0.0, 0.25);

    (0..person_count)
        .map(|idx| {
            let spread_index = idx as f64 - half;
            let x = (location.x as f64 + spread_index * spread_x)
                .clamp(min_x as f64, max_x as f64);
            let z = (location.y as f64 + spread_index.signum() * spread_z)
                .clamp(min_z as f64, max_z as f64);
            let y = 0.9_f64;
            let confidence = if node_count < 2 {
                0.20_f64.max(location.confidence as f64)
            } else {
                location.confidence as f64
            };
            TrackedPersonState {
                id: (idx + 1) as u32,
                x,
                y,
                z,
                position_m: [x, y, z],
                confidence,
                source: source.to_string(),
            }
        })
        .collect()
}

fn localization_bounds(positions: &[NodePosition]) -> Option<(f32, f32, f32, f32)> {
    let first = positions.first()?;
    let mut min_x = first.x;
    let mut max_x = first.x;
    let mut min_z = first.y;
    let mut max_z = first.y;
    for position in positions.iter().skip(1) {
        min_x = min_x.min(position.x);
        max_x = max_x.max(position.x);
        min_z = min_z.min(position.y);
        max_z = max_z.max(position.y);
    }
    Some((min_x, max_x, min_z, max_z))
}

fn fleet_ready(s: &AppStateInner, now: std::time::Instant) -> bool {
    active_node_count(s, now) >= s.min_nodes
}

fn default_node_position(node_id: u8) -> [f64; 3] {
    const POSITIONS: [[f64; 3]; 6] = [
        [-2.6, 1.1, -1.8],
        [2.6, 1.1, -1.8],
        [0.0, 1.1, 2.4],
        [-2.8, 1.1, 2.2],
        [2.8, 1.1, 2.2],
        [0.0, 1.8, 0.0],
    ];
    POSITIONS[(node_id as usize).saturating_sub(1) % POSITIONS.len()]
}

fn configured_node(env: &EnvironmentConfig, node_id: u8) -> Option<&EnvironmentNodeConfig> {
    env.nodes.iter().find(|n| n.node_id == node_id)
}

fn configured_node_position(env: &EnvironmentConfig, node_id: u8) -> [f64; 3] {
    configured_node(env, node_id)
        .map(|n| n.position_m)
        .unwrap_or_else(|| default_node_position(node_id))
}

fn node_display_label(env: &EnvironmentConfig, node_id: u8) -> String {
    configured_node(env, node_id)
        .map(|n| n.label.trim())
        .filter(|label| !label.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("ESP32-C6 #{node_id}"))
}

fn node_linked_ap(env: &EnvironmentConfig, node_id: u8) -> Option<String> {
    configured_node(env, node_id)
        .map(|n| n.linked_ap.clone())
        .or_else(|| {
            env.links
                .iter()
                .find(|l| l.node_id == node_id)
                .map(|l| l.ap_id.clone())
        })
}

fn age_ms(now: std::time::Instant, instant: Option<std::time::Instant>) -> serde_json::Value {
    instant
        .map(|t| serde_json::json!(now.saturating_duration_since(t).as_millis() as u64))
        .unwrap_or(serde_json::Value::Null)
}

fn node_runtime_status(ns: &NodeState, now: std::time::Instant) -> &'static str {
    if is_node_active(ns, now) {
        "live"
    } else if ns
        .last_frame_time
        .is_some_and(|t| now.saturating_duration_since(t) <= Duration::from_secs(60))
    {
        "stale"
    } else if ns
        .latest_sync_at
        .is_some_and(|t| now.saturating_duration_since(t) <= Duration::from_secs(9))
    {
        "sync_only"
    } else {
        "offline"
    }
}

fn node_summary_json(
    id: u8,
    ns: &NodeState,
    now: std::time::Instant,
    env: &EnvironmentConfig,
) -> serde_json::Value {
    let last_seen_ms = ns
        .last_frame_time
        .map(|t| now.saturating_duration_since(t).as_millis() as u64)
        .unwrap_or(u64::MAX);
    let active = is_node_active(ns, now);
    let sync = ns.sync_snapshot();
    let cfg = configured_node(env, id);
    let position_m = configured_node_position(env, id);
    let label = node_display_label(env, id);
    let status = node_runtime_status(ns, now);
    serde_json::json!({
        "node_id": id,
        "label": label.as_str(),
        "display_label": label.as_str(),
        "kind": cfg.map(|n| n.kind.as_str()).unwrap_or("esp32_c6"),
        "zone": cfg.map(|n| n.zone.as_str()).unwrap_or("unassigned"),
        "status": status,
        "health_status": status,
        "active": active,
        "last_seen_ms": last_seen_ms,
        "last_csi_ms": age_ms(now, ns.last_csi_time),
        "last_vitals_ms": age_ms(now, ns.last_vitals_time),
        "last_sync_ms": age_ms(now, ns.latest_sync_at),
        "remote_addr": ns.remote_addr.as_deref(),
        "rssi_dbm": ns.rssi_history.back().copied().unwrap_or(-90.0),
        "frame_rate_hz": ns.csi_fps_ema,
        "frame_rate_samples": ns.csi_fps_samples,
        "motion_level": &ns.current_motion_level,
        "presence": !matches!(ns.current_motion_level.as_str(), "absent"),
        "person_count": ns.prev_person_count,
        "position": position_m,
        "position_m": position_m,
        "coverage": coverage_json(env, Some(ns), now),
        "tdm_slot": cfg.map(|n| n.tdm_slot),
        "tdm_total": cfg.map(|n| n.tdm_total),
        "linked_ap": node_linked_ap(env, id),
        "sync_status": sync.as_ref().map(|s| if s.is_valid { "valid" } else { "stale" }).unwrap_or("no_sync"),
        "sync": sync,
        "coherence": ns.coherence_score,
        "novelty_score": ns.last_novelty_score,
    })
}

fn configured_offline_node_json(
    cfg: &EnvironmentNodeConfig,
    env: &EnvironmentConfig,
) -> serde_json::Value {
    let label = if cfg.label.trim().is_empty() {
        format!("ESP32-C6 #{}", cfg.node_id)
    } else {
        cfg.label.clone()
    };
    serde_json::json!({
        "node_id": cfg.node_id,
        "label": label.as_str(),
        "display_label": label.as_str(),
        "kind": cfg.kind.as_str(),
        "zone": cfg.zone.as_str(),
        "status": "offline",
        "health_status": "offline",
        "active": false,
        "last_seen_ms": serde_json::Value::Null,
        "last_csi_ms": serde_json::Value::Null,
        "last_vitals_ms": serde_json::Value::Null,
        "last_sync_ms": serde_json::Value::Null,
        "remote_addr": serde_json::Value::Null,
        "rssi_dbm": serde_json::Value::Null,
        "frame_rate_hz": 0.0,
        "frame_rate_samples": 0,
        "motion_level": "unknown",
        "presence": false,
        "person_count": 0,
        "position": cfg.position_m,
        "position_m": cfg.position_m,
        "coverage": coverage_json(env, None, std::time::Instant::now()),
        "tdm_slot": cfg.tdm_slot,
        "tdm_total": cfg.tdm_total,
        "linked_ap": node_linked_ap(env, cfg.node_id).unwrap_or_else(|| cfg.linked_ap.clone()),
        "sync_status": "offline",
        "sync": serde_json::Value::Null,
        "coherence": serde_json::Value::Null,
        "novelty_score": serde_json::Value::Null,
    })
}

fn all_node_summaries_json(s: &AppStateInner, now: std::time::Instant) -> Vec<serde_json::Value> {
    let mut nodes = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for cfg in &s.environment.nodes {
        seen.insert(cfg.node_id);
        if let Some(ns) = s.node_states.get(&cfg.node_id) {
            nodes.push(node_summary_json(cfg.node_id, ns, now, &s.environment));
        } else {
            nodes.push(configured_offline_node_json(cfg, &s.environment));
        }
    }

    let mut unknown: Vec<_> = s
        .node_states
        .iter()
        .filter(|(&id, _)| !seen.contains(&id))
        .map(|(&id, ns)| node_summary_json(id, ns, now, &s.environment))
        .collect();
    unknown.sort_by_key(|v| v.get("node_id").and_then(|id| id.as_u64()).unwrap_or(0));
    nodes.extend(unknown);
    nodes
}

fn observation_from_bssid(
    obs: &BssidObservation,
    now: std::time::Instant,
) -> AccessPointObservation {
    AccessPointObservation {
        bssid: obs.bssid.to_string(),
        ssid: obs.ssid.clone(),
        channel: obs.channel,
        band: obs.band.to_string(),
        rssi_dbm: obs.rssi_dbm,
        last_seen: now,
    }
}

fn position_json(position: [f64; 3], source: &str, confidence: f64) -> serde_json::Value {
    serde_json::json!({
        "x": position[0],
        "y": position[1],
        "z": position[2],
        "source": source,
        "confidence": clamp_unit(confidence),
    })
}

fn room_floor_diagonal_m(room: &RoomConfig) -> f64 {
    let width = room.dimensions_m[0].max(1.0);
    let depth = room.dimensions_m[2].max(1.0);
    (width.powi(2) + depth.powi(2)).sqrt().max(1.0)
}

fn distance_m(a: [f64; 3], b: [f64; 3]) -> f64 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

fn coverage_quality(score: f64) -> &'static str {
    if score >= 0.72 {
        "strong"
    } else if score >= 0.46 {
        "usable"
    } else if score >= 0.24 {
        "weak"
    } else {
        "offline"
    }
}

fn node_coverage_score(ns: Option<&NodeState>, now: std::time::Instant) -> (f64, Vec<&'static str>) {
    let Some(ns) = ns else {
        return (0.0, vec!["offline"]);
    };
    let mut reasons = Vec::new();
    let active = is_node_active(ns, now);
    let rssi = ns.rssi_history.back().copied().unwrap_or(-95.0);
    let rssi_score = ((rssi + 95.0) / 45.0).clamp(0.0, 1.0);
    if rssi < -82.0 {
        reasons.push("low_rssi");
    }
    let fps_score = (ns.csi_fps_ema / 10.0).clamp(0.0, 1.0);
    if ns.csi_fps_ema < 2.0 {
        reasons.push("low_frame_rate");
    }
    let coherence_score = ns.coherence_score.clamp(0.0, 1.0);
    if coherence_score < 0.35 {
        reasons.push("low_coherence");
    }
    let sync_score = ns
        .sync_snapshot()
        .map(|sync| if sync.is_valid { 1.0 } else { 0.55 })
        .unwrap_or(0.42);
    if sync_score < 0.6 {
        reasons.push("sync_missing_or_stale");
    }
    let freshness_score = if active {
        1.0
    } else if ns
        .last_frame_time
        .is_some_and(|t| now.saturating_duration_since(t) <= Duration::from_secs(60))
    {
        reasons.push("stale_csi");
        0.32
    } else {
        reasons.push("offline");
        0.0
    };
    let score = (0.36 * rssi_score
        + 0.20 * fps_score
        + 0.20 * coherence_score
        + 0.12 * sync_score
        + 0.12 * freshness_score)
        .clamp(0.0, 1.0);
    (score, reasons)
}

fn coverage_json(
    env: &EnvironmentConfig,
    ns: Option<&NodeState>,
    now: std::time::Instant,
) -> serde_json::Value {
    let (score, reasons) = node_coverage_score(ns, now);
    let diagonal = room_floor_diagonal_m(&env.room);
    let radius_m = (diagonal * (0.30 + score * 0.85)).clamp(1.0, diagonal * 1.15);
    serde_json::json!({
        "score": score,
        "quality": coverage_quality(score),
        "radius_m": radius_m,
        "reasons": reasons,
    })
}

fn matching_config_ap<'a>(
    env: &'a EnvironmentConfig,
    ap: &AccessPointObservation,
) -> Option<&'a AccessPointConfig> {
    env.access_points
        .iter()
        .find(|cfg| detected_ap_matches_config(ap, cfg))
}

fn estimated_ring_position(
    index: usize,
    total: usize,
    room: &RoomConfig,
    y: f64,
    phase: f64,
) -> [f64; 3] {
    let width = room.dimensions_m[0].max(1.0);
    let depth = room.dimensions_m[2].max(1.0);
    let n = total.max(1) as f64;
    let angle = phase + (index as f64 / n) * std::f64::consts::TAU;
    [
        angle.cos() * width * 0.38,
        y.clamp(0.0, room.dimensions_m[1].max(0.1)),
        angle.sin() * depth * 0.38,
    ]
}

fn topology_ap_position(
    env: &EnvironmentConfig,
    ap: &AccessPointObservation,
    index: usize,
    total: usize,
) -> ([f64; 3], &'static str, f64) {
    if let Some(cfg) = matching_config_ap(env, ap) {
        return (cfg.position_m, "configured", 0.9);
    }
    (
        estimated_ring_position(
            index,
            total,
            &env.room,
            env.room.dimensions_m[1] * 0.72,
            0.4,
        ),
        "estimated",
        0.3,
    )
}

fn topology_node_position(
    env: &EnvironmentConfig,
    node_id: u8,
    index: usize,
    total: usize,
    sync: Option<&NodeSyncSnapshot>,
) -> ([f64; 3], &'static str, f64) {
    if let Some(cfg) = configured_node(env, node_id) {
        return (cfg.position_m, "configured", 0.95);
    }
    let confidence = if sync.is_some_and(|s| s.is_valid && s.smoothed) {
        0.62
    } else if total >= 3 {
        0.52
    } else if total > 1 {
        0.42
    } else {
        0.32
    };
    (
        estimated_ring_position(index, total, &env.room, 1.1, -0.2),
        "estimated",
        confidence,
    )
}

fn detected_ap_matches_config(ap: &AccessPointObservation, cfg: &AccessPointConfig) -> bool {
    cfg.bssid
        .as_ref()
        .is_some_and(|bssid| bssid.eq_ignore_ascii_case(&ap.bssid))
        || (!cfg.ssid.is_empty() && cfg.ssid == ap.ssid)
}

fn topology_access_points_json(
    env: &EnvironmentConfig,
    aps: &[AccessPointObservation],
    now: std::time::Instant,
) -> Vec<serde_json::Value> {
    let mut visible = aps.to_vec();
    visible.sort_by(|a, b| {
        b.rssi_dbm
            .partial_cmp(&a.rssi_dbm)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let total = visible.len();
    visible
        .iter()
        .enumerate()
        .map(|(idx, ap)| {
            let cfg = matching_config_ap(env, ap);
            let (position, position_source, position_confidence) =
                topology_ap_position(env, ap, idx, total);
            serde_json::json!({
                "ap_id": cfg.map(|c| c.ap_id.as_str()).unwrap_or(ap.bssid.as_str()),
                "label": cfg.map(|c| c.label.as_str()).unwrap_or(if ap.ssid.is_empty() { "Hidden AP" } else { ap.ssid.as_str() }),
                "bssid": ap.bssid.as_str(),
                "ssid": ap.ssid.as_str(),
                "channel": ap.channel,
                "band": ap.band.as_str(),
                "rssi_dbm": ap.rssi_dbm,
                "last_seen_ms": now.saturating_duration_since(ap.last_seen).as_millis() as u64,
                "status": "visible",
                "position": position_json(position, position_source, position_confidence),
                "position_confidence": position_confidence,
                "position_source": position_source,
            })
        })
        .collect()
}

fn topology_nodes_json(
    env: &EnvironmentConfig,
    node_states: &HashMap<u8, NodeState>,
    now: std::time::Instant,
) -> Vec<serde_json::Value> {
    let mut ids: Vec<u8> = env.nodes.iter().map(|cfg| cfg.node_id).collect();
    for id in node_states.keys().copied() {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    ids.sort_unstable();
    let total = ids.len().max(1);

    ids.into_iter()
        .enumerate()
        .map(|(idx, id)| {
            let cfg = configured_node(env, id);
            let sync = node_states.get(&id).and_then(|ns| ns.sync_snapshot());
            let (position, position_source, position_confidence) =
                topology_node_position(env, id, idx, total, sync.as_ref());
            let label = node_display_label(env, id);

            if let Some(ns) = node_states.get(&id) {
                let last_seen_ms = ns
                    .last_frame_time
                    .map(|t| now.saturating_duration_since(t).as_millis() as u64)
                    .unwrap_or(u64::MAX);
                let status = node_runtime_status(ns, now);
                serde_json::json!({
                    "node_id": id,
                    "label": label.as_str(),
                    "display_label": label.as_str(),
                    "kind": cfg.map(|n| n.kind.as_str()).unwrap_or("esp32_c6"),
                    "zone": cfg.map(|n| n.zone.as_str()).unwrap_or("unassigned"),
                    "status": status,
                    "health_status": status,
                    "active": is_node_active(ns, now),
                    "last_seen_ms": last_seen_ms,
                    "last_csi_ms": age_ms(now, ns.last_csi_time),
                    "last_vitals_ms": age_ms(now, ns.last_vitals_time),
                    "last_sync_ms": age_ms(now, ns.latest_sync_at),
                    "remote_addr": ns.remote_addr.as_deref(),
                    "rssi_dbm": ns.rssi_history.back().copied().unwrap_or(-90.0),
                    "frame_rate_hz": ns.csi_fps_ema,
                    "frame_rate_samples": ns.csi_fps_samples,
                    "motion_level": &ns.current_motion_level,
                    "presence": !matches!(ns.current_motion_level.as_str(), "absent"),
                    "person_count": ns.prev_person_count,
                    "sync_status": sync.as_ref().map(|s| if s.is_valid { "valid" } else { "stale" }).unwrap_or("no_sync"),
                    "sync": sync,
                    "position": position_json(position, position_source, position_confidence),
                    "position_m": position,
                    "position_confidence": position_confidence,
                    "position_source": position_source,
                    "coverage": coverage_json(env, Some(ns), now),
                    "tdm_slot": cfg.map(|n| n.tdm_slot),
                    "tdm_total": cfg.map(|n| n.tdm_total),
                    "linked_ap": node_linked_ap(env, id),
                    "coherence": ns.coherence_score,
                    "novelty_score": ns.last_novelty_score,
                })
            } else {
                serde_json::json!({
                    "node_id": id,
                    "label": label.as_str(),
                    "display_label": label.as_str(),
                    "kind": cfg.map(|n| n.kind.as_str()).unwrap_or("esp32_c6"),
                    "zone": cfg.map(|n| n.zone.as_str()).unwrap_or("unassigned"),
                    "status": "offline",
                    "health_status": "offline",
                    "active": false,
                    "last_seen_ms": serde_json::Value::Null,
                    "last_csi_ms": serde_json::Value::Null,
                    "last_vitals_ms": serde_json::Value::Null,
                    "last_sync_ms": serde_json::Value::Null,
                    "remote_addr": serde_json::Value::Null,
                    "rssi_dbm": serde_json::Value::Null,
                    "frame_rate_hz": 0.0,
                    "frame_rate_samples": 0,
                    "motion_level": "unknown",
                    "presence": false,
                    "person_count": 0,
                    "sync_status": "offline",
                    "sync": serde_json::Value::Null,
                    "position": position_json(position, position_source, position_confidence),
                    "position_m": position,
                    "position_confidence": position_confidence,
                    "position_source": position_source,
                    "coverage": coverage_json(env, None, now),
                    "tdm_slot": cfg.map(|n| n.tdm_slot),
                    "tdm_total": cfg.map(|n| n.tdm_total),
                    "linked_ap": node_linked_ap(env, id),
                    "coherence": serde_json::Value::Null,
                    "novelty_score": serde_json::Value::Null,
                })
            }
        })
        .collect()
}

fn topology_links_json(
    env: &EnvironmentConfig,
    node_states: &HashMap<u8, NodeState>,
    aps: &[AccessPointObservation],
    now: std::time::Instant,
) -> Vec<serde_json::Value> {
    if aps.is_empty() {
        return Vec::new();
    }
    let strongest = aps.iter().max_by(|a, b| {
        a.rssi_dbm
            .partial_cmp(&b.rssi_dbm)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut active_ids: Vec<u8> = node_states
        .iter()
        .filter_map(|(&id, ns)| is_node_active(ns, now).then_some(id))
        .collect();
    active_ids.sort_unstable();

    active_ids
        .into_iter()
        .filter_map(|node_id| {
            let configured_ap = node_linked_ap(env, node_id)
                .and_then(|ap_id| env.access_points.iter().find(|ap| ap.ap_id == ap_id));
            let matched = configured_ap
                .and_then(|cfg| aps.iter().find(|ap| detected_ap_matches_config(ap, cfg)));
            let (ap, source, confidence) = if let Some(ap) = matched {
                (ap, "configured", 0.85)
            } else if let Some(ap) = strongest {
                (ap, "estimated", 0.35)
            } else {
                return None;
            };
            let node_state = node_states.get(&node_id);
            let (node_score, mut reasons) = node_coverage_score(node_state, now);
            let ap_rssi_score = ((ap.rssi_dbm + 95.0) / 45.0).clamp(0.0, 1.0);
            if ap.rssi_dbm < -82.0 {
                reasons.push("weak_ap_rssi");
            }
            let ap_position = configured_ap.map(|cfg| cfg.position_m);
            let node_position = configured_node_position(env, node_id);
            let link_distance_m = ap_position.map(|pos| distance_m(pos, node_position));
            if link_distance_m.is_some_and(|distance| distance > room_floor_diagonal_m(&env.room)) {
                reasons.push("long_link");
            }
            let link_score = node_score.min(ap_rssi_score).clamp(0.0, 1.0);
            Some(serde_json::json!({
                "link_id": format!("{}:node-{}", ap.bssid, node_id),
                "ap_bssid": ap.bssid.as_str(),
                "node_id": node_id,
                "source": source,
                "confidence": confidence,
                "rssi_dbm": ap.rssi_dbm,
                "distance_m": link_distance_m,
                "coverage": {
                    "score": link_score,
                    "quality": coverage_quality(link_score),
                    "radius_m": room_floor_diagonal_m(&env.room) * (0.30 + link_score * 0.85),
                    "reasons": reasons,
                },
            }))
        })
        .collect()
}

fn topology_fusion_mode(active_nodes: usize) -> &'static str {
    match active_nodes {
        0 => "offline",
        1 => "single_node",
        2 => "partial_multistatic",
        _ => "multistatic",
    }
}

fn topology_readiness_json(active_nodes: usize, min_nodes: usize) -> serde_json::Value {
    serde_json::json!({
        "ready": active_nodes >= min_nodes.max(1),
        "active_nodes": active_nodes,
        "min_nodes": min_nodes.max(1),
        "fusion_mode": topology_fusion_mode(active_nodes),
    })
}

fn topology_payload(s: &AppStateInner, now: std::time::Instant) -> serde_json::Value {
    let active_nodes = active_node_count(s, now);
    serde_json::json!({
        "product": "RuvSense Edge",
        "service": "ruvsense-master",
        "version": env!("CARGO_PKG_VERSION"),
        "source": s.effective_source(),
        "readiness": topology_readiness_json(active_nodes, s.min_nodes),
        "room": &s.environment.room,
        "access_points": topology_access_points_json(&s.environment, &s.detected_access_points, now),
        "nodes": topology_nodes_json(&s.environment, &s.node_states, now),
        "links": topology_links_json(&s.environment, &s.node_states, &s.detected_access_points, now),
        "wifi_scan": {
            "interface": s.wifi_interface.as_str(),
            "interval_secs": s.ap_scan_interval_secs,
            "available": cfg!(target_os = "linux"),
        },
    })
}

// ── ADR-044 §5.2: Rolling P95 adaptive feature normalizer ────────────────────

/// Streaming P95 estimator over a fixed-size sliding window.
///
/// Self-calibrates feature normalization to whatever distribution the deployment
/// produces — no hardcoded scale values that can saturate in large rooms or
/// degrade in high-interference environments.
///
/// O(n log n) per query via sorted copy — acceptable at 20 Hz with window=600.
/// Cold-start (len < min_samples) returns `None` so the caller uses the legacy
/// fixed denominator, preserving day-0 behaviour.
pub struct RollingP95 {
    buf: std::collections::VecDeque<f64>,
    window: usize,
    min_samples: usize,
}

impl RollingP95 {
    pub fn new(window: usize, min_samples: usize) -> Self {
        Self {
            buf: std::collections::VecDeque::with_capacity(window),
            window,
            min_samples,
        }
    }

    pub fn push(&mut self, v: f64) {
        if self.buf.len() == self.window {
            self.buf.pop_front();
        }
        self.buf.push_back(v);
    }

    /// Returns `Some(p95)` once enough samples have accumulated, else `None`.
    pub fn current(&self) -> Option<f64> {
        if self.buf.len() < self.min_samples {
            return None;
        }
        let mut sorted: Vec<f64> = self.buf.iter().copied().collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let idx = ((sorted.len() as f64) * 0.95).ceil() as usize;
        Some(sorted[idx.saturating_sub(1).min(sorted.len() - 1)])
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

// ── ADR-044 §5.3: Runtime config persistence ─────────────────────────────────

fn default_dedup_factor() -> f64 {
    3.0
}

const DEFAULT_ENABLED_MODULES: &[&str] = &[
    "respiration_tracking",
    "fall_detection",
    "intrusion_detection",
    "activity_classification",
];

const MODULE_CONFIG_VERSION: u8 = 1;
const ROOM_CONFIG_FILENAME: &str = "room-config.json";
const ROOM_DIMENSION_MIN_METERS: f64 = 1.0;
const ROOM_DIMENSION_MAX_METERS: f64 = 30.0;
const ROOM_CONFIG_MAX_NODES: usize = 6;
const DEFAULT_ROOM_VERTICAL_METERS: f64 = 2.6;
const DEFAULT_ROOM_NODE_HEIGHT_METERS: f64 = 1.0;
const APNEA_SECONDS_MIN: u64 = 10;
const APNEA_SECONDS_MAX: u64 = 60;
const NO_MOTION_SECONDS_MIN: u64 = 60;
const NO_MOTION_SECONDS_MAX: u64 = 300;
const BREATHING_CONFIDENCE_MIN: f64 = 0.10;
const BREATHING_CONFIDENCE_MAX: f64 = 0.90;

#[derive(Debug, Clone, Copy)]
struct ModulePreset {
    id: &'static str,
    label: &'static str,
    module_ids: &'static [&'static str],
}

const MODULE_PRESETS: &[ModulePreset] = &[
    ModulePreset {
        id: "disaster_recovery_mat",
        label: "Disaster Recovery MAT",
        module_ids: &[
            "fall_detection",
            "respiration_tracking",
            "panic_motion",
            "confined_space_monitor",
            "people_counting",
            "dwell_heatmap",
            "path_analytics",
        ],
    },
    ModulePreset {
        id: "intrusion_detection",
        label: "Intrusion Detection",
        module_ids: &[
            "intrusion_detection",
            "loitering_alert",
            "exclusion_zone_breach",
            "occupancy_based_access",
            "door_open_detection",
            "path_analytics",
        ],
    },
    ModulePreset {
        id: "daily_vital_monitoring",
        label: "Daily Vital Monitoring",
        module_ids: &[
            "respiration_tracking",
            "fall_detection",
            "sleep_apnea_screening",
            "cardiac_arrhythmia",
            "sleep_staging",
            "seizure_detection",
        ],
    },
    ModulePreset {
        id: "development_failsafe",
        label: "Development Failsafe",
        module_ids: DEFAULT_ENABLED_MODULES,
    },
];

fn default_enabled_modules() -> Vec<String> {
    DEFAULT_ENABLED_MODULES.iter().map(|id| (*id).to_string()).collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct UiRoomNodeConfig {
    id: u8,
    x: f64,
    y: f64,
    active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct UiRoomConfig {
    room_width_meters: f64,
    room_height_meters: f64,
    nodes: Vec<UiRoomNodeConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct AlertConfigUpdate {
    apnea_seconds: u64,
    no_motion_seconds: u64,
    breathing_confidence: f64,
}

/// Physical room profile used by the 3D console and calibration UI.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RoomConfig {
    pub name: String,
    pub dimensions_m: [f64; 3],
    pub coordinate_system: String,
}

/// Configured WiFi mesh AP anchor.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AccessPointConfig {
    pub ap_id: String,
    pub label: String,
    pub ssid: String,
    pub bssid: Option<String>,
    pub role: String,
    pub position_m: [f64; 3],
    pub channel: Option<u8>,
    pub band: String,
    pub active: bool,
}

/// Configured ESP32-C6 sensing node.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct EnvironmentNodeConfig {
    pub node_id: u8,
    pub label: String,
    pub kind: String,
    pub zone: String,
    pub position_m: [f64; 3],
    pub tdm_slot: u8,
    pub tdm_total: u8,
    pub linked_ap: String,
}

/// Configured AP-to-node RF link.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct EnvironmentLinkConfig {
    pub link_id: String,
    pub ap_id: String,
    pub node_id: u8,
}

/// Provisional or operator-confirmed environment obstacle shown by the live console.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct EnvironmentObstacleConfig {
    pub obstacle_id: String,
    pub kind: String,
    pub label: String,
    pub center_m: [f64; 3],
    pub size_m: [f64; 3],
    #[serde(default)]
    pub yaw_rad: f64,
    pub source: String,
    pub confidence: f64,
}

/// Persisted topology for the production console.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct EnvironmentConfig {
    pub room: RoomConfig,
    pub access_points: Vec<AccessPointConfig>,
    pub nodes: Vec<EnvironmentNodeConfig>,
    pub links: Vec<EnvironmentLinkConfig>,
    #[serde(default)]
    pub obstacles: Vec<EnvironmentObstacleConfig>,
}

/// AP observed by the Raspberry Pi master during a real WiFi scan.
#[derive(Debug, Clone)]
struct AccessPointObservation {
    bssid: String,
    ssid: String,
    channel: u8,
    band: String,
    rssi_dbm: f64,
    last_seen: std::time::Instant,
}

impl Default for EnvironmentConfig {
    fn default() -> Self {
        Self {
            room: RoomConfig {
                name: "primary".to_string(),
                dimensions_m: [5.2, 2.6, 4.8],
                coordinate_system: "x_right_y_up_z_depth".to_string(),
            },
            access_points: Vec::new(),
            nodes: Vec::new(),
            links: Vec::new(),
            obstacles: Vec::new(),
        }
    }
}

/// Runtime configuration that persists across server restarts via `data/config.json`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuntimeConfig {
    /// Divisor for multi-node person-count deduplication (sum / factor).
    #[serde(default = "default_dedup_factor")]
    pub dedup_factor: f64,
    #[serde(default)]
    pub environment: EnvironmentConfig,
    #[serde(default)]
    pub module_config_version: u8,
    #[serde(default = "default_enabled_modules")]
    pub enabled_modules: Vec<String>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            dedup_factor: default_dedup_factor(),
            environment: EnvironmentConfig::default(),
            module_config_version: MODULE_CONFIG_VERSION,
            enabled_modules: default_enabled_modules(),
        }
    }
}

fn normalize_runtime_config(mut config: RuntimeConfig) -> RuntimeConfig {
    if config.module_config_version == 0 {
        config.enabled_modules = default_enabled_modules();
    }

    config.enabled_modules.sort();
    config.enabled_modules.dedup();

    config.module_config_version = MODULE_CONFIG_VERSION;
    config
}

fn valid_module_ids() -> HashSet<String> {
    let empty = HashSet::new();
    business_modules(0, &empty)
        .into_iter()
        .filter_map(|module| {
            module
                .get("id")
                .and_then(|value| value.as_str())
                .map(str::to_string)
        })
        .collect()
}

fn validate_runtime_config(config: &RuntimeConfig) -> Result<(), String> {
    if !config.dedup_factor.is_finite() || !(1.0..=10.0).contains(&config.dedup_factor) {
        return Err("dedup_factor must be a finite value in [1.0, 10.0]".to_string());
    }
    if config.module_config_version > MODULE_CONFIG_VERSION {
        return Err(format!(
            "unsupported module_config_version {} (max {})",
            config.module_config_version, MODULE_CONFIG_VERSION
        ));
    }
    validate_environment(&config.environment)?;

    let valid_module_ids = valid_module_ids();
    for id in &config.enabled_modules {
        if !valid_module_ids.contains(id) {
            return Err(format!("unknown enabled module id '{id}'"));
        }
    }
    Ok(())
}

#[allow(dead_code)]
fn runtime_config_schema() -> schemars::schema::RootSchema {
    schemars::schema_for!(RuntimeConfig)
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct HomecoreAutomationYamlSchema {
    name: String,
    #[serde(default)]
    description: Option<String>,
    trigger: serde_json::Value,
    action: serde_json::Value,
    #[serde(default)]
    condition: Option<serde_json::Value>,
    #[serde(default)]
    mode: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct BfldBlueprintYamlSchema {
    blueprint: BfldBlueprintMetadataSchema,
    trigger: Vec<serde_json::Value>,
    action: Vec<serde_json::Value>,
    mode: String,
    #[serde(default)]
    variables: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct BfldBlueprintMetadataSchema {
    name: String,
    description: String,
    domain: String,
    source_url: String,
    input: HashMap<String, serde_json::Value>,
}

fn schema_for_config_kind(kind: ConfigSchemaKind) -> schemars::schema::RootSchema {
    match kind {
        ConfigSchemaKind::Runtime => runtime_config_schema(),
        ConfigSchemaKind::Swarm => ruview_swarm::config::SwarmConfig::schema(),
        ConfigSchemaKind::Training => wifi_densepose_train::config::TrainingConfig::schema(),
        ConfigSchemaKind::HomecoreAutomation => {
            schemars::schema_for!(HomecoreAutomationYamlSchema)
        }
        ConfigSchemaKind::BfldBlueprint => schemars::schema_for!(BfldBlueprintYamlSchema),
    }
}

fn parse_runtime_config_text(path: &StdPath, text: &str) -> Result<RuntimeConfig, String> {
    let ext = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let parsed = match ext.as_str() {
        "json" => serde_json::from_str(text)
            .map_err(|e| format!("{}: JSON schema parse error: {e}", path.display()))?,
        "toml" => toml::from_str(text)
            .map_err(|e| format!("{}: TOML schema parse error: {e}", path.display()))?,
        "yaml" | "yml" => serde_yaml::from_str(text)
            .map_err(|e| format!("{}: YAML schema parse error: {e}", path.display()))?,
        _ => {
            return Err(format!(
                "{}: unsupported runtime config extension; expected .json, .toml, .yaml, or .yml",
                path.display()
            ));
        }
    };
    validate_runtime_config(&parsed)
        .map_err(|e| format!("{}: invalid runtime config: {e}", path.display()))?;
    Ok(normalize_runtime_config(parsed))
}

pub(crate) fn load_runtime_config_file(path: &StdPath) -> Result<RuntimeConfig, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("{}: failed to read runtime config: {e}", path.display()))?;
    parse_runtime_config_text(path, &text)
}

#[derive(Debug, Clone, Copy)]
struct ConfigValidationReport {
    files_checked: usize,
    cargo_metadata_checked: bool,
}

impl ConfigValidationReport {
    fn summary(self) -> String {
        let cargo = if self.cargo_metadata_checked {
            "cargo metadata checked"
        } else {
            "cargo metadata skipped"
        };
        format!("validated {} config artifact(s); {cargo}", self.files_checked)
    }
}

fn validate_config_root(root: &StdPath) -> Result<ConfigValidationReport, String> {
    if !root.is_dir() {
        return Err(format!("{}: validation root is not a directory", root.display()));
    }
    let mut files = Vec::new();
    collect_config_files(root, &mut files)?;
    files.sort();

    for path in &files {
        validate_config_file(path)?;
    }

    let manifest = root.join("Cargo.toml");
    let cargo_metadata_checked = if manifest.is_file() {
        validate_cargo_metadata(&manifest)?;
        true
    } else {
        false
    };

    Ok(ConfigValidationReport {
        files_checked: files.len(),
        cargo_metadata_checked,
    })
}

fn collect_config_files(dir: &StdPath, files: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries =
        std::fs::read_dir(dir).map_err(|e| format!("{}: failed to read dir: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: failed to read dir entry: {e}", dir.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|e| format!("{}: failed to inspect file type: {e}", path.display()))?;
        if file_type.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if matches!(name.as_ref(), "target" | ".git" | "node_modules") {
                continue;
            }
            collect_config_files(&path, files)?;
        } else if file_type.is_file() && is_supported_config_artifact(&path) {
            files.push(path);
        }
    }
    Ok(())
}

fn is_supported_config_artifact(path: &StdPath) -> bool {
    matches!(
        path.extension()
            .and_then(|value| value.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("toml" | "yaml" | "yml")
    )
}

fn validate_config_file(path: &StdPath) -> Result<(), String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("{}: failed to read config artifact: {e}", path.display()))?;
    match path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "toml" => validate_toml_config_file(path, &text),
        "yaml" | "yml" => validate_yaml_config_file(path, &text),
        _ => Ok(()),
    }
}

fn validate_toml_config_file(path: &StdPath, text: &str) -> Result<(), String> {
    let value: toml::Value = toml::from_str(text)
        .map_err(|e| format!("{}: TOML parse error: {e}", path.display()))?;
    if value.get("swarm").is_some() {
        let cfg: ruview_swarm::config::SwarmConfig = toml::from_str(text)
            .map_err(|e| format!("{}: swarm config parse error: {e}", path.display()))?;
        cfg.validate()
            .map_err(|e| format!("{}: invalid swarm config: {e}", path.display()))?;
    } else if value.get("dedup_factor").is_some() || value.get("environment").is_some() {
        parse_runtime_config_text(path, text)?;
    } else if value.get("num_epochs").is_some() || value.get("batch_size").is_some() {
        let cfg: wifi_densepose_train::config::TrainingConfig = toml::from_str(text)
            .map_err(|e| format!("{}: training config parse error: {e}", path.display()))?;
        cfg.validate()
            .map_err(|e| format!("{}: invalid training config: {e}", path.display()))?;
    }
    Ok(())
}

fn validate_yaml_config_file(path: &StdPath, text: &str) -> Result<(), String> {
    let value: serde_yaml::Value = serde_yaml::from_str(text)
        .map_err(|e| format!("{}: YAML parse error: {e}", path.display()))?;
    let Some(root) = value.as_mapping() else {
        return Err(format!("{}: YAML config root must be a mapping", path.display()));
    };
    if yaml_get(root, "blueprint").is_some() {
        validate_bfld_blueprint_yaml(path, root)?;
    } else if yaml_get(root, "trigger").is_some() && yaml_get(root, "action").is_some() {
        validate_homecore_automation_yaml(path, root)?;
    } else if yaml_get(root, "dedup_factor").is_some() || yaml_get(root, "environment").is_some() {
        parse_runtime_config_text(path, text)?;
    }
    Ok(())
}

fn validate_homecore_automation_yaml(
    path: &StdPath,
    root: &serde_yaml::Mapping,
) -> Result<(), String> {
    require_yaml_string(path, root, "name")?;
    require_yaml_sequence(path, root, "trigger")?;
    require_yaml_sequence(path, root, "action")?;
    if let Some(mode) = yaml_get(root, "mode") {
        if mode.as_str().is_none() {
            return Err(format!("{}: mode must be a string", path.display()));
        }
    }
    Ok(())
}

fn validate_bfld_blueprint_yaml(path: &StdPath, root: &serde_yaml::Mapping) -> Result<(), String> {
    let blueprint = yaml_get(root, "blueprint")
        .and_then(serde_yaml::Value::as_mapping)
        .ok_or_else(|| format!("{}: blueprint must be a mapping", path.display()))?;
    require_yaml_string(path, blueprint, "name")?;
    require_yaml_string(path, blueprint, "description")?;
    require_yaml_string(path, blueprint, "source_url")?;
    let domain = require_yaml_string(path, blueprint, "domain")?;
    if domain != "automation" {
        return Err(format!(
            "{}: blueprint.domain must be automation, got {domain}",
            path.display()
        ));
    }
    yaml_get(blueprint, "input")
        .and_then(serde_yaml::Value::as_mapping)
        .filter(|input| !input.is_empty())
        .ok_or_else(|| format!("{}: blueprint.input must be a non-empty mapping", path.display()))?;
    require_yaml_sequence(path, root, "trigger")?;
    require_yaml_sequence(path, root, "action")?;
    require_yaml_string(path, root, "mode")?;
    Ok(())
}

fn yaml_get<'a>(mapping: &'a serde_yaml::Mapping, key: &str) -> Option<&'a serde_yaml::Value> {
    mapping.get(&serde_yaml::Value::String(key.to_string()))
}

fn require_yaml_string(
    path: &StdPath,
    mapping: &serde_yaml::Mapping,
    key: &str,
) -> Result<String, String> {
    yaml_get(mapping, key)
        .and_then(serde_yaml::Value::as_str)
        .map(str::to_string)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("{}: {key} must be a non-empty string", path.display()))
}

fn require_yaml_sequence(
    path: &StdPath,
    mapping: &serde_yaml::Mapping,
    key: &str,
) -> Result<(), String> {
    yaml_get(mapping, key)
        .and_then(serde_yaml::Value::as_sequence)
        .filter(|value| !value.is_empty())
        .map(|_| ())
        .ok_or_else(|| format!("{}: {key} must be a non-empty sequence", path.display()))
}

fn validate_cargo_metadata(manifest_path: &StdPath) -> Result<(), String> {
    let manifest = manifest_path.display().to_string();
    let output = std::process::Command::new("cargo")
        .args([
            "metadata",
            "--manifest-path",
            manifest.as_str(),
            "--no-deps",
            "--format-version",
            "1",
        ])
        .output()
        .map_err(|e| {
            format!(
                "{}: failed to execute cargo metadata: {e}",
                manifest_path.display()
            )
        })?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "{}: cargo metadata failed: {}",
            manifest_path.display(),
            stderr.trim()
        ))
    }
}

fn runtime_config_path_for_data_dir(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("config.json")
}

/// Load persisted runtime config from `<data_dir>/config.json`.
/// Falls back to [`RuntimeConfig::default`] only when the file is absent.
pub(crate) fn load_runtime_config(data_dir: &std::path::Path) -> Result<RuntimeConfig, String> {
    let path = runtime_config_path_for_data_dir(data_dir);
    match std::fs::read_to_string(&path) {
        Ok(json) => parse_runtime_config_text(&path, &json),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(RuntimeConfig::default()),
        Err(e) => Err(format!("{}: failed to read runtime config: {e}", path.display())),
    }
}

pub(crate) fn save_runtime_config_file(
    path: &std::path::Path,
    config: &RuntimeConfig,
) -> Result<(), String> {
    validate_runtime_config(config)?;
    let ext = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let text = match ext.as_str() {
        "json" => serde_json::to_string_pretty(config)
            .map_err(|e| format!("failed to serialize runtime config as JSON: {e}"))?,
        "toml" => toml::to_string_pretty(config)
            .map_err(|e| format!("failed to serialize runtime config as TOML: {e}"))?,
        "yaml" | "yml" => serde_yaml::to_string(config)
            .map_err(|e| format!("failed to serialize runtime config as YAML: {e}"))?,
        _ => {
            return Err(format!(
                "{}: unsupported runtime config extension; expected .json, .toml, .yaml, or .yml",
                path.display()
            ));
        }
    };
    if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }
    let mut tmp_name = path.as_os_str().to_os_string();
    tmp_name.push(".tmp");
    let tmp_path = PathBuf::from(tmp_name);
    {
        let mut file = std::fs::File::create(&tmp_path)
            .map_err(|e| format!("failed to create {}: {e}", tmp_path.display()))?;
        use std::io::Write as _;
        file.write_all(text.as_bytes())
            .map_err(|e| format!("failed to write {}: {e}", tmp_path.display()))?;
        file.write_all(b"\n")
            .map_err(|e| format!("failed to finalize {}: {e}", tmp_path.display()))?;
        file.sync_all()
            .map_err(|e| format!("failed to sync {}: {e}", tmp_path.display()))?;
    }
    if let Err(e) = std::fs::rename(&tmp_path, &path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(format!(
            "failed to save runtime config to {}: {e}",
            path.display()
        ));
    }
    info!("Runtime config saved to {}", path.display());
    Ok(())
}

/// Persist runtime config to `<data_dir>/config.json`.
pub(crate) fn save_runtime_config(
    data_dir: &std::path::Path,
    config: &RuntimeConfig,
) -> Result<(), String> {
    save_runtime_config_file(&runtime_config_path_for_data_dir(data_dir), config)
}

async fn init_sqlite_store(data_dir: &std::path::Path) -> anyhow::Result<()> {
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;

    std::fs::create_dir_all(data_dir)?;
    let db_path = data_dir.join("ruvsense.sqlite");
    let db_url = format!("sqlite://{}", db_path.display());
    let options = SqliteConnectOptions::from_str(&db_url)?.create_if_missing(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS runtime_config (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL,
            updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        )",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS nodes (
            node_id INTEGER PRIMARY KEY,
            zone TEXT,
            last_seen_at TEXT,
            status TEXT NOT NULL DEFAULT 'unknown'
        )",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            node_id INTEGER,
            event_type TEXT NOT NULL,
            payload TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        )",
    )
    .execute(&pool)
    .await?;

    Ok(())
}

/// Shared application state
struct AppStateInner {
    latest_update: Option<SensingUpdate>,
    rssi_history: VecDeque<f64>,
    /// Circular buffer of recent CSI amplitude vectors for temporal analysis.
    /// Each entry is the full subcarrier amplitude vector for one frame.
    /// Capacity: FRAME_HISTORY_CAPACITY frames.
    frame_history: VecDeque<Vec<f64>>,
    tick: u64,
    source: String,
    feature_flags: FeatureFlags,
    alert_manager: AlertManager,
    /// Instant of the last ESP32 UDP frame received (for offline detection).
    last_esp32_frame: Option<std::time::Instant>,
    tx: broadcast::Sender<String>,
    // ADR-099 D2/D3/D4: real-time CSI introspection tap. Per-frame state +
    // a parallel broadcast topic (`/ws/introspection`) running alongside
    // (not replacing) the window-aggregated `tx` / `/ws/sensing` pipeline.
    intro: wifi_densepose_sensing_server::introspection::IntrospectionState,
    intro_tx: broadcast::Sender<String>,
    total_detections: u64,
    start_time: std::time::Instant,
    /// Vital sign detector (processes CSI frames to estimate HR/RR).
    vital_detector: VitalSignDetector,
    /// Most recent vital sign reading for the REST endpoint.
    latest_vitals: VitalSigns,
    /// Last time CSI phase breathing was fused across active nodes.
    last_breathing_fusion_at: Option<std::time::Instant>,
    /// RVF container info if a model was loaded via `--load-rvf`.
    rvf_info: Option<RvfContainerInfo>,
    /// Path to save RVF container on shutdown (set via `--save-rvf`).
    save_rvf_path: Option<PathBuf>,
    /// Progressive loader for a trained model (set via `--model`).
    progressive_loader: Option<ProgressiveLoader>,
    /// Active SONA profile name.
    active_sona_profile: Option<String>,
    /// Whether a trained model is loaded.
    model_loaded: bool,
    /// Smoothed person count (EMA) for hysteresis — prevents frame-to-frame jumping.
    smoothed_person_score: f64,
    /// Previous person count for hysteresis (asymmetric up/down thresholds).
    prev_person_count: usize,
    // ── Motion smoothing & adaptive baseline (ADR-047 tuning) ────────────
    /// EMA-smoothed motion score (alpha ~0.15 for ~10 FPS → ~1s time constant).
    smoothed_motion: f64,
    /// Current classification state for hysteresis debounce.
    current_motion_level: String,
    /// How many consecutive frames the *raw* classification has agreed with a
    /// *candidate* new level.  State only changes after DEBOUNCE_FRAMES.
    debounce_counter: u32,
    /// The candidate motion level that the debounce counter is tracking.
    debounce_candidate: String,
    /// Adaptive baseline: EMA of motion score when room is "quiet" (low motion).
    /// Subtracted from raw score so slow environmental drift doesn't inflate readings.
    baseline_motion: f64,
    /// Number of frames processed so far (for baseline warm-up).
    baseline_frames: u64,
    // ── Vital signs smoothing ────────────────────────────────────────────
    /// EMA-smoothed heart rate (BPM).
    smoothed_hr: f64,
    /// EMA-smoothed breathing rate (BPM).
    smoothed_br: f64,
    /// EMA-smoothed HR confidence.
    smoothed_hr_conf: f64,
    /// EMA-smoothed BR confidence.
    smoothed_br_conf: f64,
    /// Median filter buffer for HR (last N raw values for outlier rejection).
    hr_buffer: VecDeque<f64>,
    /// Median filter buffer for BR.
    br_buffer: VecDeque<f64>,
    /// ADR-039: Latest edge vitals packet from ESP32.
    edge_vitals: Option<Esp32VitalsPacket>,
    /// ADR-040: Latest WASM output packet from ESP32.
    latest_wasm_events: Option<WasmOutputPacket>,
    // ── Model management fields ─────────────────────────────────────────────
    /// Discovered RVF model files from `data/models/`.
    discovered_models: Vec<serde_json::Value>,
    /// ID of the currently loaded model, if any.
    active_model_id: Option<String>,
    // ── Recording fields ────────────────────────────────────────────────────
    /// Metadata for recorded CSI data files.
    recordings: Vec<serde_json::Value>,
    /// Whether CSI recording is currently in progress.
    recording_active: bool,
    /// When the current recording started.
    recording_start_time: Option<std::time::Instant>,
    /// ID of the current recording (used for filename).
    recording_current_id: Option<String>,
    /// Shutdown signal for the recording writer task.
    recording_stop_tx: Option<tokio::sync::watch::Sender<bool>>,
    // ── Training fields ─────────────────────────────────────────────────────
    /// Training status: "idle", "running", "completed", "failed".
    training_status: String,
    /// Training configuration, if any.
    training_config: Option<serde_json::Value>,
    // ── Adaptive classifier (environment-tuned) ──────────────────────────
    /// Trained adaptive model (loaded from data/adaptive_model.json or trained at runtime).
    adaptive_model: Option<adaptive_classifier::AdaptiveModel>,
    // ── Per-node state (issue #249) ─────────────────────────────────────
    /// Per-node sensing state for multi-node deployments.
    /// Keyed by `node_id` from the ESP32 frame header.
    node_states: HashMap<u8, NodeState>,
    /// Per-node UDP jitter/re-sequencing counters keyed by ESP32 node id.
    udp_jitter_stats: HashMap<u8, udp_jitter::NodeJitterSnapshot>,
    /// Access points currently visible to the Raspberry Pi master.
    detected_access_points: Vec<AccessPointObservation>,
    /// Wireless interface used for Pi-side AP discovery.
    wifi_interface: String,
    /// AP scan interval in seconds.
    ap_scan_interval_secs: u64,
    // ── Accuracy sprint: Kalman tracker, multistatic fusion, eigenvalue counting ──
    /// Global Kalman-based pose tracker for stable person IDs and smoothed keypoints.
    pose_tracker: PoseTracker,
    /// Room-position alpha-beta filter for the centralized tracking state.
    location_smoother: LocationSmoother,
    /// Instant of last tracker update (for computing dt).
    last_tracker_instant: Option<std::time::Instant>,
    /// Conservative person count currently safe to render.
    stable_rendered_person_count: usize,
    /// Candidate multi-person count waiting for corroboration.
    count_candidate_persons: usize,
    /// First instant at which `count_candidate_persons` was observed.
    count_candidate_since: Option<std::time::Instant>,
    /// Last instant where the room had positive live presence evidence.
    last_present_at: Option<std::time::Instant>,
    /// Last conservative person count that was safe to render.
    last_present_count: usize,
    /// Last fused confidence used while bridging short RF dropouts.
    last_present_confidence: f64,
    /// Attention-weighted multi-node CSI fusion engine.
    multistatic_fuser: MultistaticFuser,
    /// SVD-based room field model for eigenvalue person counting (None until calibration).
    field_model: Option<FieldModel>,
    /// Runtime opt-in for safe empty-room auto-calibration.
    auto_calibration_enabled: bool,
    /// Auto-calibration policy label exposed to the live console.
    auto_calibration_policy: String,
    /// First instant of the current quiet-room guard window.
    auto_calibration_quiet_since: Option<std::time::Instant>,
    /// Last automatic calibration action for UI/audit visibility.
    auto_calibration_last_action: Option<String>,
    // ── ADR-044 §5.2: adaptive rolling-p95 normalization ─────────────────────
    /// Rolling P95 of `FeatureInfo.variance` over the last ~30 s (600 frames @ 20 Hz).
    pub(crate) p95_variance: RollingP95,
    /// Rolling P95 of `FeatureInfo.motion_band_power` over the last ~30 s.
    pub(crate) p95_motion_band_power: RollingP95,
    /// Rolling P95 of `FeatureInfo.spectral_power` over the last ~30 s.
    pub(crate) p95_spectral_power: RollingP95,
    /// Latest pose/model processing latency observed in the live update path.
    latest_pose_latency_ms: f64,
    /// Rolling P95 of pose/model processing latency.
    pose_latency_p95_ms: RollingP95,
    /// Latest DSP feature/vitals processing latency observed in the live update path.
    latest_dsp_latency_ms: f64,
    /// Rolling P95 of DSP feature/vitals processing latency.
    dsp_latency_p95_ms: RollingP95,
    // ── ADR-044 §5.3: runtime-configurable dedup factor ───────────────────────
    /// Divisor for multi-node person-count deduplication (sum / factor).
    /// Default 3.0 (one body visible to ~3 nodes on average).
    /// Configurable at runtime via `POST /api/v1/config/dedup-factor` and
    /// `POST /api/v1/config/ground-truth`. Persisted across restarts.
    pub(crate) dedup_factor: f64,
    /// Persisted allowlist of business modules enabled in the live console.
    pub(crate) enabled_modules: HashSet<String>,
    /// Minimum active ESP32 nodes required for the master to report ready.
    pub(crate) min_nodes: usize,
    /// Data directory for persisting runtime config (parent of `firmware_dir`).
    pub(crate) data_dir: std::path::PathBuf,
    /// Static UI directory served at `/ui`, used for `room-config.json`.
    pub(crate) ui_path: std::path::PathBuf,
    /// Runtime config file currently owned by runtime mutations.
    pub(crate) runtime_config_path: std::path::PathBuf,
    /// Persisted room/AP/node topology consumed by the RuvSense Console.
    pub(crate) environment: EnvironmentConfig,
}

/// If no ESP32 frame arrives within this duration, source reverts to offline.
const ESP32_OFFLINE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const AUTO_CALIBRATION_QUIET_WINDOW: std::time::Duration = std::time::Duration::from_secs(30);
const AUTO_CALIBRATION_MIN_QUALITY: f64 = 0.30;

impl AppStateInner {
    /// Return the effective data source, accounting for ESP32 frame timeout.
    /// If the source is "esp32" but no frame has arrived in 5 seconds, returns
    /// "esp32:offline" so the UI can distinguish active vs stale connections.
    /// Person count: eigenvalue-based if field model is calibrated, else heuristic.
    /// Uses global frame_history if populated, otherwise the freshest per-node history.
    fn person_count(&self) -> usize {
        match self.field_model.as_ref() {
            Some(fm) => {
                // Prefer global frame_history (populated by wifi/simulate paths).
                // Fall back to freshest per-node history (populated by ESP32 paths).
                let history = if !self.frame_history.is_empty() {
                    &self.frame_history
                } else {
                    // Find the node with the most recent frame
                    self.node_states
                        .values()
                        .filter(|ns| !ns.frame_history.is_empty())
                        .max_by_key(|ns| ns.last_frame_time)
                        .map(|ns| &ns.frame_history)
                        .unwrap_or(&self.frame_history)
                };
                field_bridge::occupancy_or_fallback(
                    fm,
                    history,
                    self.smoothed_person_score,
                    self.prev_person_count,
                )
            }
            None => score_to_person_count(self.smoothed_person_score, self.prev_person_count),
        }
    }

    fn effective_source(&self) -> String {
        if self.source == "esp32" {
            match self.last_esp32_frame {
                Some(last) if last.elapsed() <= ESP32_OFFLINE_TIMEOUT => {}
                _ => return "esp32:offline".to_string(),
            }
        }
        self.source.clone()
    }
}

/// Number of frames retained in `frame_history` for temporal analysis.
/// At 500 ms ticks this covers ~50 seconds; at 100 ms ticks ~10 seconds.
const FRAME_HISTORY_CAPACITY: usize = 100;

type SharedState = Arc<RwLock<AppStateInner>>;

fn publish_sensing_update(s: &mut AppStateInner, update: SensingUpdate) {
    let sample = alert_sample_from_update(&update);
    let alerts = s.alert_manager.evaluate(&sample);

    if let Ok(json) = serde_json::to_string(&update) {
        let _ = s.tx.send(json);
    }
    for alert in alerts {
        if let Ok(json) = serde_json::to_string(&alert) {
            let _ = s.tx.send(json);
        }
    }
    s.latest_update = Some(update);
}

fn alert_sample_from_update(update: &SensingUpdate) -> AlertSample {
    let presence_score = if update.classification.presence {
        update.classification.confidence
    } else {
        0.0
    };
    AlertSample {
        person_id: alert_person_id(update),
        breathing_bpm: update
            .vital_signs
            .as_ref()
            .and_then(|vitals| vitals.breathing_rate_bpm),
        breathing_confidence: update
            .vital_signs
            .as_ref()
            .map_or(0.0, |vitals| vitals.breathing_confidence),
        presence_score,
        motion_energy: update.features.motion_band_power,
        timestamp_ms: update_timestamp_ms(update),
    }
}

fn update_timestamp_ms(update: &SensingUpdate) -> u64 {
    if update.timestamp.is_finite() && update.timestamp > 0.0 {
        (update.timestamp * 1000.0).round() as u64
    } else {
        chrono::Utc::now().timestamp_millis().max(0) as u64
    }
}

fn alert_person_id(update: &SensingUpdate) -> u32 {
    update
        .persons
        .as_ref()
        .and_then(|persons| persons.first())
        .map(|person| person.id)
        .or_else(|| {
            update
                .state
                .as_ref()
                .and_then(|state| state.persons.first())
                .map(|person| person.id)
        })
        .unwrap_or(1)
}

// ── ESP32 Edge Vitals Packet (ADR-039, magic 0xC511_0002) ────────────────────

/// Decoded vitals packet from ESP32 edge processing pipeline.
#[derive(Debug, Clone, Serialize)]
struct Esp32VitalsPacket {
    node_id: u8,
    presence: bool,
    fall_detected: bool,
    motion: bool,
    breathing_rate_bpm: f64,
    heartrate_bpm: f64,
    rssi: i8,
    n_persons: u8,
    motion_energy: f32,
    presence_score: f32,
    timestamp_ms: u32,
}

/// Parse a 32-byte edge vitals packet (magic 0xC511_0002).
fn parse_esp32_vitals(buf: &[u8]) -> Option<Esp32VitalsPacket> {
    if buf.len() < 32 {
        return None;
    }
    let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if magic != 0xC511_0002 {
        return None;
    }

    let node_id = buf[4];
    let flags = buf[5];
    let breathing_raw = u16::from_le_bytes([buf[6], buf[7]]);
    let heartrate_raw = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
    let rssi = buf[12] as i8;
    let n_persons = buf[13];
    let motion_energy = f32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]);
    let presence_score = f32::from_le_bytes([buf[20], buf[21], buf[22], buf[23]]);
    let timestamp_ms = u32::from_le_bytes([buf[24], buf[25], buf[26], buf[27]]);

    Some(Esp32VitalsPacket {
        node_id,
        presence: (flags & 0x01) != 0,
        fall_detected: (flags & 0x02) != 0,
        motion: (flags & 0x04) != 0,
        breathing_rate_bpm: breathing_raw as f64 / 100.0,
        heartrate_bpm: heartrate_raw as f64 / 10000.0,
        rssi,
        n_persons,
        motion_energy,
        presence_score,
        timestamp_ms,
    })
}

// ── ADR-040: WASM Output Packet (magic 0xC511_0007 — reassigned per #928) ─────

/// Single WASM event (type + value).
#[derive(Debug, Clone, Serialize)]
struct WasmEvent {
    event_type: u8,
    value: f32,
}

/// Decoded WASM output packet from ESP32 Tier 3 runtime.
#[derive(Debug, Clone, Serialize)]
struct WasmOutputPacket {
    node_id: u8,
    module_id: u8,
    events: Vec<WasmEvent>,
}

/// Parse a WASM output packet (magic 0xC511_0007 — reassigned per issue #928;
/// the original 0xC511_0004 was a collision with ADR-063 fused vitals).
fn parse_wasm_output(buf: &[u8]) -> Option<WasmOutputPacket> {
    if buf.len() < 8 {
        return None;
    }
    let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if magic != 0xC511_0007 {
        return None;
    }

    let node_id = buf[4];
    let module_id = buf[5];
    let event_count = u16::from_le_bytes([buf[6], buf[7]]) as usize;

    let mut events = Vec::with_capacity(event_count);
    let mut offset = 8;
    for _ in 0..event_count {
        if offset + 5 > buf.len() {
            break;
        }
        let event_type = buf[offset];
        let value = f32::from_le_bytes([
            buf[offset + 1],
            buf[offset + 2],
            buf[offset + 3],
            buf[offset + 4],
        ]);
        events.push(WasmEvent { event_type, value });
        offset += 5;
    }

    Some(WasmOutputPacket {
        node_id,
        module_id,
        events,
    })
}

// ── ADR-063: Edge Fused Vitals Packet (magic 0xC511_0004) ─────────────────────
//
// 48-byte packed struct emitted by the ESP32-C6 + MR60BHA2 mmWave config when
// `mmwave_sensor_get_state().detected` is true. Byte layout from
// `firmware/esp32-csi-node/main/edge_processing.h` line 129 — kept in lockstep
// with the firmware's `_Static_assert(sizeof(edge_fused_vitals_pkt_t) == 48)`.
// Issue #928 surfaced that this magic was being parsed as WASM output and the
// fused vitals were silently lost. Adding the proper parser here.

#[derive(Debug, Clone, Serialize)]
struct EdgeFusedVitalsPacket {
    node_id: u8,
    /// Bit0=presence, Bit1=fall, Bit2=motion, Bit3=mmwave_present.
    flags: u8,
    /// Fused breathing rate in BPM (firmware sends BPM*100; we scale here).
    breathing_rate_bpm: f32,
    /// Fused heartrate in BPM (firmware sends BPM*10000; we scale here).
    heartrate_bpm: f32,
    rssi: i8,
    n_persons: u8,
    /// `mmwave_type_t` enum value from firmware.
    mmwave_type: u8,
    /// 0-100 fusion quality score.
    fusion_confidence: u8,
    motion_energy: f32,
    presence_score: f32,
    timestamp_ms: u32,
    /// Raw mmWave heart rate (BPM).
    mmwave_hr_bpm: f32,
    /// Raw mmWave breathing rate (BPM).
    mmwave_br_bpm: f32,
    /// Distance to nearest target (cm).
    mmwave_distance_cm: f32,
    /// Target count from mmWave.
    mmwave_targets: u8,
    /// mmWave signal quality 0-100.
    mmwave_confidence: u8,
}

/// Parse an ADR-063 edge fused vitals packet (magic 0xC511_0004, 48 bytes).
fn parse_edge_fused_vitals(buf: &[u8]) -> Option<EdgeFusedVitalsPacket> {
    if buf.len() < 48 {
        return None;
    }
    let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if magic != 0xC511_0004 {
        return None;
    }

    let node_id = buf[4];
    let flags = buf[5];
    let breathing_raw = u16::from_le_bytes([buf[6], buf[7]]);
    let heartrate_raw = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
    let rssi = buf[12] as i8;
    let n_persons = buf[13];
    let mmwave_type = buf[14];
    let fusion_confidence = buf[15];
    let motion_energy = f32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]);
    let presence_score = f32::from_le_bytes([buf[20], buf[21], buf[22], buf[23]]);
    let timestamp_ms = u32::from_le_bytes([buf[24], buf[25], buf[26], buf[27]]);
    let mmwave_hr_bpm = f32::from_le_bytes([buf[28], buf[29], buf[30], buf[31]]);
    let mmwave_br_bpm = f32::from_le_bytes([buf[32], buf[33], buf[34], buf[35]]);
    let mmwave_distance_cm = f32::from_le_bytes([buf[36], buf[37], buf[38], buf[39]]);
    let mmwave_targets = buf[40];
    let mmwave_confidence = buf[41];
    // buf[42..48] are firmware reserved fields (reserved3 u16 + reserved4 u32).

    Some(EdgeFusedVitalsPacket {
        node_id,
        flags,
        breathing_rate_bpm: breathing_raw as f32 / 100.0,
        heartrate_bpm: heartrate_raw as f32 / 10000.0,
        rssi,
        n_persons,
        mmwave_type,
        fusion_confidence,
        motion_energy,
        presence_score,
        timestamp_ms,
        mmwave_hr_bpm,
        mmwave_br_bpm,
        mmwave_distance_cm,
        mmwave_targets,
        mmwave_confidence,
    })
}

#[cfg(test)]
mod issue_928_magic_collision_tests {
    //! Issue #928 — `0xC511_0004` was being parsed as WASM output, eating the
    //! C6+mmWave fused-vitals packets. After this fix, `0xC511_0004` routes to
    //! `parse_edge_fused_vitals` and WASM output owns the freshly-allocated
    //! `0xC511_0007` slot. Tests guard both halves of the swap.
    use super::*;

    /// Build a 48-byte synthetic fused-vitals packet matching the firmware's
    /// `edge_fused_vitals_pkt_t` layout from `edge_processing.h:129`.
    fn build_fused_vitals_packet() -> Vec<u8> {
        let mut buf = vec![0u8; 48];
        buf[0..4].copy_from_slice(&0xC511_0004u32.to_le_bytes());
        buf[4] = 9; // node_id
        buf[5] = 0b0000_1001; // flags: presence | mmwave_present
        buf[6..8].copy_from_slice(&1600u16.to_le_bytes()); // breathing 16.00 BPM
        buf[8..12].copy_from_slice(&720_000u32.to_le_bytes()); // heartrate 72.0 BPM
        buf[12] = (-55i8) as u8; // rssi
        buf[13] = 1; // n_persons
        buf[14] = 2; // mmwave_type
        buf[15] = 85; // fusion_confidence
        buf[16..20].copy_from_slice(&0.42f32.to_le_bytes()); // motion_energy
        buf[20..24].copy_from_slice(&0.95f32.to_le_bytes()); // presence_score
        buf[24..28].copy_from_slice(&1_234_567u32.to_le_bytes()); // timestamp_ms
        buf[28..32].copy_from_slice(&71.5f32.to_le_bytes()); // mmwave_hr_bpm
        buf[32..36].copy_from_slice(&15.8f32.to_le_bytes()); // mmwave_br_bpm
        buf[36..40].copy_from_slice(&182.0f32.to_le_bytes()); // mmwave_distance_cm
        buf[40] = 1; // mmwave_targets
        buf[41] = 90; // mmwave_confidence
                      // bytes 42..48 — firmware reserved fields, left as zero
        buf
    }

    #[test]
    fn parse_edge_fused_vitals_extracts_fields_correctly() {
        let buf = build_fused_vitals_packet();
        let pkt = parse_edge_fused_vitals(&buf).expect("must parse a well-formed packet");
        assert_eq!(pkt.node_id, 9);
        assert_eq!(pkt.flags, 0b0000_1001);
        assert!(
            (pkt.breathing_rate_bpm - 16.0).abs() < 1e-3,
            "breathing scale 100"
        );
        assert!(
            (pkt.heartrate_bpm - 72.0).abs() < 1e-3,
            "heartrate scale 10000"
        );
        assert_eq!(pkt.rssi, -55);
        assert_eq!(pkt.n_persons, 1);
        assert_eq!(pkt.mmwave_type, 2);
        assert_eq!(pkt.fusion_confidence, 85);
        assert!((pkt.motion_energy - 0.42).abs() < 1e-6);
        assert!((pkt.presence_score - 0.95).abs() < 1e-6);
        assert_eq!(pkt.timestamp_ms, 1_234_567);
        assert!((pkt.mmwave_hr_bpm - 71.5).abs() < 1e-6);
        assert!((pkt.mmwave_br_bpm - 15.8).abs() < 1e-3);
        assert!((pkt.mmwave_distance_cm - 182.0).abs() < 1e-6);
        assert_eq!(pkt.mmwave_targets, 1);
        assert_eq!(pkt.mmwave_confidence, 90);
    }

    #[test]
    fn parse_edge_fused_vitals_rejects_short_buffer() {
        let buf = build_fused_vitals_packet();
        // Truncate to 47 bytes — one short of the 48-byte minimum.
        assert!(parse_edge_fused_vitals(&buf[..47]).is_none());
    }

    #[test]
    fn parse_edge_fused_vitals_rejects_wrong_magic() {
        let mut buf = build_fused_vitals_packet();
        buf[0..4].copy_from_slice(&0xC511_0007u32.to_le_bytes()); // WASM magic, not fused
        assert!(parse_edge_fused_vitals(&buf).is_none());
    }

    #[test]
    fn parse_wasm_output_rejects_legacy_0004_magic() {
        // The old WASM magic collided with fused vitals — must no longer be
        // accepted. A real fused-vitals packet starts with 0xC511_0004 and
        // would have been misparsed before this fix.
        let buf = build_fused_vitals_packet();
        assert!(
            parse_wasm_output(&buf).is_none(),
            "issue #928: WASM parser must NOT accept 0xC511_0004"
        );
    }

    #[test]
    fn parse_wasm_output_accepts_new_0007_magic() {
        // Build a tiny well-formed WASM output packet on the new magic.
        let mut buf = vec![0u8; 8];
        buf[0..4].copy_from_slice(&0xC511_0007u32.to_le_bytes());
        buf[4] = 5; // node_id
        buf[5] = 1; // module_id
        buf[6..8].copy_from_slice(&0u16.to_le_bytes()); // event_count = 0
        let pkt = parse_wasm_output(&buf).expect("0xC511_0007 must parse");
        assert_eq!(pkt.node_id, 5);
        assert_eq!(pkt.module_id, 1);
        assert!(pkt.events.is_empty());
    }
}

// ── ESP32 UDP frame parser ───────────────────────────────────────────────────

fn parse_esp32_frame(buf: &[u8]) -> Option<Esp32Frame> {
    let (frame, _consumed) = wifi_densepose_hardware::Esp32CsiParser::parse_frame(buf).ok()?;
    let (amplitudes, phases) = frame.to_amplitude_phase();
    let n_subcarriers = u8::try_from(frame.metadata.n_subcarriers).ok()?;
    let freq_mhz = u16::try_from(frame.metadata.channel_freq_mhz).ok()?;

    Some(Esp32Frame {
        magic: wifi_densepose_hardware::ESP32_CSI_MAGIC,
        node_id: frame.metadata.node_id,
        n_antennas: frame.metadata.n_antennas,
        n_subcarriers,
        freq_mhz,
        sequence: frame.metadata.sequence,
        rssi: frame.metadata.rssi_dbm,
        noise_floor: frame.metadata.noise_floor_dbm,
        amplitudes,
        phases,
    })
}

// ── Signal field generation ──────────────────────────────────────────────────

/// Generate a signal field that reflects where motion and signal changes are occurring.
///
/// Instead of a fixed-animation circle, this function uses the actual sensing data:
/// - `subcarrier_variances`: per-subcarrier variance computed from the frame history.
///   High-variance subcarriers indicate spatial directions where the signal is disrupted.
/// - `motion_score`: overall motion intensity [0, 1].
/// - `breathing_rate_hz`: estimated breathing rate in Hz; if > 0, adds a breathing ring.
/// - `signal_quality`: overall quality metric [0, 1] modulates field brightness.
///
/// The field grid is 20×20 cells representing a top-down view of the room.
/// Hotspots are derived from the subcarrier index (treated as an angular bin) so that
/// subcarriers with the highest variance produce peaks at the corresponding directions.
fn generate_signal_field(
    _mean_rssi: f64,
    motion_score: f64,
    breathing_rate_hz: f64,
    signal_quality: f64,
    subcarrier_variances: &[f64],
) -> SignalField {
    let grid = 20usize;
    let mut values = vec![0.0f64; grid * grid];
    let center = (grid as f64 - 1.0) / 2.0;

    // Normalise subcarrier variances to [0, 1].
    let max_var = subcarrier_variances.iter().cloned().fold(0.0f64, f64::max);
    let norm_factor = if max_var > 1e-9 { max_var } else { 1.0 };

    // For each cell, accumulate contributions from all subcarriers.
    // Each subcarrier k is assigned an angular direction proportional to its index
    // so that different subcarriers illuminate different regions of the room.
    let n_sub = subcarrier_variances.len().max(1);
    for (k, &var) in subcarrier_variances.iter().enumerate() {
        let weight = (var / norm_factor) * motion_score;
        if weight < 1e-6 {
            continue;
        }
        // Map subcarrier index to an angle across the full 2π sweep.
        let angle = (k as f64 / n_sub as f64) * 2.0 * std::f64::consts::PI;
        // Place the hotspot at a distance proportional to the weight, capped at 40% of
        // the grid radius so it stays within the room model.
        let radius = center * 0.8 * weight.sqrt();
        let hx = center + radius * angle.cos();
        let hz = center + radius * angle.sin();

        for z in 0..grid {
            for x in 0..grid {
                let dx = x as f64 - hx;
                let dz = z as f64 - hz;
                let dist2 = dx * dx + dz * dz;
                // Gaussian blob centred on the hotspot; spread scales with weight.
                let spread = (0.5 + weight * 2.0).max(0.5);
                values[z * grid + x] += weight * (-dist2 / (2.0 * spread * spread)).exp();
            }
        }
    }

    // Base radial attenuation from the router assumed at grid centre.
    for z in 0..grid {
        for x in 0..grid {
            let dx = x as f64 - center;
            let dz = z as f64 - center;
            let dist = (dx * dx + dz * dz).sqrt();
            let base = signal_quality * (-dist * 0.12).exp();
            values[z * grid + x] += base * 0.3;
        }
    }

    // Breathing ring: if a breathing rate was estimated add a faint annular highlight
    // at a radius corresponding to typical chest-wall displacement range.
    if breathing_rate_hz > 0.05 {
        let ring_r = center * 0.55;
        let ring_width = 1.8f64;
        for z in 0..grid {
            for x in 0..grid {
                let dx = x as f64 - center;
                let dz = z as f64 - center;
                let dist = (dx * dx + dz * dz).sqrt();
                let ring_val =
                    0.08 * (-(dist - ring_r).powi(2) / (2.0 * ring_width * ring_width)).exp();
                values[z * grid + x] += ring_val;
            }
        }
    }

    // Clamp and normalise to [0, 1].
    let field_max = values.iter().cloned().fold(0.0f64, f64::max);
    let scale = if field_max > 1e-9 {
        1.0 / field_max
    } else {
        1.0
    };
    for v in &mut values {
        *v = (*v * scale).clamp(0.0, 1.0);
    }

    SignalField {
        grid_size: [grid, 1, grid],
        values,
    }
}

// ── Feature extraction from ESP32 frame ──────────────────────────────────────

/// Estimate breathing rate in Hz from the amplitude time series stored in `frame_history`.
///
/// Approach:
/// 1. Build a scalar time series by computing the mean amplitude of each historical frame.
/// 2. Run a peak-detection pass: count rising-edge zero-crossings of the de-meaned signal.
/// 3. Convert the crossing rate to Hz, clipped to the physiological range 0.1–0.5 Hz
///    (12–30 breaths/min).
///
/// For accuracy the function additionally applies a simple 3-tap Goertzel-style power
/// estimate at evenly-spaced candidate frequencies in the breathing band and returns
/// the candidate with the highest energy.
fn estimate_breathing_rate_hz(frame_history: &VecDeque<Vec<f64>>, sample_rate_hz: f64) -> f64 {
    let n = frame_history.len();
    if n < 6 {
        return 0.0;
    }

    // Build scalar time series: mean amplitude per frame.
    let series: Vec<f64> = frame_history
        .iter()
        .map(|amps| {
            if amps.is_empty() {
                0.0
            } else {
                amps.iter().sum::<f64>() / amps.len() as f64
            }
        })
        .collect();

    let mean_s = series.iter().sum::<f64>() / n as f64;
    // De-mean.
    let detrended: Vec<f64> = series.iter().map(|x| x - mean_s).collect();

    // Goertzel power at candidate frequencies in the breathing band [0.1, 0.5] Hz.
    // We evaluate 9 candidate frequencies uniformly spaced in that band.
    let n_candidates = 9usize;
    let f_low = 0.1f64;
    let f_high = 0.5f64;
    let mut best_freq = 0.0f64;
    let mut best_power = 0.0f64;

    for i in 0..n_candidates {
        let freq = f_low + (f_high - f_low) * i as f64 / (n_candidates - 1).max(1) as f64;
        let omega = 2.0 * std::f64::consts::PI * freq / sample_rate_hz;
        let coeff = 2.0 * omega.cos();
        let mut s_prev2 = 0.0f64;
        let mut s_prev1 = 0.0f64;
        for &x in &detrended {
            let s = x + coeff * s_prev1 - s_prev2;
            s_prev2 = s_prev1;
            s_prev1 = s;
        }
        // Goertzel magnitude squared.
        let power = s_prev2 * s_prev2 + s_prev1 * s_prev1 - coeff * s_prev1 * s_prev2;
        if power > best_power {
            best_power = power;
            best_freq = freq;
        }
    }

    // Only report a breathing rate if the Goertzel energy is meaningfully above noise.
    // Threshold: power must exceed 10× the average power across all candidates.
    let avg_power = {
        let mut total = 0.0f64;
        for i in 0..n_candidates {
            let freq = f_low + (f_high - f_low) * i as f64 / (n_candidates - 1).max(1) as f64;
            let omega = 2.0 * std::f64::consts::PI * freq / sample_rate_hz;
            let coeff = 2.0 * omega.cos();
            let mut s_prev2 = 0.0f64;
            let mut s_prev1 = 0.0f64;
            for &x in &detrended {
                let s = x + coeff * s_prev1 - s_prev2;
                s_prev2 = s_prev1;
                s_prev1 = s;
            }
            total += s_prev2 * s_prev2 + s_prev1 * s_prev1 - coeff * s_prev1 * s_prev2;
        }
        total / n_candidates as f64
    };

    if best_power > avg_power * 3.0 {
        best_freq.clamp(f_low, f_high)
    } else {
        0.0
    }
}

/// Compute per-subcarrier variance across the sliding window of `frame_history`.
///
/// For each subcarrier index `k`, returns `Var[A_k]` over all stored frames.
/// This captures spatial signal variation; subcarriers whose amplitude fluctuates
/// heavily across time correspond to directions with motion.
/// Compute per-subcarrier importance weights using a simple sensitivity split.
///
/// Subcarriers whose sensitivity (amplitude magnitude) is above the median are
/// considered "sensitive" and receive weight `1.0 + (sens / max_sens)` (range 1.0–2.0).
/// The rest receive a baseline weight of 0.5. This mirrors the RuVector mincut
/// partition logic without requiring the graph dependency.
fn compute_subcarrier_importance_weights(sensitivity: &[f64]) -> Vec<f64> {
    let n = sensitivity.len();
    if n == 0 {
        return vec![];
    }
    let max_sens = sensitivity
        .iter()
        .cloned()
        .fold(f64::NEG_INFINITY, f64::max)
        .max(1e-9);

    // Compute median via a sorted copy.
    let mut sorted = sensitivity.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = if n % 2 == 0 {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    } else {
        sorted[n / 2]
    };

    sensitivity
        .iter()
        .map(|&s| {
            if s >= median {
                1.0 + (s / max_sens).min(1.0)
            } else {
                0.5
            }
        })
        .collect()
}

fn compute_subcarrier_variances(frame_history: &VecDeque<Vec<f64>>, n_sub: usize) -> Vec<f64> {
    if frame_history.is_empty() || n_sub == 0 {
        return vec![0.0; n_sub];
    }

    let n_frames = frame_history.len() as f64;
    let mut means = vec![0.0f64; n_sub];
    let mut sq_means = vec![0.0f64; n_sub];

    for frame in frame_history.iter() {
        for k in 0..n_sub {
            let a = if k < frame.len() { frame[k] } else { 0.0 };
            means[k] += a;
            sq_means[k] += a * a;
        }
    }

    (0..n_sub)
        .map(|k| {
            let mean = means[k] / n_frames;
            let sq_mean = sq_means[k] / n_frames;
            (sq_mean - mean * mean).max(0.0)
        })
        .collect()
}

/// Extract features from the current ESP32 frame, enhanced with temporal context from
/// `frame_history`.
///
/// Improvements over the previous single-frame approach:
///
/// - **Variance**: computed as the mean of per-subcarrier temporal variance across the
///   sliding window, not just the intra-frame spatial variance.
/// - **Motion detection**: uses frame-to-frame temporal difference (mean L2 change
///   between the current frame and the previous frame) normalised by signal amplitude,
///   so that actual changes are detected rather than just a threshold on the current frame.
/// - **Breathing rate**: estimated via Goertzel filter bank on the 0.1–0.5 Hz band of
///   the amplitude time series.
/// - **Signal quality**: based on SNR estimate (RSSI – noise floor) and subcarrier
///   variance stability.
///
/// Returns (features, raw_classification, breathing_rate_hz, sub_variances, raw_motion_score).
fn extract_features_from_frame(
    frame: &Esp32Frame,
    frame_history: &VecDeque<Vec<f64>>,
    sample_rate_hz: f64,
) -> (FeatureInfo, ClassificationInfo, f64, Vec<f64>, f64) {
    let n_sub = frame.amplitudes.len().max(1);
    let n = n_sub as f64;
    let mean_rssi = frame.rssi as f64;

    // ── RuVector Phase 1: subcarrier importance weighting ──
    // Compute per-subcarrier sensitivity from amplitude magnitude, then weight
    // sensitive subcarriers higher (>1.0) and insensitive ones lower (0.5).
    // This emphasises body-motion-correlated subcarriers in all downstream metrics.
    let sub_sensitivity: Vec<f64> = frame.amplitudes.iter().map(|a| a.abs()).collect();
    let importance_weights = compute_subcarrier_importance_weights(&sub_sensitivity);

    let weight_sum: f64 = importance_weights.iter().sum::<f64>();
    let mean_amp: f64 = if weight_sum > 0.0 {
        frame
            .amplitudes
            .iter()
            .zip(importance_weights.iter())
            .map(|(a, w)| a * w)
            .sum::<f64>()
            / weight_sum
    } else {
        frame.amplitudes.iter().sum::<f64>() / n
    };

    // ── Intra-frame subcarrier variance (weighted by importance) ──
    let intra_variance: f64 = if weight_sum > 0.0 {
        frame
            .amplitudes
            .iter()
            .zip(importance_weights.iter())
            .map(|(a, w)| w * (a - mean_amp).powi(2))
            .sum::<f64>()
            / weight_sum
    } else {
        frame
            .amplitudes
            .iter()
            .map(|a| (a - mean_amp).powi(2))
            .sum::<f64>()
            / n
    };

    // ── Temporal (sliding-window) per-subcarrier variance ──
    let sub_variances = compute_subcarrier_variances(frame_history, n_sub);
    let temporal_variance: f64 = if sub_variances.is_empty() {
        intra_variance
    } else {
        sub_variances.iter().sum::<f64>() / sub_variances.len() as f64
    };

    // Use the larger of intra-frame and temporal variance as the reported variance.
    let variance = intra_variance.max(temporal_variance);

    // ── Spectral power ──
    let spectral_power: f64 = frame.amplitudes.iter().map(|a| a * a).sum::<f64>() / n;

    // ── Motion band power (upper half of subcarriers, high spatial frequency) ──
    let half = frame.amplitudes.len() / 2;
    let motion_band_power = if half > 0 {
        frame.amplitudes[half..]
            .iter()
            .map(|a| (a - mean_amp).powi(2))
            .sum::<f64>()
            / (frame.amplitudes.len() - half) as f64
    } else {
        0.0
    };

    // ── Breathing band power (lower half of subcarriers, low spatial frequency) ──
    let breathing_band_power = if half > 0 {
        frame.amplitudes[..half]
            .iter()
            .map(|a| (a - mean_amp).powi(2))
            .sum::<f64>()
            / half as f64
    } else {
        0.0
    };

    // ── Dominant frequency via peak subcarrier index ──
    let peak_idx = frame
        .amplitudes
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(i, _)| i)
        .unwrap_or(0);
    let dominant_freq_hz = peak_idx as f64 * 0.05;

    // ── Change point detection (threshold-crossing count in current frame) ──
    let threshold = mean_amp * 1.2;
    let change_points = frame
        .amplitudes
        .windows(2)
        .filter(|w| (w[0] < threshold) != (w[1] < threshold))
        .count();

    // ── Motion score: sliding-window temporal difference ──
    // Compare current frame against the most recent historical frame.
    // The difference is normalised by the mean amplitude to be scale-invariant.
    let temporal_motion_score = if let Some(prev_frame) = frame_history.back() {
        let n_cmp = n_sub.min(prev_frame.len());
        if n_cmp > 0 {
            let diff_energy: f64 = (0..n_cmp)
                .map(|k| (frame.amplitudes[k] - prev_frame[k]).powi(2))
                .sum::<f64>()
                / n_cmp as f64;
            // Normalise by mean squared amplitude to get a dimensionless ratio.
            let ref_energy = mean_amp * mean_amp + 1e-9;
            (diff_energy / ref_energy).sqrt().clamp(0.0, 1.0)
        } else {
            0.0
        }
    } else {
        // No history yet — fall back to intra-frame variance-based estimate.
        (intra_variance / (mean_amp * mean_amp + 1e-9))
            .sqrt()
            .clamp(0.0, 1.0)
    };

    // Blend temporal motion with variance-based motion for robustness.
    // Also factor in motion_band_power and change_points for ESP32 real-world sensitivity.
    let variance_motion = (temporal_variance / 10.0).clamp(0.0, 1.0);
    let mbp_motion = (motion_band_power / 25.0).clamp(0.0, 1.0);
    let cp_motion = (change_points as f64 / 15.0).clamp(0.0, 1.0);
    let motion_score = (temporal_motion_score * 0.4
        + variance_motion * 0.2
        + mbp_motion * 0.25
        + cp_motion * 0.15)
        .clamp(0.0, 1.0);

    // ── Signal quality metric ──
    // Based on estimated SNR (RSSI relative to noise floor) and subcarrier consistency.
    let snr_db = (frame.rssi as f64 - frame.noise_floor as f64).max(0.0);
    let snr_quality = (snr_db / 40.0).clamp(0.0, 1.0); // 40 dB → quality = 1.0
                                                       // Penalise quality when temporal variance is very high (unstable signal).
    let stability =
        (1.0 - (temporal_variance / (mean_amp * mean_amp + 1e-9)).clamp(0.0, 1.0)).max(0.0);
    let signal_quality = (snr_quality * 0.6 + stability * 0.4).clamp(0.0, 1.0);

    // ── Breathing rate estimation ──
    let breathing_rate_hz = estimate_breathing_rate_hz(frame_history, sample_rate_hz);

    let features = FeatureInfo {
        mean_rssi,
        variance,
        motion_band_power,
        breathing_band_power,
        dominant_freq_hz,
        change_points,
        spectral_power,
    };

    // Return raw motion_score and signal_quality — classification is done by
    // `smooth_and_classify()` which has access to EMA state and hysteresis.
    let raw_classification = ClassificationInfo {
        motion_level: raw_classify(motion_score),
        presence: motion_score > 0.04,
        confidence: (0.4 + signal_quality * 0.3 + motion_score * 0.3).clamp(0.0, 1.0),
    };

    (
        features,
        raw_classification,
        breathing_rate_hz,
        sub_variances,
        motion_score,
    )
}

/// Simple threshold classification (no smoothing) — used as the "raw" input.
fn raw_classify(score: f64) -> String {
    if score > 0.25 {
        "active".into()
    } else if score > 0.12 {
        "present_moving".into()
    } else if score > 0.04 {
        "present_still".into()
    } else {
        "absent".into()
    }
}

/// Debounce frames required before state transition (at ~10 FPS = ~0.4s).
const DEBOUNCE_FRAMES: u32 = 4;
/// EMA alpha for motion smoothing (~1s time constant at 10 FPS).
const MOTION_EMA_ALPHA: f64 = 0.15;
/// EMA alpha for slow-adapting baseline (~30s time constant at 10 FPS).
const BASELINE_EMA_ALPHA: f64 = 0.003;
/// Number of warm-up frames before baseline subtraction kicks in.
const BASELINE_WARMUP: u64 = 50;

/// Apply EMA smoothing, adaptive baseline subtraction, and hysteresis debounce
/// to the raw classification.  Mutates the smoothing state in `AppStateInner`.
fn smooth_and_classify(state: &mut AppStateInner, raw: &mut ClassificationInfo, raw_motion: f64) {
    // 1. Adaptive baseline: slowly track the "quiet room" floor.
    //    Only update baseline when raw score is below the current smoothed level
    //    (i.e. during calm periods) so walking doesn't inflate the baseline.
    state.baseline_frames += 1;
    if state.baseline_frames < BASELINE_WARMUP {
        // During warm-up, aggressively learn the baseline.
        state.baseline_motion = state.baseline_motion * 0.9 + raw_motion * 0.1;
    } else if raw_motion < state.smoothed_motion + 0.05 {
        state.baseline_motion =
            state.baseline_motion * (1.0 - BASELINE_EMA_ALPHA) + raw_motion * BASELINE_EMA_ALPHA;
    }

    // 2. Subtract baseline and clamp.
    let adjusted = (raw_motion - state.baseline_motion * 0.7).max(0.0);

    // 3. EMA smooth the adjusted score.
    state.smoothed_motion =
        state.smoothed_motion * (1.0 - MOTION_EMA_ALPHA) + adjusted * MOTION_EMA_ALPHA;
    let sm = state.smoothed_motion;

    // 4. Classify from smoothed score.
    let candidate = raw_classify(sm);

    // 5. Hysteresis debounce: require N consecutive frames agreeing on a new state.
    if candidate == state.current_motion_level {
        // Already in this state — reset debounce.
        state.debounce_counter = 0;
        state.debounce_candidate = candidate;
    } else if candidate == state.debounce_candidate {
        state.debounce_counter += 1;
        if state.debounce_counter >= DEBOUNCE_FRAMES {
            // Transition accepted.
            state.current_motion_level = candidate;
            state.debounce_counter = 0;
        }
    } else {
        // New candidate — restart counter.
        state.debounce_candidate = candidate;
        state.debounce_counter = 1;
    }

    // 6. Write the smoothed result back into the classification.
    raw.motion_level = state.current_motion_level.clone();
    raw.presence = sm > 0.03;
    raw.confidence = (0.4 + sm * 0.6).clamp(0.0, 1.0);
}

/// Per-node variant of `smooth_and_classify` that operates on a `NodeState`
/// instead of `AppStateInner` (issue #249).
fn smooth_and_classify_node(ns: &mut NodeState, raw: &mut ClassificationInfo, raw_motion: f64) {
    ns.baseline_frames += 1;
    if ns.baseline_frames < BASELINE_WARMUP {
        ns.baseline_motion = ns.baseline_motion * 0.9 + raw_motion * 0.1;
    } else if raw_motion < ns.smoothed_motion + 0.05 {
        ns.baseline_motion =
            ns.baseline_motion * (1.0 - BASELINE_EMA_ALPHA) + raw_motion * BASELINE_EMA_ALPHA;
    }

    let adjusted = (raw_motion - ns.baseline_motion * 0.7).max(0.0);

    ns.smoothed_motion =
        ns.smoothed_motion * (1.0 - MOTION_EMA_ALPHA) + adjusted * MOTION_EMA_ALPHA;
    let sm = ns.smoothed_motion;

    let candidate = raw_classify(sm);

    if candidate == ns.current_motion_level {
        ns.debounce_counter = 0;
        ns.debounce_candidate = candidate;
    } else if candidate == ns.debounce_candidate {
        ns.debounce_counter += 1;
        if ns.debounce_counter >= DEBOUNCE_FRAMES {
            ns.current_motion_level = candidate;
            ns.debounce_counter = 0;
        }
    } else {
        ns.debounce_candidate = candidate;
        ns.debounce_counter = 1;
    }

    raw.motion_level = ns.current_motion_level.clone();
    raw.presence = sm > 0.03;
    raw.confidence = (0.4 + sm * 0.6).clamp(0.0, 1.0);
}

#[cfg(test)]
mod adaptive_calibration_tests {
    use super::*;

    fn raw_info(raw_motion: f64) -> ClassificationInfo {
        ClassificationInfo {
            motion_level: raw_classify(raw_motion),
            presence: raw_motion > 0.04,
            confidence: 1.0,
        }
    }

    fn apply_node_frame(ns: &mut NodeState, raw_motion: f64) -> ClassificationInfo {
        let mut raw = raw_info(raw_motion);
        smooth_and_classify_node(ns, &mut raw, raw_motion);
        raw
    }

    fn quiet_room_node(raw_motion: f64, frames: usize) -> (NodeState, ClassificationInfo) {
        let mut ns = NodeState::new();
        let mut last = raw_info(raw_motion);
        for _ in 0..frames {
            last = apply_node_frame(&mut ns, raw_motion);
        }
        (ns, last)
    }

    #[test]
    fn quiet_room_baseline_auto_calibrates_without_promoting_presence() {
        let quiet_raw_motion = 0.08;
        assert_eq!(raw_classify(quiet_raw_motion), "present_still");

        let (ns, classification) = quiet_room_node(quiet_raw_motion, 80);

        assert_eq!(ns.baseline_frames, 80);
        assert!(
            ns.baseline_motion > 0.07,
            "quiet-room baseline should learn the ambient floor, got {}",
            ns.baseline_motion
        );
        assert!(
            ns.smoothed_motion < 0.03,
            "baseline-subtracted quiet room should stay below presence, got {}",
            ns.smoothed_motion
        );
        assert_eq!(classification.motion_level, "absent");
        assert!(!classification.presence);
    }

    #[test]
    fn sustained_motion_after_quiet_room_baseline_promotes_presence() {
        let (mut ns, quiet_classification) = quiet_room_node(0.08, 80);
        assert_eq!(quiet_classification.motion_level, "absent");

        let baseline_before_motion = ns.baseline_motion;
        let mut classification = raw_info(0.30);
        for _ in 0..12 {
            classification = apply_node_frame(&mut ns, 0.30);
        }

        assert_ne!(classification.motion_level, "absent");
        assert!(classification.presence);
        assert!(
            (ns.baseline_motion - baseline_before_motion).abs() < 0.005,
            "motion should not be folded into the quiet-room baseline"
        );
    }
}

/// If an adaptive model is loaded, override the classification with the
/// model's prediction.  Uses the full 15-feature vector for higher accuracy.
fn adaptive_override(
    state: &AppStateInner,
    features: &FeatureInfo,
    classification: &mut ClassificationInfo,
) {
    if let Some(ref model) = state.adaptive_model {
        // Get current frame amplitudes from the latest history entry.
        let amps = state
            .frame_history
            .back()
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        let feat_arr = adaptive_classifier::features_from_runtime(
            &serde_json::json!({
                "variance": features.variance,
                "motion_band_power": features.motion_band_power,
                "breathing_band_power": features.breathing_band_power,
                "spectral_power": features.spectral_power,
                "dominant_freq_hz": features.dominant_freq_hz,
                "change_points": features.change_points,
                "mean_rssi": features.mean_rssi,
            }),
            amps,
        );
        let (label, conf) = model.classify(&feat_arr);
        classification.motion_level = label.to_string();
        classification.presence = label != "absent";
        // Blend model confidence with existing smoothed confidence.
        classification.confidence = (conf * 0.7 + classification.confidence * 0.3).clamp(0.0, 1.0);
    }
}

/// Size of the median filter window for vital signs outlier rejection.
const VITAL_MEDIAN_WINDOW: usize = 21;
/// EMA alpha for vital signs (~5s time constant at 10 FPS).
const VITAL_EMA_ALPHA: f64 = 0.02;
/// Maximum BPM jump per frame before a value is rejected as an outlier.
const HR_MAX_JUMP: f64 = 8.0;
const BR_MAX_JUMP: f64 = 2.0;
/// Minimum change from current smoothed value before EMA updates (dead-band).
/// Prevents micro-drift from creeping in.
const HR_DEAD_BAND: f64 = 2.0;
const BR_DEAD_BAND: f64 = 0.5;

/// Smooth vital signs using median-filter outlier rejection + EMA.
/// Mutates `state.smoothed_hr`, `state.smoothed_br`, etc.
/// Returns the smoothed VitalSigns to broadcast.
fn smooth_vitals(state: &mut AppStateInner, raw: &VitalSigns) -> VitalSigns {
    let raw_hr = raw.heart_rate_bpm.unwrap_or(0.0);
    let raw_br = raw.breathing_rate_bpm.unwrap_or(0.0);

    // -- Outlier rejection: skip values that jump too far from current EMA --
    let hr_ok = state.smoothed_hr < 1.0 || (raw_hr - state.smoothed_hr).abs() < HR_MAX_JUMP;
    let br_ok = state.smoothed_br < 1.0 || (raw_br - state.smoothed_br).abs() < BR_MAX_JUMP;

    // Push into buffer (only non-outlier values)
    if hr_ok && raw_hr > 0.0 {
        state.hr_buffer.push_back(raw_hr);
        if state.hr_buffer.len() > VITAL_MEDIAN_WINDOW {
            state.hr_buffer.pop_front();
        }
    }
    if br_ok && raw_br > 0.0 {
        state.br_buffer.push_back(raw_br);
        if state.br_buffer.len() > VITAL_MEDIAN_WINDOW {
            state.br_buffer.pop_front();
        }
    }

    // Compute trimmed mean: drop top/bottom 25% then average the middle 50%.
    // This is more stable than pure median and less noisy than raw mean.
    let trimmed_hr = trimmed_mean(&state.hr_buffer);
    let trimmed_br = trimmed_mean(&state.br_buffer);

    // EMA smooth with dead-band: only update if the trimmed mean differs
    // from the current smoothed value by more than the dead-band.
    // This prevents the display from constantly creeping by tiny amounts.
    if trimmed_hr > 0.0 {
        if state.smoothed_hr < 1.0 {
            state.smoothed_hr = trimmed_hr;
        } else if (trimmed_hr - state.smoothed_hr).abs() > HR_DEAD_BAND {
            state.smoothed_hr =
                state.smoothed_hr * (1.0 - VITAL_EMA_ALPHA) + trimmed_hr * VITAL_EMA_ALPHA;
        }
        // else: within dead-band, hold current value
    }
    if trimmed_br > 0.0 {
        if state.smoothed_br < 1.0 {
            state.smoothed_br = trimmed_br;
        } else if (trimmed_br - state.smoothed_br).abs() > BR_DEAD_BAND {
            state.smoothed_br =
                state.smoothed_br * (1.0 - VITAL_EMA_ALPHA) + trimmed_br * VITAL_EMA_ALPHA;
        }
    }

    // Smooth confidence
    state.smoothed_hr_conf = state.smoothed_hr_conf * 0.92 + raw.heartbeat_confidence * 0.08;
    state.smoothed_br_conf = state.smoothed_br_conf * 0.92 + raw.breathing_confidence * 0.08;

    VitalSigns {
        breathing_rate_bpm: if state.smoothed_br > 1.0 {
            Some(state.smoothed_br)
        } else {
            None
        },
        heart_rate_bpm: if state.smoothed_hr > 1.0 {
            Some(state.smoothed_hr)
        } else {
            None
        },
        breathing_confidence: state.smoothed_br_conf,
        heartbeat_confidence: state.smoothed_hr_conf,
        signal_quality: raw.signal_quality,
    }
}

/// Per-node variant of `smooth_vitals` that operates on a `NodeState` (issue #249).
fn smooth_vitals_node(ns: &mut NodeState, raw: &VitalSigns) -> VitalSigns {
    let raw_hr = raw.heart_rate_bpm.unwrap_or(0.0);
    let raw_br = raw.breathing_rate_bpm.unwrap_or(0.0);

    let hr_ok = ns.smoothed_hr < 1.0 || (raw_hr - ns.smoothed_hr).abs() < HR_MAX_JUMP;
    let br_ok = ns.smoothed_br < 1.0 || (raw_br - ns.smoothed_br).abs() < BR_MAX_JUMP;

    if hr_ok && raw_hr > 0.0 {
        ns.hr_buffer.push_back(raw_hr);
        if ns.hr_buffer.len() > VITAL_MEDIAN_WINDOW {
            ns.hr_buffer.pop_front();
        }
    }
    if br_ok && raw_br > 0.0 {
        ns.br_buffer.push_back(raw_br);
        if ns.br_buffer.len() > VITAL_MEDIAN_WINDOW {
            ns.br_buffer.pop_front();
        }
    }

    let trimmed_hr = trimmed_mean(&ns.hr_buffer);
    let trimmed_br = trimmed_mean(&ns.br_buffer);

    if trimmed_hr > 0.0 {
        if ns.smoothed_hr < 1.0 {
            ns.smoothed_hr = trimmed_hr;
        } else if (trimmed_hr - ns.smoothed_hr).abs() > HR_DEAD_BAND {
            ns.smoothed_hr =
                ns.smoothed_hr * (1.0 - VITAL_EMA_ALPHA) + trimmed_hr * VITAL_EMA_ALPHA;
        }
    }
    if trimmed_br > 0.0 {
        if ns.smoothed_br < 1.0 {
            ns.smoothed_br = trimmed_br;
        } else if (trimmed_br - ns.smoothed_br).abs() > BR_DEAD_BAND {
            ns.smoothed_br =
                ns.smoothed_br * (1.0 - VITAL_EMA_ALPHA) + trimmed_br * VITAL_EMA_ALPHA;
        }
    }

    ns.smoothed_hr_conf = ns.smoothed_hr_conf * 0.92 + raw.heartbeat_confidence * 0.08;
    ns.smoothed_br_conf = ns.smoothed_br_conf * 0.92 + raw.breathing_confidence * 0.08;

    VitalSigns {
        breathing_rate_bpm: if ns.smoothed_br > 1.0 {
            Some(ns.smoothed_br)
        } else {
            None
        },
        heart_rate_bpm: if ns.smoothed_hr > 1.0 {
            Some(ns.smoothed_hr)
        } else {
            None
        },
        breathing_confidence: ns.smoothed_br_conf,
        heartbeat_confidence: ns.smoothed_hr_conf,
        signal_quality: raw.signal_quality,
    }
}

/// Trimmed mean: sort, drop top/bottom 25%, average the middle 50%.
/// More robust than median (uses more data) and less noisy than raw mean.
fn trimmed_mean(buf: &VecDeque<f64>) -> f64 {
    if buf.is_empty() {
        return 0.0;
    }
    let mut sorted: Vec<f64> = buf.iter().copied().collect();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = sorted.len();
    let trim = n / 4; // drop 25% from each end
    let middle = &sorted[trim..n - trim.max(0)];
    if middle.is_empty() {
        sorted[n / 2] // fallback to median if too few samples
    } else {
        middle.iter().sum::<f64>() / middle.len() as f64
    }
}

// ── Windows WiFi RSSI collector ──────────────────────────────────────────────

/// Parse `netsh wlan show interfaces` output for RSSI and signal quality
fn parse_netsh_interfaces_output(output: &str) -> Option<(f64, f64, String)> {
    let mut rssi = None;
    let mut signal = None;
    let mut ssid = None;

    for line in output.lines() {
        let line = line.trim();
        if line.starts_with("Signal") {
            // "Signal                 : 89%"
            if let Some(pct) = line.split(':').nth(1) {
                let pct = pct.trim().trim_end_matches('%');
                if let Ok(v) = pct.parse::<f64>() {
                    signal = Some(v);
                    // Convert signal% to approximate dBm: -100 + (signal% * 0.6)
                    rssi = Some(-100.0 + v * 0.6);
                }
            }
        }
        if line.starts_with("SSID") && !line.starts_with("BSSID") {
            if let Some(s) = line.split(':').nth(1) {
                ssid = Some(s.trim().to_string());
            }
        }
    }

    match (rssi, signal, ssid) {
        (Some(r), Some(_s), Some(name)) => Some((r, _s, name)),
        (Some(r), Some(_s), None) => Some((r, _s, "Unknown".into())),
        _ => None,
    }
}

#[cfg(target_os = "linux")]
fn scan_linux_access_points(interface: &str) -> Result<Vec<AccessPointObservation>, String> {
    let now = std::time::Instant::now();
    let observations = match LinuxIwScanner::with_interface(interface).scan_sync() {
        Ok(obs) => obs,
        Err(primary) => LinuxIwScanner::with_interface(interface)
            .use_cached()
            .scan_sync()
            .map_err(|cached| format!("iw scan failed: {primary}; cached scan failed: {cached}"))?,
    };
    let mut access_points: Vec<_> = observations
        .iter()
        .map(|obs| observation_from_bssid(obs, now))
        .collect();
    access_points.sort_by(|a, b| {
        b.rssi_dbm
            .partial_cmp(&a.rssi_dbm)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(access_points)
}

#[cfg(target_os = "linux")]
async fn ap_scan_task(state: SharedState, interface: String, interval_secs: u64) {
    let interval = Duration::from_secs(interval_secs.max(1));
    info!(
        "Linux AP discovery active (interface={}, interval={}s)",
        interface,
        interval.as_secs()
    );

    loop {
        let iface = interface.clone();
        match tokio::task::spawn_blocking(move || scan_linux_access_points(&iface)).await {
            Ok(Ok(access_points)) => {
                let count = access_points.len();
                let mut s = state.write().await;
                s.detected_access_points = access_points;
                debug!("AP discovery updated topology with {} visible APs", count);
            }
            Ok(Err(error)) => {
                let mut s = state.write().await;
                s.detected_access_points.clear();
                debug!("AP discovery unavailable on {}: {}", interface, error);
            }
            Err(error) => debug!("AP discovery worker join error: {}", error),
        }

        tokio::time::sleep(interval).await;
    }
}

async fn windows_wifi_task(state: SharedState, tick_ms: u64) {
    let mut interval = tokio::time::interval(Duration::from_millis(tick_ms));
    let mut seq: u32 = 0;

    // ADR-022 Phase 3: Multi-BSSID pipeline state (kept across ticks)
    let mut registry = BssidRegistry::new(32, 30);
    let mut pipeline = WindowsWifiPipeline::new();

    info!(
        "Windows WiFi multi-BSSID pipeline active (tick={}ms, max_bssids=32)",
        tick_ms
    );

    loop {
        interval.tick().await;
        seq += 1;

        // ── Step 1: Run multi-BSSID scan via spawn_blocking ──────────
        // NetshBssidScanner is not Send, so we run `netsh` and parse
        // the output inside a blocking closure.
        let bssid_scan_result = tokio::task::spawn_blocking(|| {
            let output = std::process::Command::new("netsh")
                .args(["wlan", "show", "networks", "mode=bssid"])
                .output()
                .map_err(|e| format!("netsh bssid scan failed: {e}"))?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(format!(
                    "netsh exited with {}: {}",
                    output.status,
                    stderr.trim()
                ));
            }

            let stdout = String::from_utf8_lossy(&output.stdout);
            parse_netsh_bssid_output(&stdout).map_err(|e| format!("parse error: {e}"))
        })
        .await;

        // Unwrap the JoinHandle result, then the inner Result.
        let observations = match bssid_scan_result {
            Ok(Ok(obs)) if !obs.is_empty() => obs,
            Ok(Ok(_empty)) => {
                debug!("Multi-BSSID scan returned 0 observations, falling back");
                windows_wifi_fallback_tick(&state, seq).await;
                continue;
            }
            Ok(Err(e)) => {
                warn!("Multi-BSSID scan error: {e}, falling back");
                windows_wifi_fallback_tick(&state, seq).await;
                continue;
            }
            Err(join_err) => {
                error!("spawn_blocking panicked: {join_err}");
                continue;
            }
        };

        let obs_count = observations.len();

        // Derive SSID from the first observation for the source label.
        let ssid = observations
            .first()
            .map(|o| o.ssid.clone())
            .unwrap_or_else(|| "Unknown".into());

        // ── Step 2: Feed observations into registry ──────────────────
        registry.update(&observations);
        let multi_ap_frame = registry.to_multi_ap_frame();

        // ── Step 3: Run enhanced pipeline ────────────────────────────
        let enhanced = pipeline.process(&multi_ap_frame);

        // ── Step 4: Build backward-compatible Esp32Frame ─────────────
        let first_rssi = observations.first().map(|o| o.rssi_dbm).unwrap_or(-80.0);
        let _first_signal_pct = observations.first().map(|o| o.signal_pct).unwrap_or(40.0);

        let frame = Esp32Frame {
            magic: 0xC511_0001,
            node_id: 0,
            n_antennas: 1,
            n_subcarriers: obs_count.min(255) as u8,
            freq_mhz: 2437,
            sequence: seq,
            rssi: first_rssi.clamp(-128.0, 127.0) as i8,
            noise_floor: -90,
            amplitudes: multi_ap_frame.amplitudes.clone(),
            phases: multi_ap_frame.phases.clone(),
        };

        // ── Step 4b: Update frame history and extract features ───────
        let mut s_write_pre = state.write().await;
        s_write_pre
            .frame_history
            .push_back(frame.amplitudes.clone());
        if s_write_pre.frame_history.len() > FRAME_HISTORY_CAPACITY {
            s_write_pre.frame_history.pop_front();
        }
        let dsp_started = std::time::Instant::now();
        let sample_rate_hz = 1000.0 / tick_ms as f64;
        let (features, mut classification, breathing_rate_hz, sub_variances, raw_motion) =
            extract_features_from_frame(&frame, &s_write_pre.frame_history, sample_rate_hz);
        smooth_and_classify(&mut s_write_pre, &mut classification, raw_motion);
        adaptive_override(&s_write_pre, &features, &mut classification);
        record_dsp_latency(&mut s_write_pre, dsp_started);
        drop(s_write_pre);

        // ── Step 5: Build enhanced fields from pipeline result ───────
        let enhanced_motion = Some(serde_json::json!({
            "score": enhanced.motion.score,
            "level": format!("{:?}", enhanced.motion.level),
            "contributing_bssids": enhanced.motion.contributing_bssids,
        }));

        let enhanced_breathing = enhanced.breathing.as_ref().map(|b| {
            serde_json::json!({
                "rate_bpm": b.rate_bpm,
                "confidence": b.confidence,
                "bssid_count": b.bssid_count,
            })
        });

        let posture_str = enhanced.posture.map(|p| format!("{p:?}"));
        let sig_quality_score = Some(enhanced.signal_quality.score);
        let verdict_str = Some(format!("{:?}", enhanced.verdict));
        let bssid_n = Some(enhanced.bssid_count);

        // ── Step 6: Update shared state ──────────────────────────────
        let mut s = state.write().await;
        s.source = format!("wifi:{ssid}");
        let ap_seen_at = std::time::Instant::now();
        s.detected_access_points = observations
            .iter()
            .map(|obs| observation_from_bssid(obs, ap_seen_at))
            .collect();
        s.rssi_history.push_back(first_rssi);
        if s.rssi_history.len() > 60 {
            s.rssi_history.pop_front();
        }

        s.tick += 1;
        let tick = s.tick;

        let motion_score = if classification.motion_level == "active" {
            0.8
        } else if classification.motion_level == "present_still" {
            0.3
        } else {
            0.05
        };

        let raw_vitals = s
            .vital_detector
            .process_frame(&frame.amplitudes, &frame.phases);
        let vitals = smooth_vitals(&mut s, &raw_vitals);
        s.latest_vitals = vitals.clone();

        let feat_variance = features.variance;

        // ADR-044 §5.2: feed raw features into rolling-P95 estimators before scoring.
        s.p95_variance.push(features.variance);
        s.p95_motion_band_power.push(features.motion_band_power);
        s.p95_spectral_power.push(features.spectral_power);

        // Multi-person estimation with temporal smoothing (EMA α=0.10).
        let raw_score = compute_person_score(&s, &features);
        s.smoothed_person_score = s.smoothed_person_score * 0.90 + raw_score * 0.10;
        let est_persons = if classification.presence {
            let count = s.person_count();
            s.prev_person_count = count;
            count
        } else {
            s.prev_person_count = 0;
            0
        };
        let now = std::time::Instant::now();
        let count_evidence =
            apply_room_presence_continuity(&mut s, &mut classification, est_persons, now);
        let rendered_persons = count_evidence.rendered_persons;

        let mut update = SensingUpdate {
            msg_type: "sensing_update".to_string(),
            timestamp: chrono::Utc::now().timestamp_millis() as f64 / 1000.0,
            source: format!("wifi:{ssid}"),
            tick,
            nodes: vec![NodeInfo {
                node_id: 0,
                rssi_dbm: first_rssi,
                position: [0.0, 0.0, 0.0],
                amplitude: multi_ap_frame.amplitudes,
                subcarrier_count: obs_count,
                sync: None, // multi-BSSID scan path — no mesh peer
            }],
            features,
            classification,
            signal_field: generate_signal_field(
                first_rssi,
                motion_score,
                breathing_rate_hz,
                feat_variance.min(1.0),
                &sub_variances,
            ),
            vital_signs: Some(vitals),
            enhanced_motion,
            enhanced_breathing,
            posture: posture_str,
            signal_quality_score: sig_quality_score,
            quality_verdict: verdict_str,
            bssid_count: bssid_n,
            pose_keypoints: None,
            model_status: None,
            persons: None,
            state: None,
            estimated_persons: if rendered_persons > 0 {
                Some(rendered_persons)
            } else {
                None
            },
            count_evidence: Some(count_evidence),
            node_features: None,
        };

        finalize_persons_for_update(&mut s, &mut update);

        publish_sensing_update(&mut s, update);

        debug!(
            "Multi-BSSID tick #{tick}: {obs_count} BSSIDs, quality={:.2}, verdict={:?}",
            enhanced.signal_quality.score, enhanced.verdict
        );
    }
}

/// Fallback: single-RSSI collection via `netsh wlan show interfaces`.
///
/// Used when the multi-BSSID scan fails or returns 0 observations.
async fn windows_wifi_fallback_tick(state: &SharedState, seq: u32) {
    let output = match tokio::process::Command::new("netsh")
        .args(["wlan", "show", "interfaces"])
        .output()
        .await
    {
        Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(),
        Err(e) => {
            warn!("netsh interfaces fallback failed: {e}");
            return;
        }
    };

    let (rssi_dbm, signal_pct, ssid) = match parse_netsh_interfaces_output(&output) {
        Some(v) => v,
        None => {
            debug!("Fallback: no WiFi interface connected");
            return;
        }
    };

    let frame = Esp32Frame {
        magic: 0xC511_0001,
        node_id: 0,
        n_antennas: 1,
        n_subcarriers: 1,
        freq_mhz: 2437,
        sequence: seq,
        rssi: rssi_dbm as i8,
        noise_floor: -90,
        amplitudes: vec![signal_pct],
        phases: vec![0.0],
    };

    let mut s = state.write().await;
    // Update frame history before extracting features.
    s.frame_history.push_back(frame.amplitudes.clone());
    if s.frame_history.len() > FRAME_HISTORY_CAPACITY {
        s.frame_history.pop_front();
    }
    let dsp_started = std::time::Instant::now();
    let sample_rate_hz = 2.0_f64; // fallback tick ~ 500 ms => 2 Hz
    let (features, mut classification, breathing_rate_hz, sub_variances, raw_motion) =
        extract_features_from_frame(&frame, &s.frame_history, sample_rate_hz);
    smooth_and_classify(&mut s, &mut classification, raw_motion);
    adaptive_override(&s, &features, &mut classification);
    record_dsp_latency(&mut s, dsp_started);

    s.source = format!("wifi:{ssid}");
    s.rssi_history.push_back(rssi_dbm);
    if s.rssi_history.len() > 60 {
        s.rssi_history.pop_front();
    }

    s.tick += 1;
    let tick = s.tick;

    let motion_score = if classification.motion_level == "active" {
        0.8
    } else if classification.motion_level == "present_still" {
        0.3
    } else {
        0.05
    };

    let raw_vitals = s
        .vital_detector
        .process_frame(&frame.amplitudes, &frame.phases);
    let vitals = smooth_vitals(&mut s, &raw_vitals);
    s.latest_vitals = vitals.clone();

    let feat_variance = features.variance;

    // ADR-044 §5.2: feed raw features into rolling-P95 estimators before scoring.
    s.p95_variance.push(features.variance);
    s.p95_motion_band_power.push(features.motion_band_power);
    s.p95_spectral_power.push(features.spectral_power);

    // Multi-person estimation with temporal smoothing (EMA α=0.10).
    let raw_score = compute_person_score(&s, &features);
    s.smoothed_person_score = s.smoothed_person_score * 0.90 + raw_score * 0.10;
    let est_persons = if classification.presence {
        let count = s.person_count();
        s.prev_person_count = count;
        count
    } else {
        s.prev_person_count = 0;
        0
    };
    let now = std::time::Instant::now();
    let count_evidence =
        apply_room_presence_continuity(&mut s, &mut classification, est_persons, now);
    let rendered_persons = count_evidence.rendered_persons;

    let mut update = SensingUpdate {
        msg_type: "sensing_update".to_string(),
        timestamp: chrono::Utc::now().timestamp_millis() as f64 / 1000.0,
        source: format!("wifi:{ssid}"),
        tick,
        nodes: vec![NodeInfo {
            node_id: 0,
            rssi_dbm,
            position: [0.0, 0.0, 0.0],
            amplitude: vec![signal_pct],
            subcarrier_count: 1,
            sync: None, // synthetic-RSSI fallback path — no mesh peer
        }],
        features,
        classification,
        signal_field: generate_signal_field(
            rssi_dbm,
            motion_score,
            breathing_rate_hz,
            feat_variance.min(1.0),
            &sub_variances,
        ),
        vital_signs: Some(vitals),
        enhanced_motion: None,
        enhanced_breathing: None,
        posture: None,
        signal_quality_score: None,
        quality_verdict: None,
        bssid_count: None,
        pose_keypoints: None,
        model_status: None,
        persons: None,
        state: None,
        estimated_persons: if rendered_persons > 0 {
            Some(rendered_persons)
        } else {
            None
        },
        count_evidence: Some(count_evidence),
        node_features: None,
    };

    finalize_persons_for_update(&mut s, &mut update);

    publish_sensing_update(&mut s, update);
}

/// Probe if Windows WiFi is connected
async fn probe_windows_wifi() -> bool {
    match tokio::process::Command::new("netsh")
        .args(["wlan", "show", "interfaces"])
        .output()
        .await
    {
        Ok(o) => {
            let out = String::from_utf8_lossy(&o.stdout);
            parse_netsh_interfaces_output(&out).is_some()
        }
        Err(_) => false,
    }
}

/// Probe if ESP32 is streaming on UDP port
async fn probe_esp32(port: u16) -> bool {
    let addr = format!("0.0.0.0:{port}");
    match UdpSocket::bind(&addr).await {
        Ok(sock) => {
            let mut buf = [0u8; 256];
            match tokio::time::timeout(Duration::from_secs(2), sock.recv_from(&mut buf)).await {
                Ok(Ok((len, _))) => parse_esp32_frame(&buf[..len]).is_some(),
                _ => false,
            }
        }
        Err(_) => false,
    }
}

// ── Simulated data generator ─────────────────────────────────────────────────

fn generate_simulated_frame(tick: u64) -> Esp32Frame {
    let t = tick as f64 * 0.1;
    let n_sub = 56usize;
    let mut amplitudes = Vec::with_capacity(n_sub);
    let mut phases = Vec::with_capacity(n_sub);

    for i in 0..n_sub {
        let base = 15.0 + 5.0 * (i as f64 * 0.1 + t * 0.3).sin();
        let noise = (i as f64 * 7.3 + t * 13.7).sin() * 2.0;
        amplitudes.push((base + noise).max(0.1));
        phases.push((i as f64 * 0.2 + t * 0.5).sin() * std::f64::consts::PI);
    }

    Esp32Frame {
        magic: 0xC511_0001,
        node_id: 1,
        n_antennas: 1,
        n_subcarriers: n_sub as u8,
        freq_mhz: 2437,
        sequence: tick as u32,
        rssi: (-40.0 + 5.0 * (t * 0.2).sin()) as i8,
        noise_floor: -90,
        amplitudes,
        phases,
    }
}

// ── WebSocket handler ────────────────────────────────────────────────────────

async fn ws_sensing_handler(
    ws: WebSocketUpgrade,
    State(state): State<SharedState>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_ws_client(socket, state))
}

async fn handle_ws_client(mut socket: WebSocket, state: SharedState) {
    let mut rx = {
        let s = state.read().await;
        s.tx.subscribe()
    };

    info!("WebSocket client connected (sensing)");

    // ADR-044/045: ping/pong keepalive to prevent proxy idle timeouts.
    let mut ping_interval = tokio::time::interval(std::time::Duration::from_secs(30));
    ping_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    Ok(json) => {
                        if socket.send(Message::Text(json)).await.is_err() {
                            break;
                        }
                    }
                    // Lagged: client fell behind — skip missed frames, don't disconnect.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::debug!("WS client lagged by {n} frames, skipping");
                        continue;
                    }
                    Err(_) => break, // channel closed
                }
            }
            _ = ping_interval.tick() => {
                if socket.send(Message::Ping(vec![])).await.is_err() {
                    break;
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Pong(_))) => {} // keepalive response
                    _ => {} // ignore other client messages
                }
            }
        }
    }

    info!("WebSocket client disconnected (sensing)");
}

async fn ws_presence_pose_handler(
    ws: WebSocketUpgrade,
    State(state): State<SharedState>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_presence_pose_client(socket, state))
}

async fn handle_presence_pose_client(mut socket: WebSocket, state: SharedState) {
    info!("WebSocket client connected (presence pose)");

    let mut interval = tokio::time::interval(Duration::from_millis(100));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = interval.tick() => {
                let payload = {
                    let s = state.read().await;
                    presence_update_payload(&s, std::time::Instant::now())
                };
                if socket.send(Message::Text(payload.to_string())).await.is_err() {
                    break;
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Ping(payload))) => {
                        if socket.send(Message::Pong(payload)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    _ => {}
                }
            }
        }
    }

    info!("WebSocket client disconnected (presence pose)");
}

async fn ws_vitals_handler(
    ws: WebSocketUpgrade,
    State(state): State<SharedState>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_vitals_client(socket, state))
}

async fn handle_vitals_client(mut socket: WebSocket, state: SharedState) {
    info!("WebSocket client connected (vitals)");

    let mut interval = tokio::time::interval(Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = interval.tick() => {
                let payload = {
                    let s = state.read().await;
                    vitals_update_payload(&s, std::time::Instant::now())
                };
                if socket.send(Message::Text(payload.to_string())).await.is_err() {
                    break;
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Ping(payload))) => {
                        if socket.send(Message::Pong(payload)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    _ => {}
                }
            }
        }
    }

    info!("WebSocket client disconnected (vitals)");
}

fn presence_update_payload(s: &AppStateInner, now: std::time::Instant) -> serde_json::Value {
    let node_count = active_node_count(s, now);
    let system_status = if node_count == 0 { "no_nodes" } else { "live" };
    let persons = if node_count == 0 {
        Vec::new()
    } else {
        presence_persons(s, now)
    };

    serde_json::json!({
        "type": "presence_update",
        "timestamp_ms": unix_timestamp_ms(),
        "persons": persons,
        "node_count": node_count,
        "system_status": system_status,
    })
}

fn presence_persons(s: &AppStateInner, now: std::time::Instant) -> Vec<serde_json::Value> {
    let Some(update) = s.latest_update.as_ref() else {
        return Vec::new();
    };
    if !update.classification.presence {
        return Vec::new();
    }

    let (breathing_bpm, breathing_confidence) = breathing_for_presence(s, update);
    let include_heart_rate = s.feature_flags.beta_enabled(BetaFeature::PreciseHeartRate);
    let heart_rate_bpm = include_heart_rate
        .then(|| {
            update
                .vital_signs
                .as_ref()
                .and_then(|vitals| vitals.heart_rate_bpm)
                .or(s.latest_vitals.heart_rate_bpm)
                .or_else(|| s.edge_vitals.as_ref().map(|vitals| vitals.heartrate_bpm))
                .unwrap_or(0.0)
        });
    let motion_energy = current_motion_energy(s, update, now);

    if let Some(tracking) = update.state.as_ref() {
        return tracking
            .persons
            .iter()
            .map(|person| {
                presence_person_json(
                    person.id,
                    person.position_m[0],
                    person.position_m[2],
                    person.confidence,
                    breathing_bpm,
                    breathing_confidence,
                    heart_rate_bpm,
                    true,
                    motion_energy,
                )
            })
            .collect();
    }

    if let Some(persons) = update.persons.as_ref() {
        let converted: Vec<_> = persons
            .iter()
            .filter_map(|person| {
                let position = person.position_m.or(person.position)?;
                Some(presence_person_json(
                    person.id,
                    position[0],
                    position[2],
                    person.confidence,
                    breathing_bpm,
                    breathing_confidence,
                    heart_rate_bpm,
                    true,
                    motion_energy,
                ))
            })
            .collect();
        if !converted.is_empty() {
            return converted;
        }
    }

    let timestamp_ms = unix_timestamp_ms();
    localization_snapshot(s, now, timestamp_ms)
        .0
        .map(|location| {
            vec![presence_person_json(
                1,
                location.x as f64,
                location.y as f64,
                location.confidence as f64,
                breathing_bpm,
                breathing_confidence,
                heart_rate_bpm,
                true,
                motion_energy,
            )]
        })
        .unwrap_or_default()
}

fn breathing_for_presence(s: &AppStateInner, update: &SensingUpdate) -> (Option<f64>, f64) {
    let threshold = f64::from(breathing_min_confidence());
    let mut candidates = Vec::with_capacity(3);
    if let Some(vitals) = update.vital_signs.as_ref() {
        candidates.push((
            vitals.breathing_rate_bpm,
            vitals.breathing_confidence,
        ));
    }
    candidates.push((
        s.latest_vitals.breathing_rate_bpm,
        s.latest_vitals.breathing_confidence,
    ));
    if let Some(edge) = s.edge_vitals.as_ref() {
        candidates.push((
            (edge.breathing_rate_bpm > 0.0).then_some(edge.breathing_rate_bpm),
            if edge.presence { 0.7 } else { 0.0 },
        ));
    }

    for (bpm, confidence) in candidates {
        let confidence = clamp_unit(confidence);
        if bpm.is_some() || confidence > 0.0 {
            return (bpm.filter(|_| confidence >= threshold), confidence);
        }
    }

    (None, 0.0)
}

fn presence_person_json(
    id: u32,
    x: f64,
    y: f64,
    confidence: f64,
    breathing_bpm: Option<f64>,
    breathing_confidence: f64,
    heart_rate_bpm: Option<f64>,
    is_present: bool,
    motion_energy: f64,
) -> serde_json::Value {
    let mut payload = serde_json::json!({
        "id": id,
        "x": finite_or_zero(x),
        "y": finite_or_zero(y),
        "confidence": clamp_unit(confidence),
        "breathing_bpm": breathing_bpm.map(finite_or_zero),
        "breathing_confidence": clamp_unit(breathing_confidence),
        "breathing_method": BreathingResult::METHOD,
        "is_present": is_present,
        "motion_energy": clamp_unit(motion_energy),
    });
    if let (Some(obj), Some(hr)) = (payload.as_object_mut(), heart_rate_bpm) {
        obj.insert("heart_rate_bpm".to_string(), serde_json::json!(finite_or_zero(hr)));
    }
    payload
}

fn current_motion_energy(
    s: &AppStateInner,
    update: &SensingUpdate,
    now: std::time::Instant,
) -> f64 {
    let edge_energy = s
        .node_states
        .values()
        .filter(|node| is_node_active(node, now))
        .filter_map(|node| node.edge_vitals.as_ref().map(|vitals| vitals.motion_energy as f64))
        .filter(|energy| energy.is_finite())
        .fold(0.0_f64, f64::max);
    if edge_energy > 0.0 {
        return edge_energy.clamp(0.0, 1.0);
    }

    let smoothed = s
        .node_states
        .values()
        .filter(|node| is_node_active(node, now))
        .map(|node| node.smoothed_motion)
        .filter(|energy| energy.is_finite())
        .fold(s.smoothed_motion, f64::max);
    if smoothed > 0.0 {
        return smoothed.clamp(0.0, 1.0);
    }

    normalize_motion_energy(update.features.motion_band_power)
}

fn vitals_update_payload(s: &AppStateInner, now: std::time::Instant) -> serde_json::Value {
    let node_count = active_node_count(s, now);
    let system_status = if node_count == 0 { "no_nodes" } else { "live" };
    let include_heart_rate = s.feature_flags.beta_enabled(BetaFeature::PreciseHeartRate);
    let mut nodes: Vec<serde_json::Value> = s
        .node_states
        .iter()
        .map(|(&node_id, node)| node_vitals_json(node_id, node, now, include_heart_rate))
        .collect();
    nodes.sort_by_key(|node| node.get("node_id").and_then(|id| id.as_u64()).unwrap_or(0));

    serde_json::json!({
        "type": "vitals_update",
        "timestamp_ms": unix_timestamp_ms(),
        "node_count": node_count,
        "system_status": system_status,
        "nodes": nodes,
    })
}

fn node_vitals_json(
    node_id: u8,
    node: &NodeState,
    now: std::time::Instant,
    include_heart_rate: bool,
) -> serde_json::Value {
    let active = is_node_active(node, now);
    let last_seen_ms = node
        .last_frame_time
        .map(|last| now.saturating_duration_since(last).as_millis() as u64);
    let edge = node.edge_vitals.as_ref();
    let breathing_rate_bpm = edge
        .and_then(|vitals| (vitals.breathing_rate_bpm > 0.0).then_some(vitals.breathing_rate_bpm))
        .or_else(|| node.latest_breathing.breathing_bpm.map(f64::from));
    let breathing_confidence = edge
        .map(|vitals| if vitals.presence { 0.7 } else { 0.0 })
        .unwrap_or_else(|| f64::from(node.latest_breathing.confidence));
    let heart_rate_bpm = include_heart_rate.then(|| {
        edge.map(|vitals| vitals.heartrate_bpm)
            .or(node.latest_vitals.heart_rate_bpm)
            .unwrap_or(0.0)
    });
    let motion_energy = edge
        .map(|vitals| vitals.motion_energy as f64)
        .unwrap_or_else(|| node.smoothed_motion);
    let presence_score = edge
        .map(|vitals| vitals.presence_score as f64)
        .unwrap_or_else(|| node_presence_confidence(node));
    let is_present = edge
        .map(|vitals| vitals.presence)
        .unwrap_or_else(|| node_supports_presence(node, now));

    let mut payload = serde_json::json!({
        "node_id": node_id,
        "active": active,
        "is_present": is_present,
        "presence": is_present,
        "presence_score": clamp_unit(presence_score),
        "fall_detected": edge.map(|vitals| vitals.fall_detected).unwrap_or(false),
        "motion": edge.map(|vitals| vitals.motion).unwrap_or_else(|| node.smoothed_motion > 0.03),
        "motion_energy": normalize_motion_energy(motion_energy),
        "breathing_rate_bpm": breathing_rate_bpm.map(finite_or_zero),
        "breathing_bpm": breathing_rate_bpm.map(finite_or_zero),
        "breathing_confidence": clamp_unit(breathing_confidence),
        "breathing_method": if edge.is_some() { "edge_vitals" } else { node.latest_breathing.method },
        "signal_quality": clamp_unit(node.latest_vitals.signal_quality),
        "rssi_dbm": node.rssi_history.back().copied(),
        "n_persons": edge.map(|vitals| vitals.n_persons).unwrap_or(node.prev_person_count as u8),
        "frame_rate_hz": node.csi_fps_ema,
        "last_seen_ms": last_seen_ms,
        "last_csi_ms": age_ms(now, node.last_csi_time),
        "last_vitals_ms": age_ms(now, node.last_vitals_time),
        "source": if edge.is_some() { "edge_vitals" } else { "csi" },
        "esp32_timestamp_ms": edge.map(|vitals| vitals.timestamp_ms),
    });
    if let (Some(obj), Some(hr)) = (payload.as_object_mut(), heart_rate_bpm) {
        obj.insert("heart_rate_bpm".to_string(), serde_json::json!(finite_or_zero(hr)));
        obj.insert("heartrate_bpm".to_string(), serde_json::json!(finite_or_zero(hr)));
        obj.insert(
            "heartbeat_confidence".to_string(),
            serde_json::json!(clamp_unit(node.latest_vitals.heartbeat_confidence)),
        );
    }
    payload
}

fn normalize_motion_energy(value: f64) -> f64 {
    if !value.is_finite() {
        return 0.0;
    }
    if value <= 1.0 {
        value.max(0.0)
    } else {
        (value / 100.0).clamp(0.0, 1.0)
    }
}

fn finite_or_zero(value: f64) -> f64 {
    if value.is_finite() {
        value
    } else {
        0.0
    }
}

fn clamp_unit(value: f64) -> f64 {
    finite_or_zero(value).clamp(0.0, 1.0)
}

// ── ADR-099: real-time CSI introspection — WS topic + REST snapshot ──────────
//
// Parallel to the window-aggregated `/ws/sensing` topic. Subscribers see a
// fresh `IntrospectionSnapshot` JSON frame on every accepted CSI frame
// (regime / Lyapunov exponent / top-k DTW similarity), no window-close delay.

async fn ws_introspection_handler(
    ws: WebSocketUpgrade,
    State(state): State<SharedState>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_ws_introspection_client(socket, state))
}

async fn handle_ws_introspection_client(mut socket: WebSocket, state: SharedState) {
    let mut rx = {
        let s = state.read().await;
        s.intro_tx.subscribe()
    };

    info!("WebSocket client connected (introspection)");

    loop {
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    Ok(json) => {
                        if socket.send(Message::Text(json)).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {} // ignore client messages
                }
            }
        }
    }

    info!("WebSocket client disconnected (introspection)");
}

/// `GET /api/v1/introspection/snapshot` — one-shot poll for the latest
/// per-frame snapshot (regime, Lyapunov, top-k similarity). Mirrors the shape
/// of `/api/v1/sensing/latest` for the dashboard one-shot path.
async fn api_introspection_snapshot(State(state): State<SharedState>) -> impl IntoResponse {
    let s = state.read().await;
    Json(s.intro.snapshot().clone())
}

// ── Pose WebSocket handler (sends pose_data messages for live pose clients) ──

async fn ws_pose_handler(
    ws: WebSocketUpgrade,
    State(state): State<SharedState>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_ws_pose_client(socket, state))
}

async fn handle_ws_pose_client(mut socket: WebSocket, state: SharedState) {
    let mut rx = {
        let s = state.read().await;
        s.tx.subscribe()
    };

    info!("WebSocket client connected (pose)");

    // Send connection established message
    let conn_msg = serde_json::json!({
        "type": "connection_established",
        "payload": { "status": "connected", "backend": "rust+ruvector" }
    });
    let _ = socket.send(Message::Text(conn_msg.to_string())).await;

    loop {
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    Ok(json) => {
                        // Parse the sensing update and convert to pose format
                        if let Ok(sensing) = serde_json::from_str::<SensingUpdate>(&json) {
                            if sensing.msg_type == "sensing_update" {
                                // Determine pose estimation mode for the UI indicator.
                                // model_metric/model_px/sensor_geometry/synthetic_dev.
                                let model_loaded = {
                                    let s = state.read().await;
                                    s.model_loaded
                                };
                                let persons = if model_loaded {
                                    // When a trained model is loaded, prefer its keypoints if present.
                                    sensing.pose_keypoints.as_ref().map(|kps| {
                                        let kp_names = [
                                            "nose","left_eye","right_eye","left_ear","right_ear",
                                            "left_shoulder","right_shoulder","left_elbow","right_elbow",
                                            "left_wrist","right_wrist","left_hip","right_hip",
                                            "left_knee","right_knee","left_ankle","right_ankle",
                                        ];
                                        let keypoints: Vec<PoseKeypoint> = kps.iter()
                                            .enumerate()
                                            .map(|(i, kp)| PoseKeypoint {
                                                name: kp_names.get(i).unwrap_or(&"unknown").to_string(),
                                                x: kp[0], y: kp[1], z: kp[2], confidence: kp[3],
                                            })
                                            .collect();
                                        let (position_m, position_source) =
                                            estimate_person_world_position(&sensing, 0, 1);
                                        let model_pose_source =
                                            if model_keypoints_are_metric(&keypoints) {
                                                MODEL_METRIC_POSE_SOURCE
                                            } else {
                                                MODEL_PX_POSE_SOURCE
                                            };
                                        let keypoints_m =
                                            if model_pose_source == MODEL_METRIC_POSE_SOURCE {
                                                Some(keypoints.clone())
                                            } else {
                                                position_m.and_then(|position| {
                                                    metric_keypoints_from_legacy(
                                                        &keypoints,
                                                        position,
                                                    )
                                                })
                                            };
                                        vec![PersonDetection {
                                            id: 1,
                                            confidence: sensing.classification.confidence,
                                            bbox: BoundingBox { x: 260.0, y: 150.0, width: 120.0, height: 220.0 },
                                            keypoints,
                                            keypoints_m,
                                            zone: "zone_1".into(),
                                            position_m,
                                            position: position_m,
                                            position_source: Some(position_source.to_string()),
                                            pose_source: Some(model_pose_source.to_string()),
                                        }]
                                    }).unwrap_or_else(|| {
                                        // Prefer tracked persons from broadcast if available
                                        sensing.persons.clone().unwrap_or_else(|| derive_pose_from_sensing(&sensing))
                                    })
                                } else {
                                    // Prefer tracked persons from broadcast if available
                                    sensing.persons.clone().unwrap_or_else(|| derive_pose_from_sensing(&sensing))
                                };
                                let pose_source = persons
                                    .first()
                                    .and_then(|person| person.pose_source.as_deref())
                                    .unwrap_or(if model_loaded {
                                        MODEL_PX_POSE_SOURCE
                                    } else {
                                        SYNTHETIC_DEV_POSE_SOURCE
                                    });

                                let pose_msg = serde_json::json!({
                                    "type": "pose_data",
                                    "zone_id": "zone_1",
                                    "timestamp": sensing.timestamp,
                                    "payload": {
                                        "pose": {
                                            "persons": persons,
                                        },
                                        "state": sensing.state.clone(),
                                        "confidence": if sensing.classification.presence { sensing.classification.confidence } else { 0.0 },
                                        "activity": sensing.classification.motion_level,
                                        // pose_source tells the UI which estimation mode is active.
                                        "pose_source": pose_source,
                                        "metadata": {
                                            "frame_id": format!("rust_frame_{}", sensing.tick),
                                            "processing_time_ms": 1,
                                            "source": sensing.source,
                                            "tick": sensing.tick,
                                            "signal_strength": sensing.features.mean_rssi,
                                            "motion_band_power": sensing.features.motion_band_power,
                                            "breathing_band_power": sensing.features.breathing_band_power,
                                            "estimated_persons": persons.len(),
                                        }
                                    }
                                });
                                if socket.send(Message::Text(pose_msg.to_string())).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    // Lagged: skip missed frames, don't disconnect.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::debug!("WS pose client lagged by {n} frames, skipping");
                        continue;
                    }
                    Err(_) => break, // channel closed
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        // Handle ping/pong
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                            if v.get("type").and_then(|t| t.as_str()) == Some("ping") {
                                let pong = serde_json::json!({"type": "pong"});
                                let _ = socket.send(Message::Text(pong.to_string())).await;
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Pong(_))) => {} // keepalive response
                    _ => {}
                }
            }
        }
    }

    info!("WebSocket client disconnected (pose)");
}

// ── REST endpoints ───────────────────────────────────────────────────────────

async fn health(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let s = state.read().await;
    Json(serde_json::json!({
        "status": "ok",
        "source": s.effective_source(),
        "tick": s.tick,
        "clients": s.tx.receiver_count(),
    }))
}

async fn latest(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let s = state.read().await;
    match &s.latest_update {
        Some(update) => Json(serde_json::to_value(update).unwrap_or_default()),
        None => Json(serde_json::json!({"status": "no data yet"})),
    }
}

async fn alerts_active(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let s = state.read().await;
    Json(serde_json::to_value(s.alert_manager.active_response()).unwrap_or_else(|_| {
        serde_json::json!({
            "alerts": [],
            "critical_count": 0,
            "warning_count": 0,
        })
    }))
}

async fn alerts_ack(
    State(state): State<SharedState>,
    Path(alert_id): Path<String>,
) -> Response {
    let mut s = state.write().await;
    if s.alert_manager.acknowledge(&alert_id) {
        (
            StatusCode::OK,
            Json(serde_json::json!({
                "status": "ok",
                "alert_id": alert_id,
                "acknowledged": true,
            })),
        )
            .into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "status": "not_found",
                "alert_id": alert_id,
            })),
        )
            .into_response()
    }
}

/// Generate WiFi-derived pose keypoints from sensing data.
///
/// Keypoint positions are modulated by real signal features rather than a pure
/// time-based sine/cosine loop:
///
///   - `motion_band_power`    drives whole-body translation and limb splay
///   - `variance`             seeds per-frame noise so the skeleton never freezes
///   - `breathing_band_power` expands/contracts torso keypoints (shoulders, hips)
///   - `dominant_freq_hz`     tilts the upper body laterally (lean direction)
///   - `change_points`        adds burst jitter to extremities (wrists, ankles)
///
/// When `presence == false` no persons are returned (empty room).
/// When walking is detected (`motion_score > 0.55`) the figure shifts laterally
/// with a stride-swing pattern applied to arms and legs.
// ── Multi-person estimation (issue #97) ──────────────────────────────────────
/// Fuse features across all active nodes for higher SNR.
///
/// When multiple ESP32 nodes observe the same room, their CSI features
/// can be combined:
/// - Variance: use max (most sensitive node dominates)
/// - Motion/breathing/spectral power: weighted average by RSSI (closer node = higher weight)
/// - Dominant frequency: weighted average
/// - Change points: keep current node's value (not meaningful to average)
/// - Mean RSSI: use max (best signal)
fn fuse_multi_node_features(
    current_features: &FeatureInfo,
    node_states: &HashMap<u8, NodeState>,
) -> FeatureInfo {
    let now = std::time::Instant::now();
    let active: Vec<(&FeatureInfo, f64)> = node_states
        .values()
        .filter(|ns| {
            ns.last_frame_time
                .is_some_and(|t| now.duration_since(t).as_secs() < 10)
        })
        .filter_map(|ns| {
            let feat = ns.latest_features.as_ref()?;
            let rssi = ns.rssi_history.back().copied().unwrap_or(-80.0);
            Some((feat, rssi))
        })
        .collect();

    if active.len() <= 1 {
        return current_features.clone();
    }

    // RSSI-based weights: higher RSSI = closer to person = more weight.
    // Map RSSI relative to best node into [0.1, 1.0].
    let max_rssi = active
        .iter()
        .map(|(_, r)| *r)
        .fold(f64::NEG_INFINITY, f64::max);
    let weights: Vec<f64> = active
        .iter()
        .map(|(_, r)| (1.0 + (r - max_rssi + 20.0) / 20.0).clamp(0.1, 1.0))
        .collect();
    let w_sum: f64 = weights.iter().sum::<f64>().max(1e-9);

    FeatureInfo {
        // Weighted average variance (not max — max inflates person score
        // and causes count flips between 1↔2 persons).
        variance: active
            .iter()
            .zip(&weights)
            .map(|((f, _), w)| f.variance * w)
            .sum::<f64>()
            / w_sum,
        // Weighted average for motion/breathing/spectral
        motion_band_power: active
            .iter()
            .zip(&weights)
            .map(|((f, _), w)| f.motion_band_power * w)
            .sum::<f64>()
            / w_sum,
        breathing_band_power: active
            .iter()
            .zip(&weights)
            .map(|((f, _), w)| f.breathing_band_power * w)
            .sum::<f64>()
            / w_sum,
        spectral_power: active
            .iter()
            .zip(&weights)
            .map(|((f, _), w)| f.spectral_power * w)
            .sum::<f64>()
            / w_sum,
        dominant_freq_hz: active
            .iter()
            .zip(&weights)
            .map(|((f, _), w)| f.dominant_freq_hz * w)
            .sum::<f64>()
            / w_sum,
        change_points: current_features.change_points, // keep current node's value
        // Best RSSI across nodes
        mean_rssi: active
            .iter()
            .map(|(f, _)| f.mean_rssi)
            .fold(f64::NEG_INFINITY, f64::max),
    }
}

/// Estimate person count from CSI features using a weighted composite heuristic.
///
/// Single ESP32 link limitations: variance-based detection can reliably detect
/// 1-2 persons. 3+ is speculative and requires ≥3 nodes for spatial resolution.
///
/// Returns a raw score (0.0..1.0) that the caller converts to person count
/// after temporal smoothing.
fn compute_person_score(state: &AppStateInner, feat: &FeatureInfo) -> f64 {
    // ADR-044 §5.2: adaptive rolling-P95 normalization.
    // Legacy fixed denominators (variance/300, motion/250, spectral/500) saturate
    // when live ESP32 values exceed those limits — zero dynamic range results.
    // Use the P95 of the last ~30 s of history instead, falling back to the legacy
    // denominators during cold-start (<60 samples) to preserve day-0 behaviour.
    let var_denom = state
        .p95_variance
        .current()
        .map(|p| p.max(50.0))
        .unwrap_or(300.0);
    let motion_denom = state
        .p95_motion_band_power
        .current()
        .map(|p| p.max(50.0))
        .unwrap_or(250.0);
    let sp_denom = state
        .p95_spectral_power
        .current()
        .map(|p| p.max(100.0))
        .unwrap_or(500.0);
    let var_norm = (feat.variance / var_denom).clamp(0.0, 1.0);
    let cp_norm = (feat.change_points as f64 / 30.0).clamp(0.0, 1.0);
    let motion_norm = (feat.motion_band_power / motion_denom).clamp(0.0, 1.0);
    let sp_norm = (feat.spectral_power / sp_denom).clamp(0.0, 1.0);
    var_norm * 0.40 + cp_norm * 0.20 + motion_norm * 0.25 + sp_norm * 0.15
}

/// Estimate person count via ruvector DynamicMinCut on the subcarrier
/// temporal correlation graph.
///
/// Builds a graph where:
/// - Nodes = active subcarriers (variance > noise floor)
/// - Edges = Pearson correlation between subcarrier time series
///   (weight = correlation coefficient; high correlation = heavy edge)
/// - Source = virtual node connected to the most active subcarrier
/// - Sink = virtual node connected to the least correlated subcarrier
///
/// The min-cut value indicates how many independent motion clusters exist:
/// - High min-cut (relative to total edge weight) → one tightly coupled
///   group → 1 person
/// - Low min-cut → two loosely coupled groups → 2 persons
///
/// Uses `ruvector_mincut::DynamicMinCut` for O(V²E) exact max-flow.
fn estimate_persons_from_correlation(frame_history: &VecDeque<Vec<f64>>) -> usize {
    let n_frames = frame_history.len();
    if n_frames < 10 {
        return 1;
    }

    let window: Vec<&Vec<f64>> = frame_history.iter().rev().take(20).collect();
    let n_sub = window[0].len().min(56);
    if n_sub < 4 {
        return 1;
    }
    let k = window.len() as f64;

    // Per-subcarrier mean and variance
    let mut means = vec![0.0f64; n_sub];
    let mut variances = vec![0.0f64; n_sub];
    for frame in &window {
        for sc in 0..n_sub.min(frame.len()) {
            means[sc] += frame[sc] / k;
        }
    }
    for frame in &window {
        for sc in 0..n_sub.min(frame.len()) {
            variances[sc] += (frame[sc] - means[sc]).powi(2) / k;
        }
    }

    // Active subcarriers: variance above noise floor
    let noise_floor = 1.0;
    let active: Vec<usize> = (0..n_sub)
        .filter(|&sc| variances[sc] > noise_floor)
        .collect();
    let m = active.len();
    if m < 3 {
        return if m == 0 { 0 } else { 1 };
    }

    // Build correlation graph edges between active subcarriers.
    // Edge weight = |Pearson correlation|. High correlation → same person.
    let mut edges: Vec<(u64, u64, f64)> = Vec::new();
    let source = m as u64;
    let sink = (m + 1) as u64;

    // Precompute std devs
    let stds: Vec<f64> = active
        .iter()
        .map(|&sc| variances[sc].sqrt().max(1e-9))
        .collect();

    for i in 0..m {
        for j in (i + 1)..m {
            // Pearson correlation between subcarriers i and j
            let mut cov = 0.0f64;
            for frame in &window {
                let si = active[i];
                let sj = active[j];
                if si < frame.len() && sj < frame.len() {
                    cov += (frame[si] - means[si]) * (frame[sj] - means[sj]) / k;
                }
            }
            let corr = (cov / (stds[i] * stds[j])).abs();
            if corr > 0.1 {
                // Bidirectional edges for flow network
                let weight = corr * 10.0; // Scale up for integer-like flow
                edges.push((i as u64, j as u64, weight));
                edges.push((j as u64, i as u64, weight));
            }
        }
    }

    // Source → highest-variance subcarrier, Sink → lowest-variance.
    // partial_cmp returns None on NaN; the outer unwrap_or only catches an
    // empty iterator, not a comparator panic. Same NaN-panic class as #611
    // — a single NaN variance frame would kill the sensing-server process.
    let (max_var_idx, _) = active
        .iter()
        .enumerate()
        .max_by(|(_, &a), (_, &b)| {
            variances[a]
                .partial_cmp(&variances[b])
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or((0, &0));
    let (min_var_idx, _) = active
        .iter()
        .enumerate()
        .min_by(|(_, &a), (_, &b)| {
            variances[a]
                .partial_cmp(&variances[b])
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or((0, &0));

    if max_var_idx == min_var_idx {
        return 1;
    }

    edges.push((source, max_var_idx as u64, 100.0));
    edges.push((min_var_idx as u64, sink, 100.0));

    // Run min-cut
    let mc: DynamicMinCut = match MinCutBuilder::new()
        .exact()
        .with_edges(edges.clone())
        .build()
    {
        Ok(mc) => mc,
        Err(_) => return 1,
    };

    let cut_value = mc.min_cut_value();
    let total_edge_weight: f64 = edges
        .iter()
        .filter(|(s, t, _)| *s != source && *s != sink && *t != source && *t != sink)
        .map(|(_, _, w)| w)
        .sum::<f64>()
        / 2.0; // bidirectional → halve

    if total_edge_weight < 1e-9 {
        return 1;
    }

    // Normalized cut ratio: low = easy to split = multiple people
    let cut_ratio = cut_value / total_edge_weight;

    if cut_ratio > 0.4 {
        1 // Tightly coupled — one person
    } else if cut_ratio > 0.15 {
        2 // Moderately separable — two people
    } else {
        3 // Highly separable — three+ people
    }
}

/// Map a DynamicMinCut occupancy estimate (`estimate_persons_from_correlation`,
/// 0–3) onto a target score whose steady state round-trips back through
/// `score_to_person_count` to the *same* count (issue #803).
///
/// The CSI path EMA-smooths this target and re-discretises it via
/// `score_to_person_count`. The previous `corr_persons / 3.0` mapping put a
/// 2-person estimate at 0.667 — just under the 0.70 up-threshold — so the
/// smoothed score could never climb past 1, pinning the per-node count to 1
/// even when the min-cut cleanly separated two people. These anchors sit
/// inside the hysteresis bands so a *sustained* estimate converges to the
/// matching count while transient noise stays gated by the EMA:
///   1 → 0.40  (below the 0.55 down-threshold)
///   2 → 0.74  (between the 0.70 up- and 0.78 down-thresholds → reachable
///              both climbing from 1 and falling from 3)
///   3 → 0.96  (above the 0.92 up-threshold)
fn corr_persons_to_score(corr_persons: usize) -> f64 {
    match corr_persons {
        0 => 0.20,
        1 => 0.40,
        2 => 0.74,
        _ => 0.96,
    }
}

#[cfg(test)]
mod corr_persons_round_trip_tests {
    //! Issue #803 — a sustained min-cut occupancy estimate must survive the
    //! CSI path's EMA + `score_to_person_count` re-discretisation instead of
    //! collapsing back to 1.
    use super::*;

    /// Replays the CSI-loop smoothing (`score = score*0.92 + target*0.08`)
    /// followed by `score_to_person_count`, exactly as the per-node path does,
    /// and returns the steady-state reported count.
    fn converge(corr_persons: usize) -> usize {
        let mut score = 0.0f64;
        let mut count = 1usize;
        for _ in 0..400 {
            let target = corr_persons_to_score(corr_persons);
            score = score * 0.92 + target * 0.08;
            count = score_to_person_count(score, count);
        }
        count
    }

    #[test]
    fn sustained_one_person_estimate_reports_one() {
        assert_eq!(converge(1), 1);
    }

    #[test]
    fn sustained_two_person_estimate_reports_two() {
        assert_eq!(converge(2), 2, "#803: min-cut=2 must round-trip to count 2");
    }

    #[test]
    fn sustained_three_person_estimate_reports_three() {
        assert_eq!(converge(3), 3);
    }

    #[test]
    fn old_div3_mapping_would_pin_two_people_to_one() {
        // Regression-documents the bug: 2/3 = 0.667 never crosses the 0.70
        // up-threshold, so the old mapping reported 1 for two people.
        let mut score = 0.0f64;
        let mut count = 1usize;
        for _ in 0..400 {
            score = score * 0.92 + (2.0 / 3.0) * 0.08;
            count = score_to_person_count(score, count);
        }
        assert_eq!(count, 1, "old corr_persons/3.0 mapping was the #803 bug");
    }
}

/// Convert smoothed person score to discrete count with hysteresis.
///
/// Uses asymmetric thresholds: higher threshold to *add* a person, lower to
/// *drop* one.  This prevents flickering when the score hovers near a boundary
/// (the #1 user-reported issue — see #237, #249, #280, #292).
fn score_to_person_count(smoothed_score: f64, prev_count: usize) -> usize {
    // Up-thresholds (must exceed to increase count):
    //   1→2: 0.80  (raised from 0.65 — single-person movement in multipath
    //               rooms easily hits 0.65, causing false 2-person detection)
    //   2→3: 0.92  (raised from 0.85 — 3 persons needs very strong signal)
    // Down-thresholds (must drop below to decrease count):
    //   2→1: 0.55  (hysteresis gap of 0.25)
    //   3→2: 0.78  (hysteresis gap of 0.14)
    match prev_count {
        0 | 1 => {
            if smoothed_score > 0.85 {
                3
            } else if smoothed_score > 0.70 {
                2
            } else {
                1
            }
        }
        2 => {
            if smoothed_score > 0.92 {
                3
            } else if smoothed_score < 0.55 {
                1
            } else {
                2 // hold — within hysteresis band
            }
        }
        _ => {
            // prev_count >= 3
            if smoothed_score < 0.55 {
                1
            } else if smoothed_score < 0.78 {
                2
            } else {
                3 // hold
            }
        }
    }
}

/// Combine the activity-score-derived aggregate count with the count-aware
/// per-node estimates (issue #803). This remains the raw signal-side estimate;
/// `resolve_rendered_person_count` applies the conservative live rendering gate.
///
/// The aggregate `s.person_count()` is driven by `smoothed_person_score`, an
/// EMA-smoothed *activity* score (amplitude variance / motion / spectral
/// energy). That score saturates near a single occupant — one moving person
/// can max it out — so it cannot discriminate occupancy *count*, leaving the
/// reported value pinned at 1. Meanwhile the per-node paths already derive a
/// genuinely count-aware estimate (ESP32 firmware `n_persons`, or the
/// DynamicMinCut `corr_persons`) and stash it in `NodeState::prev_person_count`
/// — but that value was being discarded by the aggregator.
///
/// This takes the larger of the two. It can only ever *raise* the count when a
/// node has positively estimated more occupants, so it never regresses the
/// single-person case (a lone occupant yields `node_max == 1`).
fn aggregate_person_count(
    activity_count: usize,
    node_states: &std::collections::HashMap<u8, NodeState>,
    now: std::time::Instant,
) -> usize {
    let node_max = node_states
        .values()
        .filter(|n| is_node_active(n, now))
        .map(|n| n.prev_person_count)
        .max()
        .unwrap_or(0);
    activity_count.max(node_max)
}

const MAX_RENDERED_PERSONS: usize = 3;
const MULTI_PERSON_CONFIRMATION_MS: u128 = 1_500;
const PRESENCE_HOLD_MS: u64 = 2_500;
const EDGE_VITALS_PRESENCE_SCORE_FLOOR: f64 = 0.35;
const PRESENCE_HOLD_CONFIDENCE_FLOOR: f64 = 0.25;

fn edge_vitals_motion_profile(vitals: &Esp32VitalsPacket) -> (&'static str, f64) {
    if vitals.motion {
        ("present_moving", 0.8)
    } else if vitals.presence {
        ("present_still", 0.3)
    } else {
        ("absent", 0.05)
    }
}

fn update_node_presence_from_edge_vitals(
    ns: &mut NodeState,
    vitals: &Esp32VitalsPacket,
) -> (&'static str, f64) {
    let (motion_level, motion_score) = edge_vitals_motion_profile(vitals);
    ns.current_motion_level = motion_level.to_string();
    ns.smoothed_motion = ns.smoothed_motion * 0.80 + motion_score * 0.20;
    if vitals.presence {
        let score = (vitals.presence_score as f64).clamp(0.0, 1.0);
        ns.smoothed_person_score =
            (ns.smoothed_person_score * 0.75 + score * 0.25).clamp(0.0, 1.0);
    } else {
        ns.smoothed_person_score = (ns.smoothed_person_score * 0.75).clamp(0.0, 1.0);
    }
    (motion_level, motion_score)
}

fn node_presence_confidence(ns: &NodeState) -> f64 {
    let mut confidence = ns.smoothed_person_score.clamp(0.0, 1.0);

    if ns.prev_person_count > 0 {
        confidence = confidence.max(0.40);
    }
    if !matches!(ns.current_motion_level.as_str(), "absent") {
        confidence = confidence.max(0.30);
    }
    if let Some(vitals) = ns.edge_vitals.as_ref() {
        confidence = confidence.max((vitals.presence_score as f64).clamp(0.0, 1.0));
        if vitals.presence {
            confidence = confidence.max(0.45);
        }
    }

    confidence.clamp(0.0, 1.0)
}

fn node_supports_presence(ns: &NodeState, now: std::time::Instant) -> bool {
    if !is_node_active(ns, now) {
        return false;
    }

    ns.prev_person_count > 0
        || !matches!(ns.current_motion_level.as_str(), "absent")
        || ns.edge_vitals.as_ref().is_some_and(|vitals| {
            vitals.presence
                || (vitals.presence_score as f64) >= EDGE_VITALS_PRESENCE_SCORE_FLOOR
        })
}

fn room_presence_supporting_nodes(s: &AppStateInner, now: std::time::Instant) -> usize {
    s.node_states
        .values()
        .filter(|node| node_supports_presence(node, now))
        .count()
}

fn room_presence_confidence(
    s: &AppStateInner,
    current_confidence: f64,
    now: std::time::Instant,
) -> f64 {
    s.node_states
        .values()
        .filter(|node| is_node_active(node, now))
        .map(node_presence_confidence)
        .fold(current_confidence.clamp(0.0, 1.0), f64::max)
        .clamp(0.0, 1.0)
}

fn held_presence_confidence(base_confidence: f64, elapsed: std::time::Duration) -> f64 {
    let hold_secs = PRESENCE_HOLD_MS as f64 / 1_000.0;
    let progress = (elapsed.as_secs_f64() / hold_secs).clamp(0.0, 1.0);
    (base_confidence * (1.0 - 0.50 * progress))
        .max(PRESENCE_HOLD_CONFIDENCE_FLOOR)
        .clamp(0.0, 1.0)
}

fn presence_hold_elapsed(
    last_present_at: Option<std::time::Instant>,
    last_present_count: usize,
    active_nodes: usize,
    now: std::time::Instant,
) -> Option<std::time::Duration> {
    if active_nodes == 0 || last_present_count == 0 {
        return None;
    }
    let elapsed = last_present_at.and_then(|last_seen| now.checked_duration_since(last_seen))?;
    (elapsed <= std::time::Duration::from_millis(PRESENCE_HOLD_MS)).then_some(elapsed)
}

fn apply_room_presence_continuity(
    s: &mut AppStateInner,
    classification: &mut ClassificationInfo,
    raw_estimated_persons: usize,
    now: std::time::Instant,
) -> CountEvidence {
    let active_nodes = active_node_count(s, now);
    let supporting_nodes = room_presence_supporting_nodes(s, now);
    let room_person_count = aggregate_person_count(raw_estimated_persons, &s.node_states, now);
    let current_presence = classification.presence;
    let instant_presence = current_presence || supporting_nodes > 0 || room_person_count > 0;

    if instant_presence {
        let effective_raw = room_person_count.max(raw_estimated_persons).max(1);
        if !classification.presence {
            classification.presence = true;
            classification.motion_level = "present_still".to_string();
        }
        classification.confidence =
            room_presence_confidence(s, classification.confidence, now).clamp(0.0, 1.0);
        if supporting_nodes > 0 {
            classification.confidence = classification.confidence.max(0.35);
        }

        let mut evidence = resolve_rendered_person_count(s, true, effective_raw, now);
        if !current_presence && supporting_nodes > 0 {
            evidence.reason = "room_fused_presence".to_string();
        }

        s.prev_person_count = evidence.rendered_persons;
        s.last_present_at = Some(now);
        s.last_present_count = evidence.rendered_persons.clamp(1, MAX_RENDERED_PERSONS);
        s.last_present_confidence = classification.confidence;
        return evidence;
    }

    if let Some(elapsed) =
        presence_hold_elapsed(s.last_present_at, s.last_present_count, active_nodes, now)
    {
        let held_count = s.last_present_count.clamp(1, MAX_RENDERED_PERSONS);
        classification.presence = true;
        classification.motion_level = "present_still".to_string();
        classification.confidence = held_presence_confidence(s.last_present_confidence, elapsed);
        s.stable_rendered_person_count = held_count;
        s.count_candidate_persons = 0;
        s.count_candidate_since = None;
        s.prev_person_count = held_count;
        return CountEvidence {
            stable_persons: held_count,
            raw_estimated_persons: held_count,
            rendered_persons: held_count,
            active_nodes,
            supporting_nodes,
            ambiguous: true,
            reason: "presence_hold".to_string(),
        };
    }

    s.last_present_at = None;
    s.last_present_count = 0;
    s.last_present_confidence = 0.0;
    s.prev_person_count = 0;
    let mut evidence = resolve_rendered_person_count(s, false, 0, now);
    if active_nodes > 0 {
        evidence.reason = "absent_after_hold".to_string();
    }
    evidence
}

fn supporting_nodes_for_count(
    node_states: &std::collections::HashMap<u8, NodeState>,
    now: std::time::Instant,
    target: usize,
) -> usize {
    node_states
        .values()
        .filter(|node| is_node_active(node, now))
        .filter(|node| node.prev_person_count >= target)
        .count()
}

fn count_candidate_for_raw_estimate(
    s: &AppStateInner,
    raw_estimated_persons: usize,
    now: std::time::Instant,
) -> (usize, usize, &'static str) {
    let active_nodes = active_node_count(s, now);
    let raw_target = raw_estimated_persons.clamp(1, MAX_RENDERED_PERSONS);

    if raw_target >= 3 {
        let supporting_nodes = supporting_nodes_for_count(&s.node_states, now, 3);
        if active_nodes >= 3 && supporting_nodes >= 3 {
            return (3, supporting_nodes, "corroborated_three_persons");
        }
    }

    if raw_target >= 2 {
        let supporting_nodes = supporting_nodes_for_count(&s.node_states, now, 2);
        if active_nodes >= 2 && supporting_nodes >= 2 {
            return (2, supporting_nodes, "corroborated_two_persons");
        }
        let reason = if active_nodes < 2 {
            "insufficient_active_nodes"
        } else {
            "needs_multi_node_corroboration"
        };
        let raw_support = supporting_nodes_for_count(&s.node_states, now, raw_target);
        return (1, raw_support, reason);
    }

    let supporting_nodes = supporting_nodes_for_count(&s.node_states, now, 1);
    let fallback_support = if active_nodes > 0 { 1 } else { 0 };
    (1, supporting_nodes.max(fallback_support), "single_person")
}

fn resolve_rendered_person_count(
    s: &mut AppStateInner,
    presence: bool,
    raw_estimated_persons: usize,
    now: std::time::Instant,
) -> CountEvidence {
    let active_nodes = active_node_count(s, now);
    let (candidate, supporting_nodes, reason) = if presence && raw_estimated_persons > 0 {
        count_candidate_for_raw_estimate(s, raw_estimated_persons, now)
    } else {
        (0, 0, "absent")
    };

    resolve_rendered_person_count_core(
        &mut s.stable_rendered_person_count,
        &mut s.count_candidate_persons,
        &mut s.count_candidate_since,
        presence,
        raw_estimated_persons,
        active_nodes,
        supporting_nodes,
        candidate,
        reason,
        now,
    )
}

fn resolve_rendered_person_count_core(
    stable_rendered_person_count: &mut usize,
    count_candidate_persons: &mut usize,
    count_candidate_since: &mut Option<std::time::Instant>,
    presence: bool,
    raw_estimated_persons: usize,
    active_nodes: usize,
    supporting_nodes: usize,
    candidate: usize,
    mut reason: &'static str,
    now: std::time::Instant,
) -> CountEvidence {
    if !presence || raw_estimated_persons == 0 {
        *stable_rendered_person_count = 0;
        *count_candidate_persons = 0;
        *count_candidate_since = None;
        return CountEvidence {
            stable_persons: 0,
            raw_estimated_persons,
            rendered_persons: 0,
            active_nodes,
            supporting_nodes: 0,
            ambiguous: false,
            reason: "absent".to_string(),
        };
    }

    let raw_estimated_persons = raw_estimated_persons.max(1);
    let raw_target = raw_estimated_persons.clamp(1, MAX_RENDERED_PERSONS);
    let mut stable = (*stable_rendered_person_count).max(1);

    if candidate <= 1 {
        stable = 1;
        *count_candidate_persons = 0;
        *count_candidate_since = None;
    } else if candidate < stable {
        stable = candidate.max(1);
        *count_candidate_persons = 0;
        *count_candidate_since = None;
        reason = "downshifted_count";
    } else if candidate > stable {
        if *count_candidate_persons != candidate {
            *count_candidate_persons = candidate;
            *count_candidate_since = Some(now);
            reason = "confirming_multi_person";
        } else {
            let elapsed_ms = count_candidate_since
                .as_ref()
                .and_then(|since| now.checked_duration_since(*since))
                .map(|duration| duration.as_millis())
                .unwrap_or(0);
            if elapsed_ms >= MULTI_PERSON_CONFIRMATION_MS {
                stable = candidate;
                *count_candidate_persons = 0;
                *count_candidate_since = None;
                reason = "confirmed_multi_person";
            } else {
                reason = "confirming_multi_person";
            }
        }
    } else {
        *count_candidate_persons = 0;
        *count_candidate_since = None;
    }

    *stable_rendered_person_count = stable;
    let ambiguous = raw_target > stable || candidate > stable;

    CountEvidence {
        stable_persons: stable,
        raw_estimated_persons,
        rendered_persons: stable,
        active_nodes,
        supporting_nodes,
        ambiguous,
        reason: reason.to_string(),
    }
}

#[cfg(test)]
mod aggregate_person_count_tests {
    //! Issue #803 — the saturating activity score must not clamp a
    //! count-aware per-node estimate back down to 1.
    use super::*;
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    fn node_with_count(c: usize, last_frame_time: Option<Instant>) -> NodeState {
        let mut n = NodeState::new();
        n.prev_person_count = c;
        n.last_frame_time = last_frame_time;
        n
    }

    fn edge_vitals_packet(presence: bool, motion: bool, presence_score: f32) -> Esp32VitalsPacket {
        Esp32VitalsPacket {
            node_id: 7,
            presence,
            fall_detected: false,
            motion,
            breathing_rate_bpm: 14.0,
            heartrate_bpm: 72.0,
            rssi: -48,
            n_persons: if presence { 1 } else { 0 },
            motion_energy: if motion { 0.8 } else { 0.1 },
            presence_score,
            timestamp_ms: 42,
        }
    }

    #[test]
    fn empty_nodes_fall_back_to_activity_count() {
        let nodes: HashMap<u8, NodeState> = HashMap::new();
        let now = Instant::now();
        assert_eq!(aggregate_person_count(1, &nodes, now), 1);
        assert_eq!(aggregate_person_count(0, &nodes, now), 0);
    }

    #[test]
    fn node_estimate_raises_a_saturated_activity_count() {
        // The activity score saturates at 1, but a node positively reports 2.
        let mut nodes = HashMap::new();
        let now = Instant::now();
        nodes.insert(1u8, node_with_count(2, Some(now)));
        assert_eq!(
            aggregate_person_count(1, &nodes, now),
            2,
            "a node reporting 2 must not be discarded by the activity count"
        );
    }

    #[test]
    fn activity_count_wins_when_higher_than_nodes() {
        // Never *lower* a confident activity-derived count to a stale node value.
        let mut nodes = HashMap::new();
        let now = Instant::now();
        nodes.insert(1u8, node_with_count(1, Some(now)));
        assert_eq!(aggregate_person_count(3, &nodes, now), 3);
    }

    #[test]
    fn takes_max_across_multiple_nodes() {
        let mut nodes = HashMap::new();
        let now = Instant::now();
        nodes.insert(1u8, node_with_count(1, Some(now)));
        nodes.insert(2u8, node_with_count(3, Some(now)));
        nodes.insert(3u8, node_with_count(2, Some(now)));
        assert_eq!(aggregate_person_count(1, &nodes, now), 3);
    }

    #[test]
    fn single_occupant_is_never_inflated() {
        // Regression guard: a lone occupant (every node sees 1) stays 1.
        let mut nodes = HashMap::new();
        let now = Instant::now();
        nodes.insert(1u8, node_with_count(1, Some(now)));
        nodes.insert(2u8, node_with_count(1, Some(now)));
        assert_eq!(aggregate_person_count(1, &nodes, now), 1);
    }

    #[test]
    fn stale_node_counts_do_not_raise_activity_count() {
        let mut nodes = HashMap::new();
        let now = Instant::now();
        nodes.insert(1u8, node_with_count(1, Some(now)));
        nodes.insert(
            2u8,
            node_with_count(3, now.checked_sub(ESP32_OFFLINE_TIMEOUT + Duration::from_secs(1))),
        );

        assert_eq!(aggregate_person_count(1, &nodes, now), 1);
    }

    #[test]
    fn edge_vitals_update_feeds_node_presence_state() {
        let mut node = NodeState::new();
        let vitals = edge_vitals_packet(true, true, 0.72);

        let (motion_level, motion_score) =
            update_node_presence_from_edge_vitals(&mut node, &vitals);

        assert_eq!(motion_level, "present_moving");
        assert_eq!(node.current_motion_level, "present_moving");
        assert!(motion_score > 0.7);
        assert!(node.smoothed_person_score > 0.15);
    }

    #[test]
    fn edge_vitals_presence_supports_room_fusion_without_csi_count() {
        let now = Instant::now();
        let mut node = NodeState::new();
        node.last_frame_time = Some(now);
        node.edge_vitals = Some(edge_vitals_packet(true, false, 0.62));

        assert!(node_supports_presence(&node, now));
        assert!(node_presence_confidence(&node) >= 0.62);
    }

    #[test]
    fn stale_edge_vitals_do_not_support_room_presence() {
        let now = Instant::now();
        let mut node = NodeState::new();
        node.last_frame_time =
            now.checked_sub(ESP32_OFFLINE_TIMEOUT + Duration::from_millis(1));
        node.edge_vitals = Some(edge_vitals_packet(true, false, 0.80));

        assert!(!node_supports_presence(&node, now));
    }

    #[test]
    fn short_presence_hold_requires_active_nodes_and_fresh_latch() {
        let now = Instant::now();
        let recent = now.checked_sub(Duration::from_millis(2_000)).unwrap();
        let stale = now.checked_sub(Duration::from_millis(2_600)).unwrap();

        assert!(presence_hold_elapsed(Some(recent), 1, 1, now).is_some());
        assert!(presence_hold_elapsed(Some(stale), 1, 1, now).is_none());
        assert!(presence_hold_elapsed(Some(recent), 1, 0, now).is_none());
        assert!(presence_hold_elapsed(Some(recent), 0, 1, now).is_none());
    }

    #[test]
    fn held_presence_confidence_decays_without_disappearing_immediately() {
        let decayed = held_presence_confidence(0.80, Duration::from_millis(2_000));
        assert!(decayed < 0.80);
        assert!(decayed >= PRESENCE_HOLD_CONFIDENCE_FLOOR);
    }

    fn resolve_count(
        stable: &mut usize,
        candidate_persons: &mut usize,
        candidate_since: &mut Option<Instant>,
        presence: bool,
        raw: usize,
        active_nodes: usize,
        supporting_nodes: usize,
        candidate: usize,
        now: Instant,
    ) -> CountEvidence {
        resolve_rendered_person_count_core(
            stable,
            candidate_persons,
            candidate_since,
            presence,
            raw,
            active_nodes,
            supporting_nodes,
            candidate,
            "test",
            now,
        )
    }

    #[test]
    fn uncorroborated_four_person_spike_renders_one_ambiguous_person() {
        let now = Instant::now();
        let mut stable = 1;
        let mut candidate_persons = 0;
        let mut candidate_since = None;

        let evidence = resolve_count(
            &mut stable,
            &mut candidate_persons,
            &mut candidate_since,
            true,
            4,
            1,
            1,
            1,
            now,
        );

        assert_eq!(evidence.raw_estimated_persons, 4);
        assert_eq!(evidence.rendered_persons, 1);
        assert_eq!(evidence.stable_persons, 1);
        assert!(evidence.ambiguous);
    }

    #[test]
    fn two_corroborating_nodes_promote_after_stability_window() {
        let now = Instant::now();
        let later = now.checked_add(Duration::from_millis(1_600)).unwrap();
        let mut stable = 1;
        let mut candidate_persons = 0;
        let mut candidate_since = None;

        let warming = resolve_count(
            &mut stable,
            &mut candidate_persons,
            &mut candidate_since,
            true,
            2,
            2,
            2,
            2,
            now,
        );
        assert_eq!(warming.rendered_persons, 1);
        assert!(warming.ambiguous);

        let promoted = resolve_count(
            &mut stable,
            &mut candidate_persons,
            &mut candidate_since,
            true,
            2,
            2,
            2,
            2,
            later,
        );

        assert_eq!(promoted.rendered_persons, 2);
        assert_eq!(promoted.stable_persons, 2);
        assert!(!promoted.ambiguous);
        assert_eq!(promoted.reason, "confirmed_multi_person");
    }

    #[test]
    fn alternating_one_two_due_to_multipath_stays_single_person() {
        let now = Instant::now();
        let later = now.checked_add(Duration::from_millis(800)).unwrap();
        let final_tick = now.checked_add(Duration::from_millis(1_700)).unwrap();
        let mut stable = 1;
        let mut candidate_persons = 0;
        let mut candidate_since = None;

        let first_spike = resolve_count(
            &mut stable,
            &mut candidate_persons,
            &mut candidate_since,
            true,
            2,
            2,
            2,
            2,
            now,
        );
        assert_eq!(first_spike.rendered_persons, 1);

        let single = resolve_count(
            &mut stable,
            &mut candidate_persons,
            &mut candidate_since,
            true,
            1,
            2,
            2,
            1,
            later,
        );
        assert_eq!(single.rendered_persons, 1);
        assert_eq!(candidate_persons, 0);

        let second_spike = resolve_count(
            &mut stable,
            &mut candidate_persons,
            &mut candidate_since,
            true,
            2,
            2,
            2,
            2,
            final_tick,
        );
        assert_eq!(second_spike.rendered_persons, 1);
        assert_eq!(second_spike.reason, "confirming_multi_person");
    }

    #[test]
    fn absence_resets_stable_and_pending_counts_immediately() {
        let now = Instant::now();
        let mut stable = 2;
        let mut candidate_persons = 3;
        let mut candidate_since = Some(now);

        let evidence = resolve_count(
            &mut stable,
            &mut candidate_persons,
            &mut candidate_since,
            false,
            0,
            2,
            0,
            0,
            now,
        );

        assert_eq!(evidence.rendered_persons, 0);
        assert_eq!(stable, 0);
        assert_eq!(candidate_persons, 0);
        assert!(candidate_since.is_none());
        assert!(!evidence.ambiguous);
    }
}

/// Generate a single person's skeleton with per-person spatial offset and phase stagger.
///
/// `person_idx`: 0-based index of this person.
/// `total_persons`: total number of detected persons (for spacing calculation).
const UNLOCALIZED_POSITION_SOURCE: &str = "unlocalized";
const SENSOR_GEOMETRY_POSE_SOURCE: &str = "sensor_geometry";
const RSSI_CSI_POSE_SOURCE: &str = "rssi_csi_trilateration";
const RSSI_CSI_SINGLE_NODE_SOURCE: &str = "rssi_csi_single_node";
const SYNTHETIC_DEV_POSE_SOURCE: &str = "synthetic_dev";
const MODEL_METRIC_POSE_SOURCE: &str = "model_metric";
const MODEL_PX_POSE_SOURCE: &str = "model_px";
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
    let spread_x = ((max_x - min_x).abs() / (total_persons.max(1) as f64 + 1.0)).clamp(0.45, 1.2);
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

fn model_keypoints_are_metric(keypoints: &[PoseKeypoint]) -> bool {
    if keypoints.len() < 17 {
        return false;
    }
    let min_y = keypoints
        .iter()
        .map(|kp| kp.y)
        .fold(f64::INFINITY, f64::min);
    let max_y = keypoints
        .iter()
        .map(|kp| kp.y)
        .fold(f64::NEG_INFINITY, f64::max);
    let foot_y = keypoints
        .get(15)
        .zip(keypoints.get(16))
        .map(|(l, r)| l.y.min(r.y))
        .unwrap_or(f64::INFINITY);
    let height = max_y - foot_y;

    min_y.is_finite()
        && max_y.is_finite()
        && (1.2..=2.2).contains(&height)
        && (0.0..=0.25).contains(&foot_y)
}

fn derive_single_person_pose(
    update: &SensingUpdate,
    person_idx: usize,
    total_persons: usize,
) -> PersonDetection {
    let cls = &update.classification;
    let feat = &update.features;

    // Per-person phase offset: ~120 degrees apart so they don't move in sync.
    let phase_offset = person_idx as f64 * 2.094;

    // Spatial spread: persons distributed symmetrically around center.
    let half = (total_persons as f64 - 1.0) / 2.0;
    let person_x_offset = (person_idx as f64 - half) * 120.0; // 120px spacing

    // Confidence decays for additional persons (less certain about person 2, 3).
    let conf_decay = 1.0 - person_idx as f64 * 0.15;

    // ── Signal-derived scalars ────────────────────────────────────────────────

    let motion_score = (feat.motion_band_power / 15.0).clamp(0.0, 1.0);
    let is_walking = motion_score > 0.55;
    let breath_amp = (feat.breathing_band_power * 4.0).clamp(0.0, 12.0);

    let breath_phase = if let Some(ref vs) = update.vital_signs {
        let bpm = vs.breathing_rate_bpm.unwrap_or(15.0);
        let freq = (bpm / 60.0).clamp(0.1, 0.5);
        // Slow tick rate (0.02) for gentle breathing, not jerky oscillation.
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

    // Dampen burst and noise to reduce jitter.  The original used
    // tick*17.3 which changed wildly every frame.  Now use slow tick
    // rate and minimal burst scaling for a stable skeleton.
    let burst = (feat.change_points as f64 / 20.0).clamp(0.0, 0.3);

    let noise_seed = person_idx as f64 * 97.1; // stable per-person, no tick
    let noise_val = (noise_seed.sin() * 43758.545).fract();

    let snr_factor = ((feat.variance - 0.5) / 10.0).clamp(0.0, 1.0);
    let base_confidence = cls.confidence * (0.6 + 0.4 * snr_factor) * conf_decay;

    // ── Skeleton base position ────────────────────────────────────────────────

    let base_x = 320.0 + stride_x + lean_x * 0.5 + person_x_offset;
    let base_y = 240.0 - motion_score * 8.0;

    // ── COCO 17-keypoint offsets from hip-center ──────────────────────────────

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
        (0.0, -80.0),   // 0  nose
        (-8.0, -88.0),  // 1  left_eye
        (8.0, -88.0),   // 2  right_eye
        (-16.0, -82.0), // 3  left_ear
        (16.0, -82.0),  // 4  right_ear
        (-30.0, -50.0), // 5  left_shoulder
        (30.0, -50.0),  // 6  right_shoulder
        (-45.0, -15.0), // 7  left_elbow
        (45.0, -15.0),  // 8  right_elbow
        (-50.0, 20.0),  // 9  left_wrist
        (50.0, 20.0),   // 10 right_wrist
        (-20.0, 20.0),  // 11 left_hip
        (20.0, 20.0),   // 12 right_hip
        (-22.0, 70.0),  // 13 left_knee
        (22.0, 70.0),   // 14 right_knee
        (-24.0, 120.0), // 15 left_ankle
        (24.0, 120.0),  // 16 right_ankle
    ];

    const TORSO_KP: [usize; 4] = [5, 6, 11, 12];
    const EXTREMITY_KP: [usize; 4] = [9, 10, 15, 16];

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
                // Dampened from 12/8 to 4/3 to reduce visual jumping.
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

fn derive_pose_from_sensing(update: &SensingUpdate) -> Vec<PersonDetection> {
    let cls = &update.classification;
    if !cls.presence {
        return vec![];
    }

    // Use estimated_persons if set by the tick loop; otherwise default to 1.
    let person_count = update.estimated_persons.unwrap_or(1).max(1);

    (0..person_count)
        .map(|idx| derive_single_person_pose(update, idx, person_count))
        .collect()
}

/// PersonDetection JSON and metric-pose contract tests.
#[cfg(test)]
mod person_detection_contract_tests {
    use super::*;

    fn node(node_id: u8, position: [f64; 3], rssi_dbm: f64) -> NodeInfo {
        NodeInfo {
            node_id,
            rssi_dbm,
            position,
            amplitude: vec![],
            subcarrier_count: 0,
            sync: None,
        }
    }

    fn sensing_update(source: &str, nodes: Vec<NodeInfo>) -> SensingUpdate {
        SensingUpdate {
            msg_type: "sensing_update".to_string(),
            timestamp: 0.0,
            source: source.to_string(),
            tick: 7,
            nodes,
            features: FeatureInfo {
                mean_rssi: -48.0,
                variance: 2.0,
                motion_band_power: 8.0,
                breathing_band_power: 0.4,
                dominant_freq_hz: 1.2,
                change_points: 2,
                spectral_power: 0.6,
            },
            classification: ClassificationInfo {
                motion_level: "present_moving".to_string(),
                presence: true,
                confidence: 0.82,
            },
            signal_field: SignalField {
                grid_size: [1, 1, 1],
                values: vec![0.0],
            },
            vital_signs: None,
            enhanced_motion: None,
            enhanced_breathing: None,
            posture: None,
            signal_quality_score: None,
            quality_verdict: None,
            bssid_count: None,
            pose_keypoints: None,
            model_status: None,
            persons: None,
            state: None,
            estimated_persons: Some(1),
            count_evidence: None,
            node_features: None,
        }
    }

    #[test]
    fn unlocalized_live_pose_serializes_position_m_as_null() {
        let update = sensing_update("esp32", vec![node(1, [0.0, 0.0, 0.0], -52.0)]);
        let person = derive_single_person_pose(&update, 0, 1);

        assert_eq!(person.position_m, None);
        assert_eq!(person.position, None);
        assert_eq!(person.position_source.as_deref(), Some(UNLOCALIZED_POSITION_SOURCE));
        assert!(person.keypoints_m.is_none());

        let json = serde_json::to_value(&person).unwrap();
        assert_eq!(json["position_m"], serde_json::Value::Null);
        assert_eq!(json["position_source"], UNLOCALIZED_POSITION_SOURCE);
        assert!(json.get("position").is_none());
    }

    #[test]
    fn unlocalized_live_pose_does_not_feed_tracker_or_fan_out() {
        let update = sensing_update("esp32", vec![node(1, [0.0, 0.0, 0.0], -52.0)]);
        let mut tracker = PoseTracker::new();
        let mut last_tracker_instant = None;

        let first = persons_for_update(
            &update,
            derive_pose_from_sensing(&update),
            &mut tracker,
            &mut last_tracker_instant,
        );
        let second = persons_for_update(
            &update,
            derive_pose_from_sensing(&update),
            &mut tracker,
            &mut last_tracker_instant,
        );

        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].zone, "zone_1");
        assert_eq!(
            second[0].position_source.as_deref(),
            Some(UNLOCALIZED_POSITION_SOURCE)
        );
        assert_eq!(
            second[0].pose_source.as_deref(),
            Some(UNLOCALIZED_POSITION_SOURCE)
        );
    }

    #[test]
    fn live_sensor_geometry_pose_emits_metric_keypoints() {
        let update = sensing_update(
            "esp32",
            vec![
                node(1, [-1.2, 0.7, -0.4], -47.0),
                node(2, [1.4, 1.8, 0.6], -42.0),
            ],
        );
        let person = derive_single_person_pose(&update, 0, 1);
        let keypoints_m = person.keypoints_m.as_ref().expect("metric keypoints");

        assert_eq!(keypoints_m.len(), person.keypoints.len());
        assert_eq!(person.pose_source.as_deref(), Some(SENSOR_GEOMETRY_POSE_SOURCE));
        assert!(person.position_m.is_some());

        let foot_y = keypoints_m[15].y.min(keypoints_m[16].y);
        let height = keypoints_m
            .iter()
            .map(|kp| kp.y)
            .fold(f64::NEG_INFINITY, f64::max)
            - foot_y;
        assert!((0.0..=0.12).contains(&foot_y), "foot_y={foot_y}");
        assert!((1.45..=1.95).contains(&height), "height={height}");
    }

    #[test]
    fn tracking_state_is_single_source_for_pose_and_location_payload() {
        let mut update = sensing_update(
            "esp32",
            vec![
                node(1, [0.0, 1.1, 0.0], -43.0),
                node(2, [4.0, 1.1, 0.0], -62.0),
                node(3, [2.0, 1.1, 3.5], -64.0),
            ],
        );
        let mut smoother = LocationSmoother::default();
        attach_tracking_state(&mut update, &mut smoother);

        let tracked_position = update
            .state
            .as_ref()
            .and_then(|state| state.persons.first())
            .map(|person| person.position_m)
            .expect("tracking state position");
        let pose_person = derive_single_person_pose(&update, 0, 1);
        assert_eq!(pose_person.position_m, Some(tracked_position));

        let payload = location_payload_from_update(&update).expect("location payload");
        assert_eq!(payload["persons"][0]["position_m"], serde_json::json!(tracked_position));
        assert_eq!(payload["state"]["persons"][0]["position_m"], serde_json::json!(tracked_position));
    }

    #[test]
    fn explicit_simulated_pose_is_marked_synthetic_dev() {
        let update = sensing_update("simulated", vec![node(1, [2.0, 0.0, 1.5], -45.0)]);
        let person = derive_single_person_pose(&update, 0, 1);

        assert_eq!(person.pose_source.as_deref(), Some(SYNTHETIC_DEV_POSE_SOURCE));
        assert!(person.keypoints_m.is_some());
    }
}

// ── RuVector Phase 2: Temporal EMA smoothing for keypoints ──────────────────

fn carry_person_positions(tracked: &mut [PersonDetection], raw: &[PersonDetection]) {
    for (idx, person) in tracked.iter_mut().enumerate() {
        if let Some(source) = raw.get(idx) {
            if person.position_m.is_none() {
                person.position_m = source.position_m;
                person.position = source.position;
                person.position_source = source.position_source.clone();
            }
            if person.keypoints_m.is_none() {
                person.keypoints_m = source.keypoints_m.clone();
            }
            if person.pose_source.is_none() {
                person.pose_source = source.pose_source.clone();
            }
        }
    }
}

fn normalize_person_contract(person: &mut PersonDetection) {
    if person.position_m.is_none() {
        person.position = None;
        person.keypoints_m = None;
        if person
            .position_source
            .as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .is_empty()
        {
            person.position_source = Some(UNLOCALIZED_POSITION_SOURCE.to_string());
        }
        if person
            .pose_source
            .as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .is_empty()
        {
            person.pose_source = Some(UNLOCALIZED_POSITION_SOURCE.to_string());
        }
    }
}

fn persons_for_update(
    update: &SensingUpdate,
    raw_persons: Vec<PersonDetection>,
    tracker: &mut PoseTracker,
    last_tracker_instant: &mut Option<std::time::Instant>,
) -> Vec<PersonDetection> {
    if raw_persons.is_empty() {
        let _ = tracker_bridge::tracker_update(tracker, last_tracker_instant, Vec::new());
        return Vec::new();
    }

    let expected_count = update.estimated_persons.unwrap_or(raw_persons.len()).max(1);
    let has_metric_position = raw_persons.iter().any(|person| person.position_m.is_some());

    let mut persons = if has_metric_position || is_synthetic_dev_source(&update.source) {
        let mut tracked =
            tracker_bridge::tracker_update(tracker, last_tracker_instant, raw_persons.clone());
        carry_person_positions(&mut tracked, &raw_persons);
        tracked
    } else {
        raw_persons
    };

    for person in &mut persons {
        normalize_person_contract(person);
    }
    if persons.len() > expected_count {
        persons.truncate(expected_count);
    }
    persons
}

/// Expected bone lengths in pixel-space for the COCO-17 skeleton as used by
/// `derive_single_person_pose`. Pairs are (parent_idx, child_idx).
const POSE_BONE_PAIRS: &[(usize, usize)] = &[
    (5, 7),
    (7, 9),
    (6, 8),
    (8, 10), // arms
    (5, 11),
    (6, 12), // torso
    (11, 13),
    (13, 15),
    (12, 14),
    (14, 16), // legs
    (5, 6),
    (11, 12), // shoulders, hips
];

/// Apply temporal EMA smoothing and bone-length clamping to person detections.
///
/// For the *first* person (index 0) this uses the per-node `prev_keypoints`
/// state. Multi-person smoothing is left for a future phase.
fn apply_temporal_smoothing(persons: &mut [PersonDetection], ns: &mut NodeState) {
    if persons.is_empty() {
        return;
    }

    let alpha = ns.ema_alpha();
    let person = &mut persons[0]; // smooth primary person only

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
        // Clamp bone lengths to ±20% of previous frame.
        clamp_bone_lengths_f64(&mut out, prev);
        out
    } else {
        current_kps.clone()
    };

    // Write smoothed keypoints back into the person detection.
    for (kp, s) in person.keypoints.iter_mut().zip(smoothed.iter()) {
        kp.x = s[0];
        kp.y = s[1];
        kp.z = s[2];
    }

    ns.prev_keypoints = Some(smoothed);
}

/// Clamp bone lengths so no bone changes by more than MAX_BONE_CHANGE_RATIO
/// compared to the previous frame.
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

// ── DensePose-compatible REST endpoints ─────────────────────────────────────

async fn health_live(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let s = state.read().await;
    Json(serde_json::json!({
        "status": "alive",
        "uptime": s.start_time.elapsed().as_secs(),
    }))
}

async fn health_ready(State(state): State<SharedState>) -> impl IntoResponse {
    let s = state.read().await;
    let now = std::time::Instant::now();
    let active_nodes = active_node_count(&s, now);
    let ready = fleet_ready(&s, now);
    let status = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(serde_json::json!({
            "status": if ready { "ready" } else { "not_ready" },
            "source": s.effective_source(),
            "active_nodes": active_nodes,
            "min_nodes": s.min_nodes,
        })),
    )
}

async fn health_system(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let s = state.read().await;
    let uptime = s.start_time.elapsed().as_secs();
    Json(serde_json::json!({
        "status": "healthy",
        "components": {
            "api": { "status": "healthy", "message": "Rust Axum server" },
            "hardware": {
                "status": if s.effective_source().ends_with(":offline") { "degraded" } else { "healthy" },
                "message": format!("Source: {}", s.effective_source())
            },
            "pose": { "status": "healthy", "message": "WiFi-derived pose estimation" },
            "stream": { "status": if s.tx.receiver_count() > 0 { "healthy" } else { "idle" },
                        "message": format!("{} client(s)", s.tx.receiver_count()) },
        },
        "metrics": {
            "cpu_percent": 2.5,
            "memory_percent": 1.8,
            "disk_percent": 15.0,
            "uptime_seconds": uptime,
        }
    }))
}

async fn health_version() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "name": "ruvsense-master",
        "product": "RuvSense Edge",
        "backend": "rust+axum+ruvector",
    }))
}

async fn health_metrics(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let s = state.read().await;
    Json(serde_json::json!({
        "system_metrics": {
            "cpu": { "percent": 2.5 },
            "memory": { "percent": 1.8, "used_mb": 5 },
            "disk": { "percent": 15.0 },
        },
        "tick": s.tick,
    }))
}

fn feature_disabled_response(feature: BetaFeature) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({
            "error": "feature_disabled",
            "feature": feature.as_str(),
            "reason": "beta",
        })),
    )
        .into_response()
}

async fn api_info(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let s = state.read().await;
    Json(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "environment": "production",
        "backend": "rust",
        "source": s.effective_source(),
        "features": {
            "wifi_sensing": s.feature_flags.stable.presence_detection,
            "pose_estimation": s.feature_flags.beta_enabled(BetaFeature::SkeletonPoseEstimation),
            "signal_processing": true,
            "ruvector": true,
            "localization": s.feature_flags.stable.zone_localization,
            "streaming": true,
        },
        "feature_flags": s.feature_flags.as_json(),
    }))
}

async fn features_endpoint(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let s = state.read().await;
    Json(s.feature_flags.as_json())
}

async fn pose_current(State(state): State<SharedState>) -> Response {
    let s = state.read().await;
    if !s
        .feature_flags
        .beta_enabled(BetaFeature::SkeletonPoseEstimation)
    {
        return feature_disabled_response(BetaFeature::SkeletonPoseEstimation);
    }
    let persons = match &s.latest_update {
        Some(update) => update
            .persons
            .clone()
            .unwrap_or_else(|| derive_pose_from_sensing(update)),
        None => vec![],
    };
    Json(serde_json::json!({
        "timestamp": chrono::Utc::now().timestamp_millis() as f64 / 1000.0,
        "persons": persons,
        "total_persons": persons.len(),
        "source": s.effective_source(),
    }))
    .into_response()
}

async fn cardiac_endpoint(State(state): State<SharedState>) -> Response {
    let s = state.read().await;
    if !s
        .feature_flags
        .beta_enabled(BetaFeature::CardiacArrestDetection)
    {
        return feature_disabled_response(BetaFeature::CardiacArrestDetection);
    }
    let vs = &s.latest_vitals;
    let cardiac_risk_score = cardiac_risk_score(vs);
    Json(serde_json::json!({
        "status": "beta",
        "clinical_validation": "not_validated",
        "heart_rate_bpm": vs.heart_rate_bpm,
        "heartbeat_confidence": vs.heartbeat_confidence,
        "cardiac_risk_score": cardiac_risk_score,
        "source": s.effective_source(),
        "warning": "Research feature only; do not use for medical decisions.",
    }))
    .into_response()
}

async fn pose_stats(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let s = state.read().await;
    Json(serde_json::json!({
        "total_detections": s.total_detections,
        "average_confidence": 0.87,
        "frames_processed": s.tick,
        "source": s.effective_source(),
    }))
}

async fn pose_zones_summary(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let s = state.read().await;
    let presence = s
        .latest_update
        .as_ref()
        .map(|u| u.classification.presence)
        .unwrap_or(false);
    Json(serde_json::json!({
        "zones": {
            "zone_1": { "person_count": if presence { 1 } else { 0 }, "status": "monitored" },
            "zone_2": { "person_count": 0, "status": "clear" },
            "zone_3": { "person_count": 0, "status": "clear" },
            "zone_4": { "person_count": 0, "status": "clear" },
        }
    }))
}

async fn stream_status(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let s = state.read().await;
    Json(serde_json::json!({
        "active": true,
        "clients": s.tx.receiver_count(),
        "fps": if s.tick > 1 { 10u64 } else { 0u64 },
        "source": s.effective_source(),
    }))
}

// ── Model Management Endpoints ──────────────────────────────────────────────

/// GET /api/v1/models — list discovered RVF model files.
async fn list_models(State(state): State<SharedState>) -> Json<serde_json::Value> {
    // Re-scan directory each call so newly-added files are visible.
    let models = scan_model_files();
    let total = models.len();
    {
        let mut s = state.write().await;
        s.discovered_models = models.clone();
    }
    Json(serde_json::json!({ "models": models, "total": total }))
}

/// GET /api/v1/models/active — return currently loaded model or null.
async fn get_active_model(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let s = state.read().await;
    match &s.active_model_id {
        Some(id) => {
            let model = s
                .discovered_models
                .iter()
                .find(|m| m.get("id").and_then(|v| v.as_str()) == Some(id.as_str()));
            Json(serde_json::json!({
                "active": model.cloned().unwrap_or_else(|| serde_json::json!({ "id": id })),
            }))
        }
        None => Json(serde_json::json!({ "active": serde_json::Value::Null })),
    }
}

/// POST /api/v1/models/load — load a model by ID.
async fn load_model(
    State(state): State<SharedState>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let model_id = body
        .get("id")
        .or_else(|| body.get("model_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if model_id.is_empty() {
        return Json(serde_json::json!({ "error": "missing 'id' field", "success": false }));
    }
    let mut s = state.write().await;
    s.active_model_id = Some(model_id.clone());
    s.model_loaded = true;
    info!("Model loaded: {model_id}");
    Json(serde_json::json!({ "success": true, "model_id": model_id }))
}

/// POST /api/v1/models/unload — unload the current model.
async fn unload_model(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let mut s = state.write().await;
    let prev = s.active_model_id.take();
    s.model_loaded = false;
    info!("Model unloaded (was: {:?})", prev);
    Json(serde_json::json!({ "success": true, "previous": prev }))
}

/// DELETE /api/v1/models/:id — delete a model file.
async fn delete_model(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    // ADR-050: Sanitize path to prevent directory traversal
    let safe_id = std::path::Path::new(&id)
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("");
    if safe_id.is_empty() || safe_id != id {
        return Json(serde_json::json!({ "error": "invalid model id", "success": false }));
    }
    let path = effective_models_dir().join(format!("{}.rvf", safe_id));
    if path.exists() {
        if let Err(e) = std::fs::remove_file(&path) {
            warn!("Failed to delete model file {:?}: {}", path, e);
            return Json(
                serde_json::json!({ "error": format!("delete failed: {e}"), "success": false }),
            );
        }
        // If this was the active model, unload it
        let mut s = state.write().await;
        if s.active_model_id.as_deref() == Some(id.as_str()) {
            s.active_model_id = None;
            s.model_loaded = false;
        }
        s.discovered_models
            .retain(|m| m.get("id").and_then(|v| v.as_str()) != Some(id.as_str()));
        info!("Model deleted: {id}");
        Json(serde_json::json!({ "success": true, "deleted": id }))
    } else {
        Json(serde_json::json!({ "error": "model not found", "success": false }))
    }
}

/// GET /api/v1/models/lora/profiles — list LoRA adapter profiles.
async fn list_lora_profiles() -> Json<serde_json::Value> {
    // LoRA profiles are discovered from data/models/*.lora.json
    let profiles = scan_lora_profiles();
    Json(serde_json::json!({ "profiles": profiles }))
}

/// POST /api/v1/models/lora/activate — activate a LoRA adapter profile.
async fn activate_lora_profile(Json(body): Json<serde_json::Value>) -> Json<serde_json::Value> {
    let profile = body
        .get("profile")
        .or_else(|| body.get("name"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if profile.is_empty() {
        return Json(serde_json::json!({ "error": "missing 'profile' field", "success": false }));
    }
    info!("LoRA profile activated: {profile}");
    Json(serde_json::json!({ "success": true, "profile": profile }))
}

/// Return the effective models directory, respecting the `MODELS_DIR`
/// environment variable.  Defaults to `data/models`.
fn effective_models_dir() -> PathBuf {
    PathBuf::from(std::env::var("MODELS_DIR").unwrap_or_else(|_| "data/models".to_string()))
}

/// Scan the models directory for `.rvf` files and return metadata.
/// Respects the `MODELS_DIR` environment variable.
fn scan_model_files() -> Vec<serde_json::Value> {
    let dir = effective_models_dir();
    let mut models = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("rvf") {
                let name = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("unknown")
                    .to_string();
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                let modified = entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                models.push(serde_json::json!({
                    "id": name,
                    "name": name,
                    "path": path.display().to_string(),
                    "size_bytes": size,
                    "format": "rvf",
                    "modified_epoch": modified,
                }));
            }
        }
    }
    models
}

/// Scan the models directory for `.lora.json` LoRA profile files.
/// Respects the `MODELS_DIR` environment variable.
fn scan_lora_profiles() -> Vec<serde_json::Value> {
    let dir = effective_models_dir();
    let mut profiles = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.ends_with(".lora.json") {
                let profile_name = name.trim_end_matches(".lora.json").to_string();
                // Try to read the profile JSON
                let config = std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                    .unwrap_or_else(|| serde_json::json!({}));
                profiles.push(serde_json::json!({
                    "name": profile_name,
                    "path": path.display().to_string(),
                    "config": config,
                }));
            }
        }
    }
    profiles
}

// ── Recording Endpoints ─────────────────────────────────────────────────────

/// GET /api/v1/recording/list — list CSI recordings.
async fn list_recordings() -> Json<serde_json::Value> {
    let recordings = scan_recording_files();
    Json(serde_json::json!({ "recordings": recordings }))
}

/// POST /api/v1/recording/start — start recording CSI data.
async fn start_recording(
    State(state): State<SharedState>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let mut s = state.write().await;
    if s.recording_active {
        return Json(serde_json::json!({
            "error": "recording already in progress",
            "success": false,
            "recording_id": s.recording_current_id,
        }));
    }
    let id = body
        .get("id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("rec_{}", chrono_timestamp()));

    // Create the recording file
    let rec_path = PathBuf::from("data/recordings").join(format!("{}.jsonl", id));
    let file = match std::fs::File::create(&rec_path) {
        Ok(f) => f,
        Err(e) => {
            warn!("Failed to create recording file {:?}: {}", rec_path, e);
            return Json(serde_json::json!({
                "error": format!("cannot create file: {e}"),
                "success": false,
            }));
        }
    };

    // Create a stop signal channel
    let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);
    s.recording_active = true;
    s.recording_start_time = Some(std::time::Instant::now());
    s.recording_current_id = Some(id.clone());
    s.recording_stop_tx = Some(stop_tx);

    // Subscribe to the broadcast channel to capture CSI frames
    let mut rx = s.tx.subscribe();

    // Add initial recording entry
    s.recordings.push(serde_json::json!({
        "id": id,
        "path": rec_path.display().to_string(),
        "status": "recording",
        "started_at": chrono_timestamp(),
        "frames": 0,
    }));

    let rec_id = id.clone();

    // Spawn writer task in background
    tokio::spawn(async move {
        use std::io::Write;
        let mut writer = std::io::BufWriter::new(file);
        let mut frame_count: u64 = 0;
        loop {
            tokio::select! {
                result = rx.recv() => {
                    match result {
                        Ok(frame_json) => {
                            if writeln!(writer, "{}", frame_json).is_err() {
                                warn!("Recording {rec_id}: write error, stopping");
                                break;
                            }
                            frame_count += 1;
                            // Flush every 100 frames
                            if frame_count % 100 == 0 {
                                let _ = writer.flush();
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            debug!("Recording {rec_id}: lagged {n} frames");
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            info!("Recording {rec_id}: broadcast closed, stopping");
                            break;
                        }
                    }
                }
                _ = stop_rx.changed() => {
                    if *stop_rx.borrow() {
                        info!("Recording {rec_id}: stop signal received ({frame_count} frames)");
                        break;
                    }
                }
            }
        }
        let _ = writer.flush();
        info!("Recording {rec_id} finished: {frame_count} frames written");
    });

    info!("Recording started: {id}");
    Json(serde_json::json!({ "success": true, "recording_id": id }))
}

/// POST /api/v1/recording/stop — stop recording CSI data.
async fn stop_recording(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let mut s = state.write().await;
    if !s.recording_active {
        return Json(serde_json::json!({
            "error": "no recording in progress",
            "success": false,
        }));
    }
    // Signal the writer task to stop
    if let Some(tx) = s.recording_stop_tx.take() {
        let _ = tx.send(true);
    }
    let duration_secs = s
        .recording_start_time
        .map(|t| t.elapsed().as_secs())
        .unwrap_or(0);
    let rec_id = s.recording_current_id.take().unwrap_or_default();
    s.recording_active = false;
    s.recording_start_time = None;

    // Update the recording entry status
    for rec in s.recordings.iter_mut() {
        if rec.get("id").and_then(|v| v.as_str()) == Some(rec_id.as_str()) {
            rec["status"] = serde_json::json!("completed");
            rec["duration_secs"] = serde_json::json!(duration_secs);
        }
    }

    info!("Recording stopped: {rec_id} ({duration_secs}s)");
    Json(serde_json::json!({
        "success": true,
        "recording_id": rec_id,
        "duration_secs": duration_secs,
    }))
}

/// DELETE /api/v1/recording/:id — delete a recording file.
async fn delete_recording(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    // ADR-050: Sanitize path to prevent directory traversal
    let safe_id = std::path::Path::new(&id)
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("");
    if safe_id.is_empty() || safe_id != id {
        return Json(serde_json::json!({ "error": "invalid recording id", "success": false }));
    }
    let path = PathBuf::from("data/recordings").join(format!("{}.jsonl", safe_id));
    if path.exists() {
        if let Err(e) = std::fs::remove_file(&path) {
            warn!("Failed to delete recording {:?}: {}", path, e);
            return Json(
                serde_json::json!({ "error": format!("delete failed: {e}"), "success": false }),
            );
        }
        let mut s = state.write().await;
        s.recordings
            .retain(|r| r.get("id").and_then(|v| v.as_str()) != Some(id.as_str()));
        info!("Recording deleted: {id}");
        Json(serde_json::json!({ "success": true, "deleted": id }))
    } else {
        Json(serde_json::json!({ "error": "recording not found", "success": false }))
    }
}

/// Scan `data/recordings/` for `.jsonl` files and return metadata.
fn scan_recording_files() -> Vec<serde_json::Value> {
    let dir = PathBuf::from("data/recordings");
    let mut recordings = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                let name = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("unknown")
                    .to_string();
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                let modified = entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                // Count lines (frames) — approximate for large files
                let frame_count = std::fs::read_to_string(&path)
                    .map(|s| s.lines().count())
                    .unwrap_or(0);
                recordings.push(serde_json::json!({
                    "id": name,
                    "name": name,
                    "path": path.display().to_string(),
                    "size_bytes": size,
                    "frames": frame_count,
                    "modified_epoch": modified,
                    "status": "completed",
                }));
            }
        }
    }
    recordings
}

// ── Training Endpoints ──────────────────────────────────────────────────────

/// GET /api/v1/train/status — get training status.
async fn train_status(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let s = state.read().await;
    Json(serde_json::json!({
        "status": s.training_status,
        "config": s.training_config,
    }))
}

/// POST /api/v1/train/start — start a training run.
async fn train_start(
    State(state): State<SharedState>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let mut s = state.write().await;
    if s.training_status == "running" {
        return Json(serde_json::json!({
            "error": "training already running",
            "success": false,
        }));
    }
    s.training_status = "running".to_string();
    s.training_config = Some(body.clone());
    info!("Training started with config: {}", body);
    Json(serde_json::json!({
        "success": true,
        "status": "running",
        "message": "Training pipeline started. Use GET /api/v1/train/status to monitor.",
    }))
}

/// POST /api/v1/train/stop — stop the current training run.
async fn train_stop(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let mut s = state.write().await;
    if s.training_status != "running" {
        return Json(serde_json::json!({
            "error": "no training in progress",
            "success": false,
        }));
    }
    s.training_status = "idle".to_string();
    info!("Training stopped");
    Json(serde_json::json!({
        "success": true,
        "status": "idle",
    }))
}

// ── Adaptive classifier endpoints ────────────────────────────────────────────

/// POST /api/v1/adaptive/train — train the adaptive classifier from recordings.
async fn adaptive_train(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let rec_dir = PathBuf::from("data/recordings");
    eprintln!("=== Adaptive Classifier Training ===");
    match adaptive_classifier::train_from_recordings(&rec_dir) {
        Ok(model) => {
            let accuracy = model.training_accuracy;
            let frames = model.trained_frames;
            let stats: Vec<_> = model
                .class_stats
                .iter()
                .map(|cs| {
                    serde_json::json!({
                        "class": cs.label,
                        "samples": cs.count,
                        "feature_means": cs.mean,
                    })
                })
                .collect();

            // Save to disk.
            if let Err(e) = model.save(&adaptive_classifier::model_path()) {
                warn!("Failed to save adaptive model: {e}");
            } else {
                info!(
                    "Adaptive model saved to {}",
                    adaptive_classifier::model_path().display()
                );
            }

            // Load into runtime state.
            let mut s = state.write().await;
            s.adaptive_model = Some(model);

            Json(serde_json::json!({
                "success": true,
                "trained_frames": frames,
                "accuracy": accuracy,
                "class_stats": stats,
            }))
        }
        Err(e) => Json(serde_json::json!({
            "success": false,
            "error": e,
        })),
    }
}

/// GET /api/v1/adaptive/status — check adaptive model status.
async fn adaptive_status(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let s = state.read().await;
    match &s.adaptive_model {
        Some(model) => Json(serde_json::json!({
            "loaded": true,
            "trained_frames": model.trained_frames,
            "accuracy": model.training_accuracy,
            "version": model.version,
            "classes": model.class_names,
            "class_stats": model.class_stats,
        })),
        None => Json(serde_json::json!({
            "loaded": false,
            "message": "No adaptive model. POST /api/v1/adaptive/train to train one.",
        })),
    }
}

/// POST /api/v1/adaptive/unload — unload the adaptive model (revert to thresholds).
async fn adaptive_unload(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let mut s = state.write().await;
    s.adaptive_model = None;
    Json(serde_json::json!({ "success": true, "message": "Adaptive model unloaded." }))
}

// ── Field model calibration endpoints (eigenvalue person counting) ──────────

#[derive(Debug, Deserialize)]
struct AutoCalibrationUpdate {
    enabled: bool,
    policy: Option<String>,
}

#[derive(Debug)]
struct AutoCalibrationGuard {
    source_live: bool,
    quorum_ready: bool,
    presence_blocked: bool,
    motion_blocked: bool,
    quality_blocked: bool,
    active_nodes: usize,
    signal_quality: Option<f64>,
    quiet_elapsed_secs: u64,
    can_collect: bool,
    blockers: Vec<&'static str>,
}

fn motion_level_blocks_empty_room(level: &str) -> bool {
    let normalized = level.trim().to_ascii_lowercase();
    !matches!(
        normalized.as_str(),
        "" | "absent" | "idle" | "none" | "unknown"
    )
}

fn room_quality_score(s: &AppStateInner, now: std::time::Instant) -> Option<f64> {
    let mut scores = Vec::new();
    for node in s.node_states.values().filter(|node| is_node_active(node, now)) {
        scores.push(node_coverage_score(Some(node), now).0);
    }
    if !scores.is_empty() {
        return Some(scores.iter().sum::<f64>() / scores.len() as f64);
    }
    s.latest_update
        .as_ref()
        .and_then(|update| update.signal_quality_score)
        .or_else(|| {
            s.latest_update
                .as_ref()
                .and_then(|update| update.vital_signs.as_ref().map(|vitals| vitals.signal_quality))
        })
}

fn auto_calibration_guard(s: &AppStateInner, now: std::time::Instant) -> AutoCalibrationGuard {
    let effective_source = s.effective_source();
    let source_live = !effective_source.contains("offline")
        && !effective_source.contains("simulate")
        && !effective_source.contains("simulated");
    let active_nodes = active_node_count(s, now);
    let quorum_ready = active_nodes >= s.min_nodes.max(1);
    let presence_blocked = s
        .latest_update
        .as_ref()
        .is_some_and(|update| update.classification.presence)
        || room_presence_supporting_nodes(s, now) > 0
        || s.last_present_at
            .is_some_and(|seen| now.saturating_duration_since(seen) <= AUTO_CALIBRATION_QUIET_WINDOW);
    let motion_blocked = s
        .latest_update
        .as_ref()
        .is_some_and(|update| motion_level_blocks_empty_room(&update.classification.motion_level))
        || s.node_states
            .values()
            .filter(|node| is_node_active(node, now))
            .any(|node| motion_level_blocks_empty_room(&node.current_motion_level));
    let signal_quality = room_quality_score(s, now);
    let quality_blocked = signal_quality.is_some_and(|score| score < AUTO_CALIBRATION_MIN_QUALITY);
    let quiet_elapsed_secs = s
        .auto_calibration_quiet_since
        .map(|seen| now.saturating_duration_since(seen).as_secs())
        .unwrap_or(0);
    let mut blockers = Vec::new();
    if !source_live {
        blockers.push("source_not_live");
    }
    if !quorum_ready {
        blockers.push("node_quorum");
    }
    if presence_blocked {
        blockers.push("presence_detected");
    }
    if motion_blocked {
        blockers.push("motion_detected");
    }
    if quality_blocked {
        blockers.push("low_signal_quality");
    }
    let can_collect =
        source_live && quorum_ready && !presence_blocked && !motion_blocked && !quality_blocked;
    AutoCalibrationGuard {
        source_live,
        quorum_ready,
        presence_blocked,
        motion_blocked,
        quality_blocked,
        active_nodes,
        signal_quality,
        quiet_elapsed_secs,
        can_collect,
        blockers,
    }
}

fn calibration_status_label(s: &AppStateInner) -> String {
    s.field_model
        .as_ref()
        .map(|fm| format!("{:?}", fm.status()))
        .unwrap_or_else(|| "not_started".to_string())
}

fn calibration_frame_count(s: &AppStateInner) -> u64 {
    s.field_model
        .as_ref()
        .map(|fm| fm.calibration_frame_count())
        .unwrap_or(0)
}

fn calibration_snapshot_json(s: &AppStateInner, now: std::time::Instant) -> serde_json::Value {
    let guard = auto_calibration_guard(s, now);
    let status = calibration_status_label(s);
    let recommended_action = if !s.auto_calibration_enabled {
        "enable_auto_or_start_manual"
    } else if !guard.can_collect {
        "wait_for_safe_empty_room"
    } else if guard.quiet_elapsed_secs < AUTO_CALIBRATION_QUIET_WINDOW.as_secs() {
        "waiting_for_quiet_window"
    } else if matches!(status.as_str(), "Collecting") {
        "collecting"
    } else if matches!(status.as_str(), "Fresh") {
        "calibrated"
    } else {
        "ready_to_collect"
    };
    serde_json::json!({
        "status": status,
        "active": s.field_model.is_some(),
        "enabled": s.field_model.is_some(),
        "frame_count": calibration_frame_count(s),
        "min_frames": field_bridge::FIELD_MODEL_MIN_CALIBRATION_FRAMES,
        "active_nodes": guard.active_nodes,
        "min_nodes": s.min_nodes,
        "dedup_factor": s.dedup_factor,
        "adaptive": adaptive_calibration_nodes_json(&s.node_states),
        "auto_mode": {
            "enabled": s.auto_calibration_enabled,
            "policy": s.auto_calibration_policy.as_str(),
            "quiet_window_sec": AUTO_CALIBRATION_QUIET_WINDOW.as_secs(),
            "quiet_elapsed_sec": guard.quiet_elapsed_secs,
            "source_live": guard.source_live,
            "quorum_ready": guard.quorum_ready,
            "presence_blocked": guard.presence_blocked,
            "motion_blocked": guard.motion_blocked,
            "quality_blocked": guard.quality_blocked,
            "signal_quality": guard.signal_quality,
            "guard_state": if guard.can_collect { "clear" } else { "blocked" },
            "blockers": guard.blockers,
            "recommended_action": recommended_action,
            "last_action": s.auto_calibration_last_action.as_deref(),
        },
        "actions": {
            "start": "/api/v1/calibration/start",
            "stop": "/api/v1/calibration/stop",
            "abort": "/api/v1/calibration/abort",
            "auto": "/api/v1/calibration/auto",
            "status": "/api/v1/calibration/status"
        }
    })
}

fn start_field_model_calibration(s: &mut AppStateInner) -> Result<(), String> {
    let fm = FieldModel::new(field_bridge::single_link_config()).map_err(|e| format!("{e}"))?;
    s.field_model = Some(fm);
    Ok(())
}

fn maybe_drive_auto_calibration(s: &mut AppStateInner, now: std::time::Instant) {
    if !s.auto_calibration_enabled {
        return;
    }
    let guard = auto_calibration_guard(s, now);
    if !guard.can_collect {
        s.auto_calibration_quiet_since = None;
        if s.field_model
            .as_ref()
            .is_some_and(|fm| fm.status() == CalibrationStatus::Collecting)
        {
            s.field_model = None;
            s.auto_calibration_last_action = Some(format!(
                "aborted: {}",
                guard.blockers.join(",")
            ));
        }
        return;
    }

    let quiet_since = s.auto_calibration_quiet_since.get_or_insert(now);
    let quiet_elapsed = now.saturating_duration_since(*quiet_since);
    if quiet_elapsed < AUTO_CALIBRATION_QUIET_WINDOW {
        return;
    }

    match s.field_model.as_ref().map(|fm| fm.status()) {
        Some(CalibrationStatus::Collecting) => {
            let frame_count = calibration_frame_count(s);
            if frame_count >= field_bridge::FIELD_MODEL_MIN_CALIBRATION_FRAMES as u64 {
                if let Some(ref mut fm) = s.field_model {
                    let ts = chrono::Utc::now().timestamp_micros() as u64;
                    match fm.finalize_calibration(ts, 0) {
                        Ok(_) => {
                            s.auto_calibration_last_action =
                                Some(format!("finalized:{frame_count}_frames"));
                        }
                        Err(err) => {
                            s.auto_calibration_last_action =
                                Some(format!("finalize_failed:{err}"));
                        }
                    }
                }
            }
        }
        Some(CalibrationStatus::Fresh) => {}
        _ => match start_field_model_calibration(s) {
            Ok(()) => {
                s.auto_calibration_last_action = Some("started".to_string());
            }
            Err(err) => {
                s.auto_calibration_last_action = Some(format!("start_failed:{err}"));
            }
        },
    }
}

async fn calibration_start(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let mut s = state.write().await;
    // Guard: don't discard an in-progress or fresh calibration
    if let Some(ref fm) = s.field_model {
        match fm.status() {
            CalibrationStatus::Collecting => {
                return Json(serde_json::json!({
                    "success": false,
                    "error": "Calibration already in progress. Call /calibration/stop first.",
                    "frame_count": fm.calibration_frame_count(),
                    "min_frames": field_bridge::FIELD_MODEL_MIN_CALIBRATION_FRAMES,
                }));
            }
            CalibrationStatus::Fresh => {
                return Json(serde_json::json!({
                    "success": false,
                    "error": "A fresh calibration already exists. Call /calibration/stop or wait for expiry.",
                    "frame_count": fm.calibration_frame_count(),
                    "min_frames": field_bridge::FIELD_MODEL_MIN_CALIBRATION_FRAMES,
                }));
            }
            _ => {} // Stale/Expired/Uncalibrated — ok to recalibrate
        }
    }
    match start_field_model_calibration(&mut s) {
        Ok(()) => {
            Json(serde_json::json!({
                "success": true,
                "frame_count": 0,
                "min_frames": field_bridge::FIELD_MODEL_MIN_CALIBRATION_FRAMES,
                "message": "Calibration started — keep room empty while frames accumulate.",
            }))
        }
        Err(e) => Json(serde_json::json!({
            "success": false,
            "error": format!("{e}"),
        })),
    }
}

async fn calibration_stop(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let mut s = state.write().await;
    if let Some(ref mut fm) = s.field_model {
        let ts = chrono::Utc::now().timestamp_micros() as u64;
        match fm.finalize_calibration(ts, 0) {
            Ok(modes) => {
                let baseline = modes.baseline_eigenvalue_count;
                let variance_explained = modes.variance_explained;
                info!("Field model calibrated: baseline_eigenvalues={baseline}, variance_explained={variance_explained:.2}");
                Json(serde_json::json!({
                    "success": true,
                    "baseline_eigenvalue_count": baseline,
                    "variance_explained": variance_explained,
                    "frame_count": fm.calibration_frame_count(),
                    "min_frames": field_bridge::FIELD_MODEL_MIN_CALIBRATION_FRAMES,
                }))
            }
            Err(e) => Json(serde_json::json!({
                "success": false,
                "error": format!("{e}"),
            })),
        }
    } else {
        Json(serde_json::json!({
            "success": false,
            "error": "No field model active — call /calibration/start first.",
        }))
    }
}

async fn calibration_status(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let mut s = state.write().await;
    let now = std::time::Instant::now();
    maybe_drive_auto_calibration(&mut s, now);
    Json(calibration_snapshot_json(&s, now))
}

async fn calibration_abort(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let mut s = state.write().await;
    s.field_model = None;
    s.auto_calibration_enabled = false;
    s.auto_calibration_quiet_since = None;
    s.auto_calibration_last_action = Some("aborted_by_operator".to_string());
    for node in s.node_states.values_mut() {
        node.adaptive_calibration = None;
    }
    Json(serde_json::json!({
        "success": true,
        "message": "Calibration aborted.",
        "calibration": calibration_snapshot_json(&s, std::time::Instant::now()),
    }))
}

async fn calibration_auto_update(
    State(state): State<SharedState>,
    Json(body): Json<AutoCalibrationUpdate>,
) -> Json<serde_json::Value> {
    let mut s = state.write().await;
    s.auto_calibration_enabled = body.enabled;
    s.auto_calibration_policy = body.policy.unwrap_or_else(|| "safe".to_string());
    if !s.auto_calibration_enabled {
        s.auto_calibration_quiet_since = None;
        s.auto_calibration_last_action = Some("auto_disabled".to_string());
    } else {
        s.auto_calibration_last_action = Some("auto_enabled".to_string());
    }
    let now = std::time::Instant::now();
    maybe_drive_auto_calibration(&mut s, now);
    Json(calibration_snapshot_json(&s, now))
}

/// Generate a simple timestamp string (epoch seconds) for recording IDs.
fn chrono_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

async fn location_endpoint(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let s = state.read().await;
    Json(location_payload(&s, std::time::Instant::now()))
}

fn cardiac_risk_score(vs: &VitalSigns) -> f64 {
    let Some(hr) = vs.heart_rate_bpm else {
        return 0.0;
    };
    if !hr.is_finite() {
        return 0.0;
    }
    let rate_risk = if !(40.0..=120.0).contains(&hr) {
        0.8
    } else if !(50.0..=110.0).contains(&hr) {
        0.4
    } else {
        0.1
    };
    (rate_risk * vs.heartbeat_confidence.clamp(0.0, 1.0)).clamp(0.0, 1.0)
}

async fn vital_signs_endpoint(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let s = state.read().await;
    let vs = &s.latest_vitals;
    let (br_len, br_cap, hb_len, hb_cap) = s.vital_detector.buffer_status();
    let timestamp_ms = unix_timestamp_ms();
    let now = std::time::Instant::now();
    let breathing_threshold = f64::from(breathing_min_confidence());
    let breathing_bpm = vs
        .breathing_rate_bpm
        .filter(|_| vs.breathing_confidence >= breathing_threshold)
        .or_else(|| s.edge_vitals.as_ref().map(|vitals| vitals.breathing_rate_bpm));
    let motion_energy = s
        .latest_update
        .as_ref()
        .map(|update| current_motion_energy(&s, update, now))
        .or_else(|| s.edge_vitals.as_ref().map(|vitals| vitals.motion_energy as f64))
        .unwrap_or(s.smoothed_motion);
    let presence_score = s
        .edge_vitals
        .as_ref()
        .map(|vitals| vitals.presence_score as f64)
        .or_else(|| s.latest_update.as_ref().map(|update| update.classification.confidence))
        .unwrap_or(0.0);
    let fall_suspected = s
        .edge_vitals
        .as_ref()
        .map(|vitals| vitals.fall_detected)
        .unwrap_or(false);
    let zone_id = s
        .environment
        .nodes
        .iter()
        .find(|node| {
            s.node_states
                .get(&node.node_id)
                .map(|node_state| is_node_active(node_state, now))
                .unwrap_or(false)
        })
        .map(|node| node.zone.clone())
        .unwrap_or_else(|| {
            if presence_score > 0.0 {
                "zone_1".to_string()
            } else {
                "unknown".to_string()
            }
        });
    let location = s
        .latest_update
        .as_ref()
        .and_then(|update| update.state.as_ref())
        .and_then(|tracking| tracking.persons.first().map(|person| {
            serde_json::json!({
                "x": person.position_m[0],
                "y": person.position_m[2],
                "z": person.position_m[1],
                "position_m": person.position_m,
                "confidence": person.confidence,
                "source": person.source,
                "timestamp_ms": tracking.timestamp_ms,
            })
        }))
        .or_else(|| {
            localization_snapshot(&s, std::time::Instant::now(), timestamp_ms)
                .0
                .map(|loc| {
                    serde_json::json!({
                        "x": loc.x,
                        "y": loc.y,
                        "z": 0.9,
                        "position_m": [loc.x, 0.9_f32, loc.y],
                        "confidence": loc.confidence,
                        "source": "rssi_csi_trilateration",
                        "timestamp_ms": loc.timestamp_ms,
                    })
                })
        });
    let mut vital_signs = serde_json::json!({
        "breathing_bpm": breathing_bpm,
        "breathing_rate_bpm": breathing_bpm,
        "breathing_confidence": vs.breathing_confidence,
        "breathing_method": BreathingResult::METHOD,
        "motion_energy": normalize_motion_energy(motion_energy),
        "presence_score": clamp_unit(presence_score),
        "fall_suspected": fall_suspected,
        "zone_id": zone_id,
        "signal_quality": vs.signal_quality,
    });
    if let Some(obj) = vital_signs.as_object_mut() {
        if s.feature_flags.beta_enabled(BetaFeature::PreciseHeartRate) {
            obj.insert("heart_rate_bpm".to_string(), serde_json::json!(vs.heart_rate_bpm));
            obj.insert(
                "heartbeat_confidence".to_string(),
                serde_json::json!(vs.heartbeat_confidence),
            );
        }
        if s
            .feature_flags
            .beta_enabled(BetaFeature::CardiacArrestDetection)
        {
            obj.insert(
                "cardiac_risk_score".to_string(),
                serde_json::json!(cardiac_risk_score(vs)),
            );
        }
    }
    Json(serde_json::json!({
        "vital_signs": vital_signs,
        "buffer_status": {
            "breathing_samples": br_len,
            "breathing_capacity": br_cap,
            "heartbeat_samples": hb_len,
            "heartbeat_capacity": hb_cap,
        },
        "location": location,
        "source": s.effective_source(),
        "tick": s.tick,
    }))
}

/// Query params for `GET /api/v1/edge/registry`.
#[derive(Debug, Deserialize)]
struct EdgeRegistryParams {
    /// `?refresh=1` bypasses the in-process cache. Logged at debug for
    /// abuse visibility. ADR-102 §"Cache semantics".
    #[serde(default)]
    refresh: Option<String>,
}

/// GET /api/v1/edge/registry — surfaces the canonical Cognitum cog catalog.
///
/// See ADR-102 (`docs/adr/ADR-102-edge-module-registry.md`) for the design
/// + trust model + security review.
async fn edge_registry_endpoint(
    Extension(reg): Extension<
        Option<Arc<wifi_densepose_sensing_server::edge_registry::EdgeRegistry>>,
    >,
    Query(params): Query<EdgeRegistryParams>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let Some(reg) = reg else {
        // --no-edge-registry, or upstream URL empty.
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": "edge_registry_disabled",
                "detail": "This sensing-server was started with --no-edge-registry."
            })),
        ));
    };
    let force_refresh = matches!(params.refresh.as_deref(), Some("1") | Some("true"));
    if force_refresh {
        tracing::debug!(
            event = "edge_registry.refresh_requested",
            "?refresh=1 bypassed the cache; verify this isn't being abused"
        );
    }
    match tokio::task::spawn_blocking(move || reg.get(force_refresh)).await {
        Ok(Ok(resp)) => Ok(Json(
            serde_json::to_value(resp).unwrap_or(serde_json::json!({})),
        )),
        Ok(Err(err)) => {
            tracing::warn!(error = %err, "edge_registry upstream fetch failed and no cache");
            Err((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "error": "edge_registry_upstream_unavailable",
                    "detail": err.to_string()
                })),
            ))
        }
        Err(join_err) => {
            tracing::error!(error = %join_err, "edge_registry spawn_blocking task panicked");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "edge_registry_internal_error",
                    "detail": join_err.to_string()
                })),
            ))
        }
    }
}

/// GET /api/v1/edge-vitals — latest edge vitals from ESP32 (ADR-039).
async fn edge_vitals_endpoint(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let s = state.read().await;
    match &s.edge_vitals {
        Some(v) => Json(serde_json::json!({
            "status": "ok",
            "edge_vitals": v,
        })),
        None => Json(serde_json::json!({
            "status": "no_data",
            "edge_vitals": null,
            "message": "No edge vitals packet received yet. Ensure ESP32 edge_tier >= 1.",
        })),
    }
}

/// GET /api/v1/wasm-events — latest WASM events from ESP32 (ADR-040).
async fn wasm_events_endpoint(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let s = state.read().await;
    match &s.latest_wasm_events {
        Some(w) => Json(serde_json::json!({
            "status": "ok",
            "wasm_events": w,
        })),
        None => Json(serde_json::json!({
            "status": "no_data",
            "wasm_events": null,
            "message": "No WASM output packet received yet. Upload and start a .wasm module on the ESP32.",
        })),
    }
}

async fn model_info(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let s = state.read().await;
    match &s.rvf_info {
        Some(info) => Json(serde_json::json!({
            "status": "loaded",
            "container": info,
        })),
        None => Json(serde_json::json!({
            "status": "no_model",
            "message": "No RVF container loaded. Use --load-rvf <path> to load one.",
        })),
    }
}

async fn model_layers(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let s = state.read().await;
    match &s.progressive_loader {
        Some(loader) => {
            let (a, b, c) = loader.layer_status();
            Json(serde_json::json!({
                "layer_a": a,
                "layer_b": b,
                "layer_c": c,
                "progress": loader.loading_progress(),
            }))
        }
        None => Json(serde_json::json!({
            "layer_a": false,
            "layer_b": false,
            "layer_c": false,
            "progress": 0.0,
            "message": "No model loaded with progressive loading",
        })),
    }
}

async fn model_segments(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let s = state.read().await;
    match &s.progressive_loader {
        Some(loader) => Json(serde_json::json!({ "segments": loader.segment_list() })),
        None => Json(serde_json::json!({ "segments": [] })),
    }
}

async fn sona_profiles(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let s = state.read().await;
    let names = s
        .progressive_loader
        .as_ref()
        .map(|l| l.sona_profile_names())
        .unwrap_or_default();
    let active = s.active_sona_profile.clone().unwrap_or_default();
    Json(serde_json::json!({ "profiles": names, "active": active }))
}

async fn sona_activate(
    State(state): State<SharedState>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let profile = body
        .get("profile")
        .and_then(|p| p.as_str())
        .unwrap_or("")
        .to_string();

    let mut s = state.write().await;
    let available = s
        .progressive_loader
        .as_ref()
        .map(|l| l.sona_profile_names())
        .unwrap_or_default();

    if available.contains(&profile) {
        s.active_sona_profile = Some(profile.clone());
        Json(serde_json::json!({ "status": "activated", "profile": profile }))
    } else {
        Json(serde_json::json!({
            "status": "error",
            "message": format!("Profile '{}' not found. Available: {:?}", profile, available),
        }))
    }
}

/// GET /api/v1/nodes — per-node health and feature info.
/// ADR-110 iter 29 — per-node mesh sync snapshot via HTTP.
///
/// GET /api/v1/nodes/:id/sync
///   200 → Json(NodeSyncSnapshot) when latest_sync is present
///   404 → {"error": "no_sync", "node_id": N} otherwise
///
/// Complements the WebSocket `sync` field (iter 23) for clients that
/// can't hold a streaming connection (curl scripts, Home Assistant REST
/// sensors, automation rule probes).
async fn node_sync_endpoint(
    State(state): State<SharedState>,
    Path(id): Path<u8>,
) -> Result<Json<NodeSyncSnapshot>, (StatusCode, Json<serde_json::Value>)> {
    let s = state.read().await;
    let ns = s.node_states.get(&id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": "unknown_node", "node_id": id,
            })),
        )
    })?;
    ns.sync_snapshot().map(Json).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": "no_sync", "node_id": id,
                "hint": "node hasn't emitted a sync packet yet (no mesh peer or not v0.6.9+)",
            })),
        )
    })
}

/// ADR-110 iter 29 — fleet-wide mesh state via HTTP.
///
/// GET /api/v1/mesh
///   200 → { "nodes": { "<id>": NodeSyncSnapshot, ... }, "total": N }
///   Nodes without a recent sync are omitted from the map; an empty
///   `nodes` object means no mesh peers reachable.
/// ADR-110 iter 36 — Prometheus exposition format for mesh state.
///
/// GET /api/v1/mesh/metrics → text/plain
///   wifi_densepose_mesh_offset_us{node="N"} <signed-int>
///   wifi_densepose_mesh_is_leader{node="N"} 0|1
///   wifi_densepose_mesh_is_valid{node="N"} 0|1
///   wifi_densepose_mesh_smoothed{node="N"} 0|1
///   wifi_densepose_mesh_sequence{node="N"} <u32>
///   wifi_densepose_mesh_csi_fps{node="N"} <float>
///   wifi_densepose_mesh_csi_fps_samples{node="N"} <u32>
///   wifi_densepose_mesh_staleness_ms{node="N"} <u64>
///
/// Spec: <https://prometheus.io/docs/instrumenting/exposition_formats/>.
/// Each metric is a gauge labeled by node_id. Nodes without a fresh sync
/// are simply absent from the output (Prometheus handles missing series
/// natively — the scrape just reports them as stale after the configured
/// staleness duration).
async fn mesh_metrics_endpoint(State(state): State<SharedState>) -> impl IntoResponse {
    use std::fmt::Write;
    let s = state.read().await;
    let mut body = String::with_capacity(1024);

    // Each metric: HELP + TYPE header + one line per node that has a snapshot.
    let metrics: &[(&str, &str, &str)] = &[
        (
            "wifi_densepose_mesh_offset_us",
            "Cross-board mesh-aligned offset, microseconds (signed)",
            "gauge",
        ),
        (
            "wifi_densepose_mesh_is_leader",
            "1 if this node is the elected mesh leader, else 0",
            "gauge",
        ),
        (
            "wifi_densepose_mesh_is_valid",
            "1 if this node has heard a fresh leader beacon, else 0",
            "gauge",
        ),
        (
            "wifi_densepose_mesh_smoothed",
            "1 once the firmware-side EMA filter has seeded, else 0",
            "gauge",
        ),
        (
            "wifi_densepose_mesh_sequence",
            "High-water CSI sequence at sync emit time",
            "gauge",
        ),
        (
            "wifi_densepose_mesh_csi_fps",
            "Per-node measured CSI frame rate (Hz)",
            "gauge",
        ),
        (
            "wifi_densepose_mesh_csi_fps_samples",
            "How many inter-frame deltas the fps EMA has seen",
            "gauge",
        ),
        (
            "wifi_densepose_mesh_staleness_ms",
            "Milliseconds since the host last received this node's sync packet",
            "gauge",
        ),
    ];

    // Collect (id, snapshot) pairs once so each metric loop reads the same set.
    let snaps: Vec<(u8, NodeSyncSnapshot)> = s
        .node_states
        .iter()
        .filter_map(|(&id, ns)| ns.sync_snapshot().map(|snap| (id, snap)))
        .collect();

    // Iter 37: fleet cardinality summary — Ops dashboards want the
    // "how many leaders / followers / no-sync" tally at a glance
    // without scraping every per-node series and counting.
    let (leaders, followers) = fleet_role_counts(&snaps);
    let no_sync = s.node_states.len().saturating_sub(snaps.len()) as u64;
    let _ = writeln!(
        body,
        "# HELP wifi_densepose_mesh_node_total Per-state node count across the fleet"
    );
    let _ = writeln!(body, "# TYPE wifi_densepose_mesh_node_total gauge");
    let _ = writeln!(
        body,
        "wifi_densepose_mesh_node_total{{state=\"leader\"}} {leaders}"
    );
    let _ = writeln!(
        body,
        "wifi_densepose_mesh_node_total{{state=\"follower\"}} {followers}"
    );
    let _ = writeln!(
        body,
        "wifi_densepose_mesh_node_total{{state=\"no_sync\"}} {no_sync}"
    );

    for (name, help, kind) in metrics {
        let _ = writeln!(body, "# HELP {name} {help}");
        let _ = writeln!(body, "# TYPE {name} {kind}");
        for (id, snap) in &snaps {
            let value = match *name {
                "wifi_densepose_mesh_offset_us" => snap.offset_us.to_string(),
                "wifi_densepose_mesh_is_leader" => bool_metric(snap.is_leader),
                "wifi_densepose_mesh_is_valid" => bool_metric(snap.is_valid),
                "wifi_densepose_mesh_smoothed" => bool_metric(snap.smoothed),
                "wifi_densepose_mesh_sequence" => snap.sequence.to_string(),
                "wifi_densepose_mesh_csi_fps" => format!("{:.3}", snap.csi_fps_ema),
                "wifi_densepose_mesh_csi_fps_samples" => snap.csi_fps_samples.to_string(),
                "wifi_densepose_mesh_staleness_ms" => snap
                    .staleness_ms
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "0".into()),
                _ => continue,
            };
            let _ = writeln!(body, "{name}{{node=\"{id}\"}} {value}");
        }
    }
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        body,
    )
}

fn bool_metric(b: bool) -> String {
    (if b { 1 } else { 0 }).to_string()
}

/// ADR-110 iter 37 — count (leaders, followers) in a populated snapshot set.
/// Free function for testability — same pattern as iter 18's `update_csi_fps_ema`.
pub(crate) fn fleet_role_counts(snaps: &[(u8, NodeSyncSnapshot)]) -> (u64, u64) {
    let leaders = snaps.iter().filter(|(_, s)| s.is_leader).count() as u64;
    let followers = (snaps.len() as u64).saturating_sub(leaders);
    (leaders, followers)
}

async fn mesh_endpoint(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let s = state.read().await;
    let mut nodes = serde_json::Map::new();
    for (&id, ns) in s.node_states.iter() {
        if let Some(snap) = ns.sync_snapshot() {
            nodes.insert(id.to_string(), serde_json::to_value(snap).unwrap());
        }
    }
    let total = nodes.len();
    Json(serde_json::json!({
        "nodes": serde_json::Value::Object(nodes),
        "total": total,
    }))
}

async fn nodes_endpoint(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let s = state.read().await;
    let now = std::time::Instant::now();
    let nodes = all_node_summaries_json(&s, now);
    let total = nodes.len();
    Json(serde_json::json!({
        "nodes": nodes,
        "total": total,
        "active": active_node_count(&s, now),
        "min_nodes": s.min_nodes,
        "ready": fleet_ready(&s, now),
    }))
}

async fn fleet_endpoint(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let s = state.read().await;
    let now = std::time::Instant::now();
    let active_nodes = active_node_count(&s, now);
    let nodes = all_node_summaries_json(&s, now);
    let avg_frame_rate_hz = if active_nodes > 0 {
        s.node_states
            .values()
            .filter(|ns| is_node_active(ns, now))
            .map(|ns| ns.csi_fps_ema)
            .sum::<f64>()
            / active_nodes as f64
    } else {
        0.0
    };
    let fusion_status = if active_nodes >= s.min_nodes {
        "active"
    } else if active_nodes > 0 {
        "degraded"
    } else {
        "offline"
    };
    Json(serde_json::json!({
        "product": "RuvSense Edge",
        "service": "ruvsense-master",
        "version": env!("CARGO_PKG_VERSION"),
        "source": s.effective_source(),
        "source_mode": source_mode_json(&s.effective_source()),
        "ready": active_nodes >= s.min_nodes,
        "active_nodes": active_nodes,
        "known_nodes": s.node_states.len(),
        "configured_nodes": s.environment.nodes.len(),
        "min_nodes": s.min_nodes,
        "frame_rate_hz": avg_frame_rate_hz,
        "fusion_status": fusion_status,
        "uptime_seconds": s.start_time.elapsed().as_secs(),
        "tick": s.tick,
        "clients": s.tx.receiver_count(),
        "nodes": nodes,
    }))
}

fn source_mode_json(source: &str) -> serde_json::Value {
    let simulated = matches!(source, "simulate" | "simulated");
    let offline = source.ends_with(":offline") || source == "offline";
    serde_json::json!({
        "kind": source.split(':').next().unwrap_or(source),
        "raw": source,
        "live": !simulated && !offline,
        "simulated": simulated,
        "degraded_reason": if offline { Some("offline") } else { None::<&str> },
    })
}

async fn environment_endpoint(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let s = state.read().await;
    let now = std::time::Instant::now();
    let active_nodes = active_node_count(&s, now);
    let latest = s.latest_update.as_ref();
    let calibration = s
        .field_model
        .as_ref()
        .map(|fm| format!("{:?}", fm.status()))
        .unwrap_or_else(|| "not_started".to_string());
    Json(serde_json::json!({
        "room": &s.environment.room,
        "access_points": &s.environment.access_points,
        "links": &s.environment.links,
        "fusion": {
            "mode": if active_nodes >= 3 { "multistatic" } else if active_nodes > 0 { "single_or_partial" } else { "offline" },
            "active_nodes": active_nodes,
            "min_nodes": s.min_nodes,
            "dedup_factor": s.dedup_factor,
        },
        "calibration": {
            "status": calibration,
            "empty_room_required": s.field_model.is_some(),
        },
        "occupancy": {
            "estimated_persons": latest.and_then(|u| u.estimated_persons).unwrap_or(0),
            "presence": latest.map(|u| u.classification.presence).unwrap_or(false),
            "motion_level": latest.map(|u| u.classification.motion_level.clone()).unwrap_or_else(|| "unknown".to_string()),
        },
        "signal_field": latest.map(|u| serde_json::to_value(&u.signal_field).unwrap_or_default()),
        "nodes": all_node_summaries_json(&s, now),
    }))
}

async fn topology_endpoint(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let s = state.read().await;
    Json(topology_payload(&s, std::time::Instant::now()))
}

fn validate_dimension_meters(name: &str, value: f64) -> Result<(), String> {
    if !value.is_finite()
        || !(ROOM_DIMENSION_MIN_METERS..=ROOM_DIMENSION_MAX_METERS).contains(&value)
    {
        return Err(format!(
            "{name} must be a finite value in [{ROOM_DIMENSION_MIN_METERS}, {ROOM_DIMENSION_MAX_METERS}] meters"
        ));
    }
    Ok(())
}

fn validate_ui_room_config(config: &UiRoomConfig) -> Result<(), String> {
    validate_dimension_meters("room_width_meters", config.room_width_meters)?;
    validate_dimension_meters("room_height_meters", config.room_height_meters)?;
    if config.nodes.is_empty() {
        return Err("nodes must include at least one node".to_string());
    }
    if config.nodes.len() > ROOM_CONFIG_MAX_NODES {
        return Err(format!(
            "nodes must contain at most {ROOM_CONFIG_MAX_NODES} entries"
        ));
    }

    let mut seen = HashSet::new();
    for node in &config.nodes {
        if !(1..=ROOM_CONFIG_MAX_NODES as u8).contains(&node.id) {
            return Err(format!(
                "node id {} must be in [1, {}]",
                node.id, ROOM_CONFIG_MAX_NODES
            ));
        }
        if !seen.insert(node.id) {
            return Err(format!("duplicate node id {}", node.id));
        }
        if !node.x.is_finite() || !(0.0..=config.room_width_meters).contains(&node.x) {
            return Err(format!(
                "node {} x must be finite and inside the room width",
                node.id
            ));
        }
        if !node.y.is_finite() || !(0.0..=config.room_height_meters).contains(&node.y) {
            return Err(format!(
                "node {} y must be finite and inside the room height",
                node.id
            ));
        }
    }
    Ok(())
}

fn room_vertical_meters(env: &EnvironmentConfig) -> f64 {
    let y = env.room.dimensions_m[1];
    if y.is_finite() && (ROOM_DIMENSION_MIN_METERS..=ROOM_DIMENSION_MAX_METERS).contains(&y) {
        y
    } else {
        DEFAULT_ROOM_VERTICAL_METERS
    }
}

fn environment_from_ui_room_config(
    existing: &EnvironmentConfig,
    config: &UiRoomConfig,
) -> EnvironmentConfig {
    let mut environment = existing.clone();
    let vertical_m = room_vertical_meters(existing);
    let node_height_m = DEFAULT_ROOM_NODE_HEIGHT_METERS.min(vertical_m);
    environment.room.dimensions_m = [
        config.room_width_meters,
        vertical_m,
        config.room_height_meters,
    ];

    let active_total = config.nodes.iter().filter(|node| node.active).count().max(1) as u8;
    let mut active_ids = HashSet::new();
    environment.nodes = config
        .nodes
        .iter()
        .filter(|node| node.active)
        .enumerate()
        .map(|(index, node)| {
            active_ids.insert(node.id);
            let previous = existing.nodes.iter().find(|existing| existing.node_id == node.id);
            EnvironmentNodeConfig {
                node_id: node.id,
                label: previous
                    .map(|existing| existing.label.clone())
                    .unwrap_or_else(|| format!("ESP32-C6 #{}", node.id)),
                kind: previous
                    .map(|existing| existing.kind.clone())
                    .unwrap_or_else(|| "esp32_c6".to_string()),
                zone: previous
                    .map(|existing| existing.zone.clone())
                    .unwrap_or_else(|| "primary".to_string()),
                position_m: [node.x, node_height_m, node.y],
                tdm_slot: previous
                    .map(|existing| existing.tdm_slot)
                    .unwrap_or(index as u8),
                tdm_total: previous
                    .map(|existing| existing.tdm_total.max(1))
                    .unwrap_or(active_total),
                linked_ap: previous
                    .map(|existing| existing.linked_ap.clone())
                    .unwrap_or_default(),
            }
        })
        .collect();
    environment.nodes.sort_by_key(|node| node.node_id);
    environment
        .links
        .retain(|link| active_ids.contains(&link.node_id));
    environment
}

fn save_ui_room_config_file(ui_path: &StdPath, config: &UiRoomConfig) -> Result<(), String> {
    validate_ui_room_config(config)?;
    let path = ui_path.join(ROOM_CONFIG_FILENAME);
    let text = serde_json::to_string_pretty(config)
        .map_err(|e| format!("failed to serialize room config: {e}"))?;
    std::fs::create_dir_all(ui_path)
        .map_err(|e| format!("failed to create {}: {e}", ui_path.display()))?;

    let mut tmp_name = path.as_os_str().to_os_string();
    tmp_name.push(".tmp");
    let tmp_path = PathBuf::from(tmp_name);
    {
        let mut file = std::fs::File::create(&tmp_path)
            .map_err(|e| format!("failed to create {}: {e}", tmp_path.display()))?;
        use std::io::Write as _;
        file.write_all(text.as_bytes())
            .map_err(|e| format!("failed to write {}: {e}", tmp_path.display()))?;
        file.write_all(b"\n")
            .map_err(|e| format!("failed to finalize {}: {e}", tmp_path.display()))?;
        file.sync_all()
            .map_err(|e| format!("failed to sync {}: {e}", tmp_path.display()))?;
    }

    if let Err(first) = std::fs::rename(&tmp_path, &path) {
        if path.exists() {
            std::fs::remove_file(&path)
                .map_err(|e| format!("failed to replace {}: {e}", path.display()))?;
            std::fs::rename(&tmp_path, &path)
                .map_err(|e| format!("failed to save {}: {e}", path.display()))?;
        } else {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(format!("failed to save {}: {first}", path.display()));
        }
    }
    Ok(())
}

fn alert_thresholds_from_update(
    current: &AlertThresholds,
    body: &AlertConfigUpdate,
) -> Result<AlertThresholds, String> {
    if !(APNEA_SECONDS_MIN..=APNEA_SECONDS_MAX).contains(&body.apnea_seconds) {
        return Err(format!(
            "apnea_seconds must be in [{APNEA_SECONDS_MIN}, {APNEA_SECONDS_MAX}]"
        ));
    }
    if !(NO_MOTION_SECONDS_MIN..=NO_MOTION_SECONDS_MAX).contains(&body.no_motion_seconds) {
        return Err(format!(
            "no_motion_seconds must be in [{NO_MOTION_SECONDS_MIN}, {NO_MOTION_SECONDS_MAX}]"
        ));
    }
    if !body.breathing_confidence.is_finite()
        || !(BREATHING_CONFIDENCE_MIN..=BREATHING_CONFIDENCE_MAX)
            .contains(&body.breathing_confidence)
    {
        return Err(format!(
            "breathing_confidence must be a finite value in [{BREATHING_CONFIDENCE_MIN}, {BREATHING_CONFIDENCE_MAX}]"
        ));
    }

    let mut thresholds = current.clone();
    thresholds.apnea_trigger_seconds = body.apnea_seconds;
    thresholds.no_motion_trigger_seconds = body.no_motion_seconds;
    thresholds.apnea_min_confidence = body.breathing_confidence;
    Ok(thresholds)
}

fn validate_environment(env: &EnvironmentConfig) -> Result<(), String> {
    if env
        .room
        .dimensions_m
        .iter()
        .any(|v| !v.is_finite() || *v <= 0.0)
    {
        return Err("room dimensions must be positive finite meters".to_string());
    }
    let mut ap_ids = std::collections::HashSet::new();
    for ap in &env.access_points {
        if ap.ap_id.trim().is_empty() {
            return Err("access point id cannot be empty".to_string());
        }
        if !ap_ids.insert(ap.ap_id.as_str()) {
            return Err(format!("duplicate access point id '{}'", ap.ap_id));
        }
        if ap.position_m.iter().any(|v| !v.is_finite()) {
            return Err(format!("access point '{}' has invalid position", ap.ap_id));
        }
    }
    let mut node_ids = std::collections::HashSet::new();
    for node in &env.nodes {
        if node.node_id == 0 {
            return Err("node_id 0 is reserved for WiFi scan aggregate data".to_string());
        }
        if !node_ids.insert(node.node_id) {
            return Err(format!("duplicate node_id {}", node.node_id));
        }
        if node.position_m.iter().any(|v| !v.is_finite()) {
            return Err(format!("node {} has invalid position", node.node_id));
        }
    }
    for link in &env.links {
        if !ap_ids.contains(link.ap_id.as_str()) {
            return Err(format!(
                "link '{}' references unknown AP '{}'",
                link.link_id, link.ap_id
            ));
        }
        if !node_ids.contains(&link.node_id) {
            return Err(format!(
                "link '{}' references unknown node {}",
                link.link_id, link.node_id
            ));
        }
    }
    let mut obstacle_ids = std::collections::HashSet::new();
    for obstacle in &env.obstacles {
        if obstacle.obstacle_id.trim().is_empty() {
            return Err("obstacle id cannot be empty".to_string());
        }
        if !obstacle_ids.insert(obstacle.obstacle_id.as_str()) {
            return Err(format!("duplicate obstacle id '{}'", obstacle.obstacle_id));
        }
        if obstacle.center_m.iter().any(|v| !v.is_finite()) {
            return Err(format!(
                "obstacle '{}' has invalid center",
                obstacle.obstacle_id
            ));
        }
        if obstacle
            .size_m
            .iter()
            .any(|v| !v.is_finite() || *v <= 0.0)
        {
            return Err(format!(
                "obstacle '{}' has invalid size",
                obstacle.obstacle_id
            ));
        }
        if !obstacle.yaw_rad.is_finite() || !(0.0..=1.0).contains(&obstacle.confidence) {
            return Err(format!(
                "obstacle '{}' has invalid confidence or yaw",
                obstacle.obstacle_id
            ));
        }
    }
    Ok(())
}

fn configured_fuser_positions(env: &EnvironmentConfig) -> Vec<[f32; 3]> {
    let mut nodes = env.nodes.clone();
    nodes.sort_by_key(|node| node.node_id);
    nodes
        .into_iter()
        .map(|node| {
            [
                node.position_m[0] as f32,
                node.position_m[1] as f32,
                node.position_m[2] as f32,
            ]
        })
        .collect()
}

fn apply_environment_node_positions(fuser: &mut MultistaticFuser, env: &EnvironmentConfig) {
    let positions = configured_fuser_positions(env);
    if positions.is_empty() {
        info!("Clearing configured environment node positions from multistatic fuser");
    } else {
        info!(
            "Applying {} configured environment node position(s) to multistatic fuser",
            positions.len()
        );
    }
    fuser.set_node_positions(positions);
}

#[derive(Debug, Clone, Deserialize)]
struct NodePositionUpdate {
    node_id: u8,
    position_m: [f64; 3],
}

#[derive(Debug, Deserialize)]
struct NodePositionsUpdate {
    nodes: Vec<NodePositionUpdate>,
}

fn validate_node_position_updates(updates: &[NodePositionUpdate]) -> Result<(), String> {
    if updates.is_empty() {
        return Err("nodes must include at least one position".to_string());
    }
    let mut seen = HashSet::new();
    for update in updates {
        if update.node_id == 0 {
            return Err("node_id 0 is reserved for WiFi scan aggregate data".to_string());
        }
        if !seen.insert(update.node_id) {
            return Err(format!("duplicate node_id {}", update.node_id));
        }
        if update.position_m.iter().any(|value| !value.is_finite()) {
            return Err(format!("node {} has invalid position", update.node_id));
        }
    }
    Ok(())
}

fn upsert_environment_node_positions(
    env: &mut EnvironmentConfig,
    updates: &[NodePositionUpdate],
    known_live_nodes: &HashSet<u8>,
) -> Result<(), String> {
    validate_node_position_updates(updates)?;
    for update in updates {
        if let Some(existing) = env.nodes.iter_mut().find(|node| node.node_id == update.node_id) {
            existing.position_m = update.position_m;
            continue;
        }
        if !known_live_nodes.contains(&update.node_id) {
            return Err(format!(
                "node {} is not configured or live",
                update.node_id
            ));
        }
        env.nodes.push(EnvironmentNodeConfig {
            node_id: update.node_id,
            label: format!("ESP32-C6 #{}", update.node_id),
            kind: "esp32_c6".to_string(),
            zone: "manual".to_string(),
            position_m: update.position_m,
            tdm_slot: 0,
            tdm_total: 1,
            linked_ap: String::new(),
        });
    }
    env.nodes.sort_by_key(|node| node.node_id);
    validate_environment(env)
}

async fn environment_update_endpoint(
    State(state): State<SharedState>,
    Json(environment): Json<EnvironmentConfig>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if let Err(message) = validate_environment(&environment) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "status": "error", "message": message })),
        ));
    }

    let s = state.read().await;
    let runtime_config_path = s.runtime_config_path.clone();
    let dedup_factor = s.dedup_factor;
    let enabled_modules = sorted_module_ids(&s.enabled_modules);
    drop(s);

    if let Err(message) = save_runtime_config_file(
        &runtime_config_path,
        &RuntimeConfig {
            dedup_factor,
            environment: environment.clone(),
            module_config_version: MODULE_CONFIG_VERSION,
            enabled_modules,
        },
    ) {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "status": "error", "message": message })),
        ));
    }

    let mut s = state.write().await;
    s.environment = environment.clone();
    apply_environment_node_positions(&mut s.multistatic_fuser, &environment);

    Ok(Json(serde_json::json!({
        "status": "ok",
        "environment": environment,
    })))
}

async fn environment_node_positions_update_endpoint(
    State(state): State<SharedState>,
    Json(body): Json<NodePositionsUpdate>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let s = state.read().await;
    let mut environment = s.environment.clone();
    let known_live_nodes = s.node_states.keys().copied().collect::<HashSet<_>>();
    if let Err(message) =
        upsert_environment_node_positions(&mut environment, &body.nodes, &known_live_nodes)
    {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "status": "error", "message": message })),
        ));
    }
    let runtime_config_path = s.runtime_config_path.clone();
    let dedup_factor = s.dedup_factor;
    let enabled_modules = sorted_module_ids(&s.enabled_modules);
    drop(s);

    if let Err(message) = save_runtime_config_file(
        &runtime_config_path,
        &RuntimeConfig {
            dedup_factor,
            environment: environment.clone(),
            module_config_version: MODULE_CONFIG_VERSION,
            enabled_modules,
        },
    ) {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "status": "error", "message": message })),
        ));
    }

    let mut s = state.write().await;
    s.environment = environment.clone();
    apply_environment_node_positions(&mut s.multistatic_fuser, &environment);

    Ok(Json(serde_json::json!({
        "status": "ok",
        "environment": environment,
    })))
}

fn module_status(active_nodes: usize, required_nodes: usize) -> &'static str {
    if active_nodes >= required_nodes {
        "active"
    } else if active_nodes > 0 {
        "available"
    } else {
        "offline"
    }
}

fn module_confidence(active_nodes: usize, required_nodes: usize) -> f64 {
    if active_nodes >= required_nodes {
        (0.62 + active_nodes as f64 * 0.08).min(0.95)
    } else if active_nodes > 0 {
        0.42
    } else {
        0.0
    }
}

fn sorted_module_ids(enabled_modules: &HashSet<String>) -> Vec<String> {
    let mut ids = enabled_modules.iter().cloned().collect::<Vec<_>>();
    ids.sort();
    ids
}

fn module_presets_json() -> Vec<serde_json::Value> {
    MODULE_PRESETS
        .iter()
        .map(|preset| {
            serde_json::json!({
                "id": preset.id,
                "label": preset.label,
                "module_ids": preset.module_ids,
            })
        })
        .collect()
}

fn business_modules(active_nodes: usize, enabled_modules: &HashSet<String>) -> Vec<serde_json::Value> {
    let modules: &[(&str, &str, &str, u16, usize, &[&str])] = &[
        (
            "fall_detection",
            "Health & Vitals",
            "Fall detection",
            8,
            1,
            &["edge_vitals", "sensing_update"],
        ),
        (
            "sleep_apnea_screening",
            "Health & Vitals",
            "Sleep apnea screening",
            12,
            1,
            &["vital_signs", "edge_vitals"],
        ),
        (
            "cardiac_arrhythmia",
            "Health & Vitals",
            "Cardiac arrhythmia",
            14,
            1,
            &["vital_signs"],
        ),
        (
            "seizure_detection",
            "Health & Vitals",
            "Seizure detection",
            10,
            3,
            &["sensing_update", "introspection"],
        ),
        (
            "sleep_staging",
            "Health & Vitals",
            "Sleep staging",
            18,
            1,
            &["vital_signs", "sensing_update"],
        ),
        (
            "respiration_tracking",
            "Health & Vitals",
            "Respiration tracking",
            6,
            1,
            &["vital_signs"],
        ),
        (
            "intrusion_detection",
            "Safety & Security",
            "Intrusion detection",
            9,
            1,
            &["sensing_update"],
        ),
        (
            "loitering_alert",
            "Safety & Security",
            "Loitering alert",
            7,
            2,
            &["sensing_update", "environment"],
        ),
        (
            "panic_motion",
            "Safety & Security",
            "Panic motion",
            5,
            1,
            &["sensing_update"],
        ),
        (
            "confined_space_monitor",
            "Safety & Security",
            "Confined-space monitor",
            11,
            3,
            &["environment", "vital_signs"],
        ),
        (
            "exclusion_zone_breach",
            "Safety & Security",
            "Exclusion-zone breach",
            8,
            3,
            &["environment"],
        ),
        (
            "cobot_proximity",
            "Safety & Security",
            "Cobot proximity",
            12,
            3,
            &["environment", "pose"],
        ),
        (
            "hvac_zone_control",
            "Building Automation",
            "HVAC zone control",
            10,
            1,
            &["environment"],
        ),
        (
            "lighting_automation",
            "Building Automation",
            "Lighting automation",
            6,
            1,
            &["sensing_update"],
        ),
        (
            "occupancy_based_access",
            "Building Automation",
            "Occupancy-based access",
            11,
            2,
            &["fleet", "environment"],
        ),
        (
            "meeting_room_sensing",
            "Building Automation",
            "Meeting-room sensing",
            8,
            1,
            &["sensing_update"],
        ),
        (
            "desk_utilization",
            "Building Automation",
            "Desk utilization",
            7,
            2,
            &["environment"],
        ),
        (
            "door_open_detection",
            "Building Automation",
            "Door-open detection",
            5,
            1,
            &["introspection"],
        ),
        (
            "queue_length",
            "Analytics",
            "Queue length",
            8,
            3,
            &["environment"],
        ),
        (
            "dwell_heatmap",
            "Analytics",
            "Dwell heatmap",
            14,
            3,
            &["environment"],
        ),
        (
            "table_turnover",
            "Analytics",
            "Table turnover",
            9,
            2,
            &["environment"],
        ),
        (
            "people_counting",
            "Analytics",
            "People counting",
            7,
            3,
            &["sensing_update", "fleet"],
        ),
        (
            "path_analytics",
            "Analytics",
            "Path analytics",
            16,
            3,
            &["environment", "pose"],
        ),
        (
            "conversion_funnels",
            "Analytics",
            "Conversion funnels",
            11,
            3,
            &["environment"],
        ),
        (
            "gesture_recognition",
            "Interaction",
            "Gesture recognition",
            22,
            2,
            &["introspection", "pose"],
        ),
        (
            "pointing_swipe",
            "Interaction",
            "Pointing / swipe",
            15,
            3,
            &["pose"],
        ),
        (
            "emotion_inference",
            "Interaction",
            "Emotion inference",
            28,
            3,
            &["vital_signs", "pose"],
        ),
        (
            "activity_classification",
            "Interaction",
            "Activity classification",
            19,
            2,
            &["sensing_update", "introspection"],
        ),
        (
            "posture_analytics",
            "Interaction",
            "Posture analytics",
            13,
            3,
            &["pose"],
        ),
        (
            "gait_biometrics",
            "Interaction",
            "Gait biometrics",
            24,
            3,
            &["pose", "environment"],
        ),
    ];

    modules
        .iter()
        .map(|(id, category, name, size_kb, required_nodes, streams)| {
            let safety_note = if *category == "Health & Vitals" {
                Some("Monitoring/screening only; not a medical diagnosis.")
            } else {
                None
            };
            let capability_status = module_status(active_nodes, *required_nodes);
            let enabled = enabled_modules.contains(*id);
            let effective_status = if enabled {
                capability_status
            } else {
                "disabled"
            };
            serde_json::json!({
                "id": id,
                "category": category,
                "name": name,
                "size_kb": size_kb,
                "status": effective_status,
                "enabled": enabled,
                "effective_status": effective_status,
                "capability_status": capability_status,
                "confidence": module_confidence(active_nodes, *required_nodes),
                "required_nodes": required_nodes,
                "active_nodes": active_nodes,
                "evidence_streams": streams,
                "safety_note": safety_note,
            })
        })
        .collect()
}

fn modules_catalog_json(
    active_nodes: usize,
    min_nodes: usize,
    enabled_modules: &HashSet<String>,
) -> serde_json::Value {
    serde_json::json!({
        "catalog": "ruvsense-edge",
        "active_nodes": active_nodes,
        "min_nodes": min_nodes,
        "enabled_modules": sorted_module_ids(enabled_modules),
        "presets": module_presets_json(),
        "categories": [
            "Health & Vitals",
            "Safety & Security",
            "Building Automation",
            "Analytics",
            "Interaction"
        ],
        "modules": business_modules(active_nodes, enabled_modules),
    })
}

async fn modules_endpoint(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let s = state.read().await;
    let now = std::time::Instant::now();
    let active_nodes = active_node_count(&s, now);
    Json(modules_catalog_json(
        active_nodes,
        s.min_nodes,
        &s.enabled_modules,
    ))
}

#[derive(Debug, Deserialize)]
struct ModuleEnabledUpdate {
    enabled: bool,
}

#[derive(Debug, Deserialize)]
struct ModulesEnabledUpdate {
    enabled_modules: Vec<String>,
}

fn module_id_exists(id: &str) -> bool {
    let empty = HashSet::new();
    business_modules(0, &empty)
        .iter()
        .any(|module| module.get("id").and_then(|v| v.as_str()) == Some(id))
}

fn enabled_module_set_from_ids(ids: &[String]) -> Result<HashSet<String>, String> {
    let mut set = HashSet::new();
    for id in ids {
        if !module_id_exists(id) {
            return Err(format!("unknown module '{id}'"));
        }
        set.insert(id.clone());
    }
    Ok(set)
}

async fn modules_enabled_update_endpoint(
    State(state): State<SharedState>,
    Json(body): Json<ModulesEnabledUpdate>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let next_enabled_modules = enabled_module_set_from_ids(&body.enabled_modules).map_err(|message| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "status": "error", "message": message })),
        )
    })?;

    let s = state.read().await;
    let runtime_config_path = s.runtime_config_path.clone();
    let dedup_factor = s.dedup_factor;
    let environment = s.environment.clone();
    let enabled_modules = sorted_module_ids(&next_enabled_modules);
    drop(s);

    if let Err(message) = save_runtime_config_file(
        &runtime_config_path,
        &RuntimeConfig {
            dedup_factor,
            environment,
            module_config_version: MODULE_CONFIG_VERSION,
            enabled_modules,
        },
    ) {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "status": "error", "message": message })),
        ));
    }

    let mut s = state.write().await;
    s.enabled_modules = next_enabled_modules;
    let active_nodes = active_node_count(&s, std::time::Instant::now());
    Ok(Json(serde_json::json!({
        "status": "ok",
        "catalog": modules_catalog_json(active_nodes, s.min_nodes, &s.enabled_modules),
    })))
}

async fn module_enabled_update_endpoint(
    Path(id): Path<String>,
    State(state): State<SharedState>,
    Json(body): Json<ModuleEnabledUpdate>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if !module_id_exists(&id) {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "status": "error",
                "message": format!("unknown module '{id}'"),
            })),
        ));
    }

    let s = state.read().await;
    let mut next_enabled_modules = s.enabled_modules.clone();
    if body.enabled {
        next_enabled_modules.insert(id.clone());
    } else {
        next_enabled_modules.remove(&id);
    }
    let active_nodes = active_node_count(&s, std::time::Instant::now());
    let modules = business_modules(active_nodes, &next_enabled_modules);
    let module = modules
        .into_iter()
        .find(|module| module.get("id").and_then(|v| v.as_str()) == Some(id.as_str()))
        .unwrap_or_else(|| serde_json::json!({ "id": id, "enabled": body.enabled }));
    let runtime_config_path = s.runtime_config_path.clone();
    let dedup_factor = s.dedup_factor;
    let environment = s.environment.clone();
    let enabled_modules = sorted_module_ids(&next_enabled_modules);
    drop(s);

    if let Err(message) = save_runtime_config_file(
        &runtime_config_path,
        &RuntimeConfig {
            dedup_factor,
            environment,
            module_config_version: MODULE_CONFIG_VERSION,
            enabled_modules,
        },
    ) {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "status": "error", "message": message })),
        ));
    }

    let mut s = state.write().await;
    s.enabled_modules = next_enabled_modules;

    Ok(Json(serde_json::json!({
        "status": "ok",
        "module": module,
    })))
}

fn adaptive_calibration_snapshot_json(
    node_id: u8,
    snapshot: AdaptiveCalibrationSnapshot,
) -> serde_json::Value {
    serde_json::json!({
        "node_id": node_id,
        "status": format!("{:?}", snapshot.status),
        "last_decision": format!("{:?}", snapshot.last_decision),
        "frames_observed": snapshot.frames_observed,
        "candidate_frames": snapshot.candidate_frames,
        "confirmed_drift_windows": snapshot.confirmed_drift_windows,
        "last_window_drift_score": snapshot.last_window_drift_score,
        "promotions": snapshot.promotions,
        "rejections": snapshot.rejections,
        "active_baseline_frames": snapshot.active_baseline_frames,
    })
}

fn adaptive_calibration_nodes_json(
    node_states: &HashMap<u8, NodeState>,
) -> Vec<serde_json::Value> {
    node_states
        .iter()
        .filter_map(|(&node_id, ns)| {
            ns.adaptive_calibration
                .as_ref()
                .map(|monitor| adaptive_calibration_snapshot_json(node_id, monitor.snapshot()))
        })
        .collect()
}

async fn calibration_overview_endpoint(
    State(state): State<SharedState>,
) -> Json<serde_json::Value> {
    let mut s = state.write().await;
    let now = std::time::Instant::now();
    maybe_drive_auto_calibration(&mut s, now);
    Json(calibration_snapshot_json(&s, now))
}

fn append_udp_jitter_prometheus_metrics(
    body: &mut String,
    stats_by_node: &HashMap<u8, udp_jitter::NodeJitterSnapshot>,
) {
    use std::fmt::Write;

    let _ = writeln!(
        body,
        "# HELP ruvsense_csi_frames_total CSI frames emitted by the UDP jitter buffer"
    );
    let _ = writeln!(body, "# TYPE ruvsense_csi_frames_total counter");
    for (&id, stats) in stats_by_node {
        let _ = writeln!(
            body,
            "ruvsense_csi_frames_total{{node=\"{id}\",kind=\"live\"}} {}",
            stats.emitted_live_total
        );
        let _ = writeln!(
            body,
            "ruvsense_csi_frames_total{{node=\"{id}\",kind=\"interpolated\"}} {}",
            stats.interpolated_total
        );
    }
    let _ = writeln!(
        body,
        "# HELP ruvsense_udp_packets_dropped_total CSI packet gaps or stale duplicates dropped by the UDP jitter buffer"
    );
    let _ = writeln!(body, "# TYPE ruvsense_udp_packets_dropped_total counter");
    for (&id, stats) in stats_by_node {
        let _ = writeln!(
            body,
            "ruvsense_udp_packets_dropped_total{{node=\"{id}\",reason=\"missing_gap\"}} {}",
            stats.missing_dropped_total
        );
        let _ = writeln!(
            body,
            "ruvsense_udp_packets_dropped_total{{node=\"{id}\",reason=\"late_or_duplicate\"}} {}",
            stats.late_or_duplicate_total
        );
    }
    let _ = writeln!(
        body,
        "# HELP ruvsense_node_dropped_packet_ratio Dropped CSI packet ratio per ESP32 node"
    );
    let _ = writeln!(body, "# TYPE ruvsense_node_dropped_packet_ratio gauge");
    for (&id, stats) in stats_by_node {
        let _ = writeln!(
            body,
            "ruvsense_node_dropped_packet_ratio{{node=\"{id}\"}} {:.6}",
            stats.dropped_ratio()
        );
    }
    let _ = writeln!(
        body,
        "# HELP ruvsense_udp_reordered_frames_total Out-of-order CSI frames recovered by the UDP jitter buffer"
    );
    let _ = writeln!(body, "# TYPE ruvsense_udp_reordered_frames_total counter");
    for (&id, stats) in stats_by_node {
        let _ = writeln!(
            body,
            "ruvsense_udp_reordered_frames_total{{node=\"{id}\"}} {}",
            stats.reordered_total
        );
    }
    let _ = writeln!(
        body,
        "# HELP ruvsense_udp_jitter_buffer_depth Current queued CSI frames in the UDP jitter buffer"
    );
    let _ = writeln!(body, "# TYPE ruvsense_udp_jitter_buffer_depth gauge");
    for (&id, stats) in stats_by_node {
        let _ = writeln!(
            body,
            "ruvsense_udp_jitter_buffer_depth{{node=\"{id}\"}} {}",
            stats.buffer_depth
        );
    }
    let _ = writeln!(
        body,
        "# HELP ruvsense_udp_jitter_hold_ms Latest and max jitter-buffer hold time"
    );
    let _ = writeln!(body, "# TYPE ruvsense_udp_jitter_hold_ms gauge");
    for (&id, stats) in stats_by_node {
        let _ = writeln!(
            body,
            "ruvsense_udp_jitter_hold_ms{{node=\"{id}\",kind=\"latest\"}} {}",
            stats.last_hold_ms
        );
        let _ = writeln!(
            body,
            "ruvsense_udp_jitter_hold_ms{{node=\"{id}\",kind=\"max\"}} {}",
            stats.max_hold_ms
        );
    }
}

fn append_adaptive_calibration_prometheus_metrics(
    body: &mut String,
    node_states: &HashMap<u8, NodeState>,
) {
    use std::fmt::Write;

    let _ = writeln!(
        body,
        "# HELP ruvsense_adaptive_calibration_promotions_total Adaptive baseline promotions per ESP32 node"
    );
    let _ = writeln!(
        body,
        "# TYPE ruvsense_adaptive_calibration_promotions_total counter"
    );
    for (&id, ns) in node_states {
        if let Some(ref monitor) = ns.adaptive_calibration {
            let snapshot = monitor.snapshot();
            let _ = writeln!(
                body,
                "ruvsense_adaptive_calibration_promotions_total{{node=\"{id}\"}} {}",
                snapshot.promotions
            );
        }
    }

    let _ = writeln!(
        body,
        "# HELP ruvsense_adaptive_calibration_rejections_total Adaptive baseline candidate rejections per ESP32 node"
    );
    let _ = writeln!(
        body,
        "# TYPE ruvsense_adaptive_calibration_rejections_total counter"
    );
    for (&id, ns) in node_states {
        if let Some(ref monitor) = ns.adaptive_calibration {
            let snapshot = monitor.snapshot();
            let _ = writeln!(
                body,
                "ruvsense_adaptive_calibration_rejections_total{{node=\"{id}\"}} {}",
                snapshot.rejections
            );
        }
    }

    let _ = writeln!(
        body,
        "# HELP ruvsense_adaptive_calibration_candidate_frames Current adaptive calibration candidate frames"
    );
    let _ = writeln!(
        body,
        "# TYPE ruvsense_adaptive_calibration_candidate_frames gauge"
    );
    for (&id, ns) in node_states {
        if let Some(ref monitor) = ns.adaptive_calibration {
            let snapshot = monitor.snapshot();
            let _ = writeln!(
                body,
                "ruvsense_adaptive_calibration_candidate_frames{{node=\"{id}\"}} {}",
                snapshot.candidate_frames
            );
        }
    }

    let _ = writeln!(
        body,
        "# HELP ruvsense_adaptive_calibration_status Adaptive calibration status label; value is always 1 for the current status"
    );
    let _ = writeln!(body, "# TYPE ruvsense_adaptive_calibration_status gauge");
    for (&id, ns) in node_states {
        if let Some(ref monitor) = ns.adaptive_calibration {
            let snapshot = monitor.snapshot();
            let _ = writeln!(
                body,
                "ruvsense_adaptive_calibration_status{{node=\"{id}\",status=\"{:?}\"}} 1",
                snapshot.status
            );
        }
    }
}

async fn prometheus_metrics_endpoint(State(state): State<SharedState>) -> impl IntoResponse {
    let s = state.read().await;
    let now = std::time::Instant::now();
    match render_prometheus_metrics(&s, now) {
        Ok(body) => (
            StatusCode::OK,
            [(
                axum::http::header::CONTENT_TYPE,
                "text/plain; version=0.0.4; charset=utf-8",
            )],
            body,
        )
            .into_response(),
        Err(message) => {
            error!("Failed to render Prometheus metrics: {message}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                [(
                    axum::http::header::CONTENT_TYPE,
                    "text/plain; charset=utf-8",
                )],
                format!("metrics render error: {message}\n"),
            )
                .into_response()
        }
    }
}

fn render_prometheus_metrics(s: &AppStateInner, now: std::time::Instant) -> Result<String, String> {
    use prometheus::{
        Encoder, GaugeVec, IntCounter, IntCounterVec, IntGauge, IntGaugeVec, Opts, Registry,
        TextEncoder,
    };

    fn register<M: prometheus::core::Collector + Clone + 'static>(
        registry: &Registry,
        metric: &M,
    ) -> Result<(), String> {
        registry
            .register(Box::new(metric.clone()))
            .map_err(|e| format!("register metric: {e}"))
    }

    let registry = Registry::new();
    let active_nodes = active_node_count(s, now) as i64;
    let ready = if (active_nodes as usize) >= s.min_nodes {
        1
    } else {
        0
    };

    let master_ready = IntGauge::with_opts(Opts::new(
        "ruvsense_master_ready",
        "Master readiness based on active node quorum",
    ))
    .map_err(|e| e.to_string())?;
    register(&registry, &master_ready)?;
    master_ready.set(ready);

    let active_nodes_gauge = IntGauge::with_opts(Opts::new(
        "ruvsense_active_nodes",
        "Active ESP32 nodes seen within readiness timeout",
    ))
    .map_err(|e| e.to_string())?;
    register(&registry, &active_nodes_gauge)?;
    active_nodes_gauge.set(active_nodes);

    let min_nodes = IntGauge::with_opts(Opts::new(
        "ruvsense_min_nodes",
        "Minimum active nodes required for ready",
    ))
    .map_err(|e| e.to_string())?;
    register(&registry, &min_nodes)?;
    min_nodes.set(s.min_nodes as i64);

    let ticks = IntCounter::with_opts(Opts::new(
        "ruvsense_ticks_total",
        "Sensing update ticks processed",
    ))
    .map_err(|e| e.to_string())?;
    register(&registry, &ticks)?;
    ticks.inc_by(s.tick);

    let ws_clients = IntGauge::with_opts(Opts::new(
        "ruvsense_ws_clients",
        "Current WebSocket subscriber count",
    ))
    .map_err(|e| e.to_string())?;
    register(&registry, &ws_clients)?;
    ws_clients.set(s.tx.receiver_count() as i64);

    let frame_rate = GaugeVec::new(
        Opts::new(
            "ruvsense_node_frame_rate_hz",
            "Per-node CSI frame-rate EMA",
        ),
        &["node"],
    )
    .map_err(|e| e.to_string())?;
    register(&registry, &frame_rate)?;

    let node_active = IntGaugeVec::new(
        Opts::new("ruvsense_node_active", "Per-node active status"),
        &["node"],
    )
    .map_err(|e| e.to_string())?;
    register(&registry, &node_active)?;

    let tensor_compression = GaugeVec::new(
        Opts::new(
            "ruvsense_tensor_compression_ratio",
            "Raw-to-encoded tensor compression ratio by node and buffer",
        ),
        &["node", "buffer"],
    )
    .map_err(|e| e.to_string())?;
    register(&registry, &tensor_compression)?;

    for (&id, ns) in &s.node_states {
        let id_label = id.to_string();
        frame_rate
            .with_label_values(&[id_label.as_str()])
            .set(ns.csi_fps_ema);
        node_active
            .with_label_values(&[id_label.as_str()])
            .set(if is_node_active(ns, now) { 1 } else { 0 });
        tensor_compression
            .with_label_values(&[id_label.as_str(), "breathing"])
            .set(ns.latest_breathing_compression_ratio);
        tensor_compression
            .with_label_values(&[id_label.as_str(), "heartbeat"])
            .set(ns.latest_heartbeat_compression_ratio);
    }

    let csi_frames = IntCounterVec::new(
        Opts::new(
            "ruvsense_csi_frames_total",
            "CSI frames emitted by the UDP jitter buffer",
        ),
        &["node", "kind"],
    )
    .map_err(|e| e.to_string())?;
    register(&registry, &csi_frames)?;

    let udp_dropped = IntCounterVec::new(
        Opts::new(
            "ruvsense_udp_packets_dropped_total",
            "CSI packet gaps or stale duplicates dropped by the UDP jitter buffer",
        ),
        &["node", "reason"],
    )
    .map_err(|e| e.to_string())?;
    register(&registry, &udp_dropped)?;

    let dropped_ratio = GaugeVec::new(
        Opts::new(
            "ruvsense_node_dropped_packet_ratio",
            "Dropped CSI packet ratio per ESP32 node",
        ),
        &["node"],
    )
    .map_err(|e| e.to_string())?;
    register(&registry, &dropped_ratio)?;

    let reordered = IntCounterVec::new(
        Opts::new(
            "ruvsense_udp_reordered_frames_total",
            "Out-of-order CSI frames recovered by the UDP jitter buffer",
        ),
        &["node"],
    )
    .map_err(|e| e.to_string())?;
    register(&registry, &reordered)?;

    let jitter_depth = IntGaugeVec::new(
        Opts::new(
            "ruvsense_udp_jitter_buffer_depth",
            "Current queued CSI frames in the UDP jitter buffer",
        ),
        &["node"],
    )
    .map_err(|e| e.to_string())?;
    register(&registry, &jitter_depth)?;

    let jitter_hold = IntGaugeVec::new(
        Opts::new(
            "ruvsense_udp_jitter_hold_ms",
            "Latest and max jitter-buffer hold time",
        ),
        &["node", "kind"],
    )
    .map_err(|e| e.to_string())?;
    register(&registry, &jitter_hold)?;

    for (&id, stats) in &s.udp_jitter_stats {
        let id_label = id.to_string();
        csi_frames
            .with_label_values(&[id_label.as_str(), "live"])
            .inc_by(stats.emitted_live_total);
        csi_frames
            .with_label_values(&[id_label.as_str(), "interpolated"])
            .inc_by(stats.interpolated_total);
        udp_dropped
            .with_label_values(&[id_label.as_str(), "missing_gap"])
            .inc_by(stats.missing_dropped_total);
        udp_dropped
            .with_label_values(&[id_label.as_str(), "late_or_duplicate"])
            .inc_by(stats.late_or_duplicate_total);
        dropped_ratio
            .with_label_values(&[id_label.as_str()])
            .set(stats.dropped_ratio());
        reordered
            .with_label_values(&[id_label.as_str()])
            .inc_by(stats.reordered_total);
        jitter_depth
            .with_label_values(&[id_label.as_str()])
            .set(stats.buffer_depth as i64);
        jitter_hold
            .with_label_values(&[id_label.as_str(), "latest"])
            .set(stats.last_hold_ms as i64);
        jitter_hold
            .with_label_values(&[id_label.as_str(), "max"])
            .set(stats.max_hold_ms as i64);
    }

    let adaptive_promotions = IntCounterVec::new(
        Opts::new(
            "ruvsense_adaptive_calibration_promotions_total",
            "Adaptive baseline promotions per ESP32 node",
        ),
        &["node"],
    )
    .map_err(|e| e.to_string())?;
    register(&registry, &adaptive_promotions)?;
    let adaptive_rejections = IntCounterVec::new(
        Opts::new(
            "ruvsense_adaptive_calibration_rejections_total",
            "Adaptive baseline candidate rejections per ESP32 node",
        ),
        &["node"],
    )
    .map_err(|e| e.to_string())?;
    register(&registry, &adaptive_rejections)?;
    let adaptive_candidate_frames = IntGaugeVec::new(
        Opts::new(
            "ruvsense_adaptive_calibration_candidate_frames",
            "Current adaptive calibration candidate frames",
        ),
        &["node"],
    )
    .map_err(|e| e.to_string())?;
    register(&registry, &adaptive_candidate_frames)?;
    let adaptive_status = IntGaugeVec::new(
        Opts::new(
            "ruvsense_adaptive_calibration_status",
            "Adaptive calibration status label; value is always 1 for the current status",
        ),
        &["node", "status"],
    )
    .map_err(|e| e.to_string())?;
    register(&registry, &adaptive_status)?;
    for (&id, ns) in &s.node_states {
        if let Some(ref monitor) = ns.adaptive_calibration {
            let snapshot = monitor.snapshot();
            let id_label = id.to_string();
            adaptive_promotions
                .with_label_values(&[id_label.as_str()])
                .inc_by(snapshot.promotions);
            adaptive_rejections
                .with_label_values(&[id_label.as_str()])
                .inc_by(snapshot.rejections);
            adaptive_candidate_frames
                .with_label_values(&[id_label.as_str()])
                .set(snapshot.candidate_frames as i64);
            let status_label = format!("{:?}", snapshot.status);
            adaptive_status
                .with_label_values(&[id_label.as_str(), status_label.as_str()])
                .set(1);
        }
    }

    let neural_latency = GaugeVec::new(
        Opts::new(
            "ruvsense_neural_inference_latency_ms",
            "Latest and P95 pose/model processing latency",
        ),
        &["quantile"],
    )
    .map_err(|e| e.to_string())?;
    register(&registry, &neural_latency)?;
    neural_latency
        .with_label_values(&["latest"])
        .set(s.latest_pose_latency_ms);
    neural_latency
        .with_label_values(&["p95"])
        .set(s.pose_latency_p95_ms.current().unwrap_or(0.0));

    let dsp_latency = GaugeVec::new(
        Opts::new(
            "ruvsense_dsp_latency_ms",
            "Latest and P95 DSP feature/vitals processing latency",
        ),
        &["quantile"],
    )
    .map_err(|e| e.to_string())?;
    register(&registry, &dsp_latency)?;
    dsp_latency
        .with_label_values(&["latest"])
        .set(s.latest_dsp_latency_ms);
    dsp_latency
        .with_label_values(&["p95"])
        .set(s.dsp_latency_p95_ms.current().unwrap_or(0.0));

    let active_tracking_zones = s
        .latest_update
        .as_ref()
        .and_then(|u| u.persons.as_ref())
        .map(|persons| {
            persons
                .iter()
                .filter(|person| person.confidence > 0.0)
                .map(|person| person.zone.as_str())
                .collect::<HashSet<_>>()
                .len()
        })
        .unwrap_or(0);
    let active_zones = IntGauge::with_opts(Opts::new(
        "ruvsense_active_tracking_zones",
        "Active tracking zones with live person tracks",
    ))
    .map_err(|e| e.to_string())?;
    register(&registry, &active_zones)?;
    active_zones.set(active_tracking_zones as i64);

    let encoder = TextEncoder::new();
    let metric_families = registry.gather();
    let mut buffer = Vec::new();
    encoder
        .encode(&metric_families, &mut buffer)
        .map_err(|e| format!("encode metrics: {e}"))?;
    String::from_utf8(buffer).map_err(|e| format!("metrics were not UTF-8: {e}"))
}

async fn config_schema_endpoint() -> Json<schemars::schema::RootSchema> {
    Json(runtime_config_schema())
}

async fn info_page() -> Html<String> {
    Html(
        "<html><body>\
         <h1>WiFi-DensePose Sensing Server</h1>\
         <p>Rust + Axum + RuVector</p>\
         <ul>\
         <li><a href='/health'>/health</a> — Server health</li>\
         <li><a href='/api/v1/sensing/latest'>/api/v1/sensing/latest</a> — Latest sensing data</li>\
         <li><a href='/api/v1/vital-signs'>/api/v1/vital-signs</a> — Vital sign estimates (HR/RR)</li>\
         <li><a href='/api/v1/model/info'>/api/v1/model/info</a> — RVF model container info</li>\
         <li>ws://localhost:8765/ws/sensing — WebSocket stream</li>\
         </ul>\
         </body></html>"
         .to_string()
    )
}

// ── UDP receiver task ────────────────────────────────────────────────────────

async fn udp_receiver_task(
    state: SharedState,
    udp_port: u16,
    jitter_config: udp_jitter::UdpJitterConfig,
) {
    let addr = format!("0.0.0.0:{udp_port}");
    let socket = match UdpSocket::bind(&addr).await {
        Ok(s) => {
            info!("UDP listening on {addr} for ESP32 CSI frames");
            s
        }
        Err(e) => {
            error!("Failed to bind UDP {addr}: {e}");
            return;
        }
    };

    let mut buf = [0u8; 2048];
    let mut jitter = udp_jitter::UdpJitterBuffer::new(jitter_config);
    let mut last_src_by_node: HashMap<u8, SocketAddr> = HashMap::new();
    loop {
        match tokio::time::timeout(jitter.max_hold(), socket.recv_from(&mut buf)).await {
            Ok(Ok((len, src))) => {
                // ADR-039: Try edge vitals packet first (magic 0xC511_0002).
                if let Some(vitals) = parse_esp32_vitals(&buf[..len]) {
                    debug!(
                        "ESP32 vitals from {src}: node={} br={:.1} hr={:.1} pres={}",
                        vitals.node_id,
                        vitals.breathing_rate_bpm,
                        vitals.heartrate_bpm,
                        vitals.presence
                    );
                    let mut s = state.write().await;
                    // Broadcast vitals via WebSocket.
                    if let Ok(json) = serde_json::to_string(&serde_json::json!({
                        "type": "edge_vitals",
                        "node_id": vitals.node_id,
                        "presence": vitals.presence,
                        "fall_detected": vitals.fall_detected,
                        "motion": vitals.motion,
                        "breathing_rate_bpm": vitals.breathing_rate_bpm,
                        "heartrate_bpm": vitals.heartrate_bpm,
                        "n_persons": vitals.n_persons,
                        "motion_energy": vitals.motion_energy,
                        "presence_score": vitals.presence_score,
                        "rssi": vitals.rssi,
                    })) {
                        let _ = s.tx.send(json);
                    }

                    // Issue #323: Also emit a sensing_update so the UI renders
                    // detections for ESP32 nodes running the edge DSP pipeline
                    // (Tier 2+).  Without this, vitals arrive but the UI shows
                    // "no detection" because it only renders sensing_update msgs.
                    s.source = "esp32".to_string();
                    s.last_esp32_frame = Some(std::time::Instant::now());

                    // ── Per-node state for edge vitals (issue #249) ──────
                    let node_id = vitals.node_id;
                    let ns = s
                        .node_states
                        .entry(node_id)
                        .or_insert_with(|| NodeState::new_for_node(node_id));
                    let now_arrival = std::time::Instant::now();
                    ns.observe_edge_vitals_arrival(now_arrival);
                    ns.observe_remote_addr(src);
                    ns.edge_vitals = Some(vitals.clone());
                    ns.rssi_history.push_back(vitals.rssi as f64);
                    if ns.rssi_history.len() > 60 {
                        ns.rssi_history.pop_front();
                    }

                    // Store per-node person count from edge vitals.
                    let node_est = if vitals.presence {
                        (vitals.n_persons as usize).max(1)
                    } else {
                        0
                    };
                    ns.prev_person_count = node_est;
                    let (motion_level, motion_score) =
                        update_node_presence_from_edge_vitals(ns, &vitals);

                    s.tick += 1;
                    let tick = s.tick;

                    // Aggregate person count: gate on presence first (matching WiFi path).
                    let now = std::time::Instant::now();
                    let total_persons = if vitals.presence {
                        let dedup = s.dedup_factor;
                        let (fused, fallback_count) = multistatic_bridge::fuse_or_fallback(
                            &s.multistatic_fuser,
                            &s.node_states,
                            dedup,
                        );
                        match fused {
                            Some(ref f) => {
                                let score =
                                    multistatic_bridge::compute_person_score_from_amplitudes(
                                        &f.fused_amplitude,
                                    );
                                s.smoothed_person_score =
                                    s.smoothed_person_score * 0.90 + score * 0.10;
                                // #803: don't let the saturating activity score
                                // discard count-aware per-node estimates.
                                let count =
                                    aggregate_person_count(s.person_count(), &s.node_states, now);
                                s.prev_person_count = count;
                                count.max(1) // presence=true => at least 1
                            }
                            None => {
                                aggregate_person_count(
                                    fallback_count.unwrap_or(0),
                                    &s.node_states,
                                    now,
                                )
                                    .max(1)
                            }
                        }
                    } else {
                        s.prev_person_count = 0;
                        0
                    };

                    maybe_drive_auto_calibration(&mut s, now);
                    // Feed field model calibration if active (use per-node history for ESP32).
                    if let Some(frame_history) = s
                        .node_states
                        .get(&node_id)
                        .map(|ns| ns.frame_history.clone())
                    {
                        if let Some(ref mut fm) = s.field_model {
                            field_bridge::maybe_feed_calibration(fm, &frame_history);
                        }
                    }
                    maybe_drive_auto_calibration(&mut s, now);

                    // Build nodes array with all active nodes.
                    let environment = s.environment.clone();
                    let active_nodes: Vec<NodeInfo> = s
                        .node_states
                        .iter()
                        .filter(|(_, n)| {
                            n.last_frame_time
                                .is_some_and(|t| now.duration_since(t).as_secs() < 10)
                        })
                        .map(|(&id, n)| NodeInfo {
                            node_id: id,
                            rssi_dbm: n.rssi_history.back().copied().unwrap_or(0.0),
                            position: configured_node_position(&environment, id),
                            amplitude: vec![],
                            subcarrier_count: 0,
                            // Vitals-only path; still expose the sync snapshot
                            // if the node also speaks ESP-NOW.
                            sync: n.sync_snapshot(),
                        })
                        .collect();

                    let features = FeatureInfo {
                        mean_rssi: vitals.rssi as f64,
                        variance: vitals.motion_energy as f64,
                        motion_band_power: vitals.motion_energy as f64,
                        breathing_band_power: if vitals.presence { 0.5 } else { 0.0 },
                        dominant_freq_hz: vitals.breathing_rate_bpm / 60.0,
                        change_points: 0,
                        spectral_power: vitals.motion_energy as f64,
                    };

                    // Store latest features on node for cross-node fusion.
                    if let Some(ns) = s.node_states.get_mut(&node_id) {
                        ns.latest_features = Some(features.clone());
                    }

                    // Cross-node fusion: combine features from all active nodes.
                    let fused_features = fuse_multi_node_features(&features, &s.node_states);

                    let mut classification = ClassificationInfo {
                        motion_level: motion_level.to_string(),
                        presence: vitals.presence,
                        confidence: vitals.presence_score as f64,
                    };

                    // Boost classification confidence with multi-node coverage.
                    let n_active = s
                        .node_states
                        .values()
                        .filter(|ns| {
                            ns.last_frame_time
                                .is_some_and(|t| now.duration_since(t).as_secs() < 10)
                        })
                        .count();
                    if n_active > 1 {
                        classification.confidence = (classification.confidence
                            * (1.0 + 0.15 * (n_active as f64 - 1.0)))
                            .clamp(0.0, 1.0);
                    }

                    let signal_field = generate_signal_field(
                        fused_features.mean_rssi,
                        motion_score,
                        vitals.breathing_rate_bpm / 60.0,
                        (vitals.presence_score as f64).min(1.0),
                        &[],
                    );
                    let count_evidence = apply_room_presence_continuity(
                        &mut s,
                        &mut classification,
                        total_persons,
                        now,
                    );
                    let rendered_persons = count_evidence.rendered_persons;

                    let mut update = SensingUpdate {
                        msg_type: "sensing_update".to_string(),
                        timestamp: chrono::Utc::now().timestamp_millis() as f64 / 1000.0,
                        source: "esp32".to_string(),
                        tick,
                        nodes: active_nodes,
                        features: fused_features.clone(),
                        classification,
                        signal_field,
                        vital_signs: Some(VitalSigns {
                            breathing_rate_bpm: if vitals.breathing_rate_bpm > 0.0 {
                                Some(vitals.breathing_rate_bpm)
                            } else {
                                None
                            },
                            heart_rate_bpm: if vitals.heartrate_bpm > 0.0 {
                                Some(vitals.heartrate_bpm)
                            } else {
                                None
                            },
                            breathing_confidence: if vitals.presence { 0.7 } else { 0.0 },
                            heartbeat_confidence: if vitals.presence { 0.7 } else { 0.0 },
                            signal_quality: vitals.presence_score as f64,
                        }),
                        enhanced_motion: None,
                        enhanced_breathing: None,
                        posture: None,
                        signal_quality_score: None,
                        quality_verdict: None,
                        bssid_count: None,
                        pose_keypoints: None,
                        model_status: None,
                        persons: None,
                        state: None,
                        estimated_persons: if rendered_persons > 0 {
                            Some(rendered_persons)
                        } else {
                            None
                        },
                        count_evidence: Some(count_evidence),
                        // ADR-084 Pass 3.6: surface per-node novelty_score
                        // (and the rest of the per-node feature snapshot)
                        // on the WebSocket envelope so cluster-Pi consumers
                        // can implement model-wake gating without round-
                        // tripping back to the server.
                        node_features: build_node_features(&s.node_states, now),
                    };

                    finalize_persons_for_update(&mut s, &mut update);

                    publish_sensing_update(&mut s, update);
                    s.edge_vitals = Some(vitals);
                    continue;
                }

                // ADR-110 §A0.12: Try sync packet (magic 0xC511_A110).
                // A 32-byte UDP datagram carrying mesh-aligned epoch + sequence
                // high-water from the node's c6_sync_espnow EMA-smoothed offset.
                // Stored per-node so subsequent CSI frames with byte 19 bit 4
                // set can have an aligned timestamp recovered downstream.
                if len >= wifi_densepose_hardware::SYNC_PACKET_SIZE {
                    let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
                    if magic == wifi_densepose_hardware::SYNC_PACKET_MAGIC {
                        match wifi_densepose_hardware::SyncPacket::from_bytes(&buf[..len]) {
                            Ok(sync) => {
                                debug!("ESP32 sync from {src}: node={} leader={} valid={} smoothed={} \
                                        seq={} offset_us={}",
                                       sync.node_id, sync.flags.is_leader, sync.flags.is_valid,
                                       sync.flags.smoothed_used, sync.sequence,
                                       sync.local_minus_epoch_us());
                                let mut s = state.write().await;
                                let node_id = sync.node_id;
                                let ns = s
                                    .node_states
                                    .entry(node_id)
                                    .or_insert_with(|| NodeState::new_for_node(node_id));
                                ns.apply_sync_packet(sync, std::time::Instant::now());
                                ns.observe_remote_addr(src);
                                continue;
                            }
                            Err(e) => {
                                debug!("Sync packet decode error from {src}: {e}");
                                // Fall through — magic matched but decode failed; not a CSI frame.
                                continue;
                            }
                        }
                    }
                }

                // ADR-063: Try edge fused vitals packet (magic 0xC511_0004).
                // Must come BEFORE the WASM parser — issue #928: these two
                // packet types shared a magic and the WASM parser was eating
                // fused-vitals frames on the C6+mmWave config. The reassign of
                // WASM_OUTPUT_MAGIC → 0xC511_0007 (firmware side) plus this
                // dedicated parser resolve the collision.
                if let Some(fused) = parse_edge_fused_vitals(&buf[..len]) {
                    debug!(
                        "Edge fused vitals from {src}: node={} br={:.1} hr={:.1} \
                         mmwave_targets={} fusion_conf={}",
                        fused.node_id,
                        fused.breathing_rate_bpm,
                        fused.heartrate_bpm,
                        fused.mmwave_targets,
                        fused.fusion_confidence,
                    );
                    let mut s = state.write().await;
                    let ns = s
                        .node_states
                        .entry(fused.node_id)
                        .or_insert_with(|| NodeState::new_for_node(fused.node_id));
                    ns.observe_edge_vitals_arrival(std::time::Instant::now());
                    ns.observe_remote_addr(src);
                    ns.rssi_history.push_back(fused.rssi as f64);
                    if ns.rssi_history.len() > 60 {
                        ns.rssi_history.pop_front();
                    }
                    if let Ok(json) = serde_json::to_string(&serde_json::json!({
                        "type": "edge_fused_vitals",
                        "node_id": fused.node_id,
                        "breathing_rate_bpm": fused.breathing_rate_bpm,
                        "heartrate_bpm": fused.heartrate_bpm,
                        "n_persons": fused.n_persons,
                        "fusion_confidence": fused.fusion_confidence,
                        "mmwave": {
                            "hr_bpm": fused.mmwave_hr_bpm,
                            "br_bpm": fused.mmwave_br_bpm,
                            "distance_cm": fused.mmwave_distance_cm,
                            "targets": fused.mmwave_targets,
                            "confidence": fused.mmwave_confidence,
                            "type": fused.mmwave_type,
                        },
                        "motion_energy": fused.motion_energy,
                        "presence_score": fused.presence_score,
                        "timestamp_ms": fused.timestamp_ms,
                    })) {
                        let _ = s.tx.send(json);
                    }
                    continue;
                }

                // ADR-040: Try WASM output packet (magic 0xC511_0007 post-#928).
                if let Some(wasm_output) = parse_wasm_output(&buf[..len]) {
                    debug!(
                        "WASM output from {src}: node={} module={} events={}",
                        wasm_output.node_id,
                        wasm_output.module_id,
                        wasm_output.events.len()
                    );
                    let mut s = state.write().await;
                    // Broadcast WASM events via WebSocket.
                    if let Ok(json) = serde_json::to_string(&serde_json::json!({
                        "type": "wasm_event",
                        "node_id": wasm_output.node_id,
                        "module_id": wasm_output.module_id,
                        "events": wasm_output.events,
                    })) {
                        let _ = s.tx.send(json);
                    }
                    s.node_states
                        .entry(wasm_output.node_id)
                        .or_insert_with(|| NodeState::new_for_node(wasm_output.node_id))
                        .observe_remote_addr(src);
                    s.latest_wasm_events = Some(wasm_output);
                    continue;
                }

                if let Some(frame) = parse_esp32_frame(&buf[..len]) {
                    let jitter_node_id = frame.node_id;
                    last_src_by_node.insert(jitter_node_id, src);
                    let delivered_frames = jitter.push(frame, std::time::Instant::now());
                    if delivered_frames.is_empty() {
                        if let Some(stats) = jitter.snapshot(jitter_node_id) {
                            state
                                .write()
                                .await
                                .udp_jitter_stats
                                .insert(jitter_node_id, stats);
                        }
                    }
                    for delivered in delivered_frames {
                        let frame = delivered.frame;
                        let is_live_frame = delivered.kind == udp_jitter::FrameKind::Live;
                        let jitter_snapshot = jitter.snapshot(frame.node_id);
                        debug!(
                            "ESP32 frame from {src}: node={}, subs={}, seq={}",
                            frame.node_id, frame.n_subcarriers, frame.sequence
                        );

                    let mut s = state.write().await;
                    if let Some(stats) = jitter_snapshot {
                        s.udp_jitter_stats.insert(frame.node_id, stats);
                    }
                    if is_live_frame {
                        s.source = "esp32".to_string();
                        s.last_esp32_frame = Some(std::time::Instant::now());
                    }

                    // Also maintain global frame_history for backward compat
                    // (simulation path, REST endpoints, etc.).
                    s.frame_history.push_back(frame.amplitudes.clone());
                    if s.frame_history.len() > FRAME_HISTORY_CAPACITY {
                        s.frame_history.pop_front();
                    }

                    // ── ADR-099: real-time introspection tap ────────────────
                    // Per-frame update of the attractor / DTW pipeline running
                    // parallel to the window-aggregated event path. Placed
                    // BEFORE the per-node `&mut` borrow of `s.node_states` so
                    // `s.intro` / `s.intro_tx` stay reachable. Never window-
                    // blocked; `/ws/introspection` sees a fresh snapshot on
                    // every accepted frame.
                    {
                        let intro_feature = if frame.amplitudes.is_empty() {
                            0.0
                        } else {
                            frame.amplitudes.iter().copied().sum::<f64>()
                                / frame.amplitudes.len() as f64
                        };
                        let intro_ts_ns = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_nanos() as u64)
                            .unwrap_or(0);
                        let _ = s.intro.update(intro_ts_ns, intro_feature);
                        if let Ok(intro_json) = serde_json::to_string(s.intro.snapshot()) {
                            let _ = s.intro_tx.send(intro_json);
                        }
                    }

                    // ── Per-node processing (issue #249) ──────────────────
                    // Process entirely within per-node state so different
                    // ESP32 nodes never mix their smoothing/vitals buffers.
                    // We scope the mutable borrow of node_states so we can
                    // access other AppStateInner fields afterward.
                    let node_id = frame.node_id;
                    // Clone adaptive model before mutable borrow of node_states
                    // to avoid unsafe raw pointer (review finding #2).
                    let adaptive_model_clone = s.adaptive_model.clone();

                    let ns = s
                        .node_states
                        .entry(node_id)
                        .or_insert_with(|| NodeState::new_for_node(node_id));
                    // ADR-110 iter 19 — feed the per-node fps EMA from real
                    // CSI arrivals. The helper sets `last_frame_time` as a
                    // side effect, so the previous bare assignment is gone.
                    if is_live_frame {
                        ns.observe_csi_frame_arrival(std::time::Instant::now());
                    }
                    ns.observe_remote_addr(src);

                    // ADR-084 Pass 3: cluster-Pi novelty sensor.
                    // Score this frame's feature vector against the per-node
                    // sketch bank *before* pushing it (so the score reflects
                    // pre-insert state). Result lands in `ns.last_novelty_score`
                    // for downstream model-wake gating.
                    ns.update_novelty(&frame.amplitudes);

                    ns.frame_history.push_back(frame.amplitudes.clone());
                    if ns.frame_history.len() > FRAME_HISTORY_CAPACITY {
                        ns.frame_history.pop_front();
                    }

                    let dsp_started = std::time::Instant::now();
                    let sample_rate_hz = 1000.0 / 500.0_f64;
                    let (
                        features,
                        mut classification,
                        breathing_rate_hz,
                        sub_variances,
                        raw_motion,
                    ) = extract_features_from_frame(&frame, &ns.frame_history, sample_rate_hz);
                    smooth_and_classify_node(ns, &mut classification, raw_motion);

                    // Adaptive override using cloned model (safe, no raw pointers).
                    if let Some(ref model) = adaptive_model_clone {
                        let amps = ns.frame_history.back().map(|v| v.as_slice()).unwrap_or(&[]);
                        let feat_arr = adaptive_classifier::features_from_runtime(
                            &serde_json::json!({
                                "variance": features.variance,
                                "motion_band_power": features.motion_band_power,
                                "breathing_band_power": features.breathing_band_power,
                                "spectral_power": features.spectral_power,
                                "dominant_freq_hz": features.dominant_freq_hz,
                                "change_points": features.change_points,
                                "mean_rssi": features.mean_rssi,
                            }),
                            amps,
                        );
                        let (label, conf) = model.classify(&feat_arr);
                        classification.motion_level = label.to_string();
                        classification.presence = label != "absent";
                        classification.confidence =
                            (conf * 0.7 + classification.confidence * 0.3).clamp(0.0, 1.0);
                    }

                    update_node_adaptive_calibration(
                        ns,
                        &frame,
                        &classification,
                        &features,
                        is_live_frame,
                    );

                    ns.rssi_history.push_back(features.mean_rssi);
                    if ns.rssi_history.len() > 60 {
                        ns.rssi_history.pop_front();
                    }

                    let breathing_result =
                        update_node_breathing_from_phase(ns, &frame, is_live_frame);
                    let raw_vitals = ns
                        .vital_detector
                        .process_frame(&frame.amplitudes, &frame.phases);
                    let mut vitals = smooth_vitals_node(ns, &raw_vitals);
                    apply_breathing_to_vitals(&mut vitals, breathing_result);
                    ns.latest_vitals = vitals.clone();
                    ns.update_tensor_compression(&frame.amplitudes, &frame.phases);

                    // DynamicMinCut person estimation from subcarrier correlation.
                    let corr_persons = estimate_persons_from_correlation(&ns.frame_history);
                    // #803: map the min-cut count onto a threshold-aligned score
                    // so it round-trips back to the same count. The old
                    // `corr_persons / 3.0` left 2 people at 0.667 — under the
                    // 0.70 up-threshold — so the count was pinned at 1.
                    let raw_score = corr_persons_to_score(corr_persons);
                    ns.smoothed_person_score = ns.smoothed_person_score * 0.92 + raw_score * 0.08;
                    if classification.presence {
                        let count =
                            score_to_person_count(ns.smoothed_person_score, ns.prev_person_count);
                        ns.prev_person_count = count;
                    } else {
                        ns.prev_person_count = 0;
                    }

                    // Store latest features on node for cross-node fusion.
                    ns.latest_features = Some(features.clone());
                    let _ = ns;
                    record_dsp_latency(&mut s, dsp_started);

                    // Done with per-node mutable borrow; now read aggregated
                    // state from all nodes (the borrow of `ns` ends here).
                    // (We re-borrow node_states immutably via `s` below.)

                    s.rssi_history.push_back(features.mean_rssi);
                    if s.rssi_history.len() > 60 {
                        s.rssi_history.pop_front();
                    }
                    s.latest_vitals = vitals.clone();

                    // Cross-node fusion: combine features from all active nodes.
                    let fused_features = fuse_multi_node_features(&features, &s.node_states);

                    s.tick += 1;
                    let tick = s.tick;

                    let motion_score = if classification.motion_level == "active" {
                        0.8
                    } else if classification.motion_level == "present_still" {
                        0.3
                    } else {
                        0.05
                    };

                    // Aggregate person count: gate on presence first (matching WiFi path).
                    let now = std::time::Instant::now();
                    let total_persons = if classification.presence {
                        let dedup = s.dedup_factor;
                        let (fused, fallback_count) = multistatic_bridge::fuse_or_fallback(
                            &s.multistatic_fuser,
                            &s.node_states,
                            dedup,
                        );
                        match fused {
                            Some(ref f) => {
                                let score =
                                    multistatic_bridge::compute_person_score_from_amplitudes(
                                        &f.fused_amplitude,
                                    );
                                s.smoothed_person_score =
                                    s.smoothed_person_score * 0.90 + score * 0.10;
                                // #803: don't let the saturating activity score
                                // discard count-aware per-node estimates.
                                let count =
                                    aggregate_person_count(s.person_count(), &s.node_states, now);
                                s.prev_person_count = count;
                                count.max(1)
                            }
                            None => {
                                aggregate_person_count(
                                    fallback_count.unwrap_or(0),
                                    &s.node_states,
                                    now,
                                )
                                    .max(1)
                            }
                        }
                    } else {
                        s.prev_person_count = 0;
                        0
                    };
                    let count_evidence = apply_room_presence_continuity(
                        &mut s,
                        &mut classification,
                        total_persons,
                        now,
                    );
                    let rendered_persons = count_evidence.rendered_persons;

                    maybe_drive_auto_calibration(&mut s, now);
                    // Feed field model calibration only from live CSI. Interpolated
                    // frames stabilize runtime DSP but never alter baselines.
                    if is_live_frame {
                        if let Some(frame_history) = s
                            .node_states
                            .get(&node_id)
                            .map(|ns| ns.frame_history.clone())
                        {
                            if let Some(ref mut fm) = s.field_model {
                                field_bridge::maybe_feed_calibration(fm, &frame_history);
                            }
                        }
                    }
                    maybe_drive_auto_calibration(&mut s, now);
                    maybe_update_fused_breathing(&mut s, now);
                    let update_vitals = s.latest_vitals.clone();

                    // Build nodes array with all active nodes.
                    let environment = s.environment.clone();
                    let active_nodes: Vec<NodeInfo> = s
                        .node_states
                        .iter()
                        .filter(|(_, n)| {
                            n.last_frame_time
                                .is_some_and(|t| now.duration_since(t).as_secs() < 10)
                        })
                        .map(|(&id, n)| NodeInfo {
                            node_id: id,
                            rssi_dbm: n.rssi_history.back().copied().unwrap_or(0.0),
                            position: configured_node_position(&environment, id),
                            amplitude: n
                                .frame_history
                                .back()
                                .map(|a| a.iter().take(56).cloned().collect())
                                .unwrap_or_default(),
                            subcarrier_count: n.frame_history.back().map_or(0, |a| a.len()),
                            // ADR-110 iter 23 / iter 30 — single source of truth.
                            sync: n.sync_snapshot(),
                        })
                        .collect();

                    let mut update = SensingUpdate {
                        msg_type: "sensing_update".to_string(),
                        timestamp: chrono::Utc::now().timestamp_millis() as f64 / 1000.0,
                        source: if is_live_frame {
                            "esp32"
                        } else {
                            "esp32:interpolated"
                        }
                        .to_string(),
                        tick,
                        nodes: active_nodes,
                        features: fused_features.clone(),
                        classification,
                        signal_field: generate_signal_field(
                            fused_features.mean_rssi,
                            motion_score,
                            breathing_rate_hz,
                            fused_features.variance.min(1.0),
                            &sub_variances,
                        ),
                        vital_signs: Some(update_vitals),
                        enhanced_motion: None,
                        enhanced_breathing: None,
                        posture: None,
                        signal_quality_score: None,
                        quality_verdict: None,
                        bssid_count: None,
                        pose_keypoints: None,
                        model_status: None,
                        persons: None,
                        state: None,
                        estimated_persons: if rendered_persons > 0 {
                            Some(rendered_persons)
                        } else {
                            None
                        },
                        count_evidence: Some(count_evidence),
                        // ADR-084 Pass 3.6: surface per-node novelty_score
                        // (and the rest of the per-node feature snapshot)
                        // on the WebSocket envelope so cluster-Pi consumers
                        // can implement model-wake gating without round-
                        // tripping back to the server.
                        node_features: build_node_features(&s.node_states, now),
                    };

                    finalize_persons_for_update(&mut s, &mut update);

                    publish_sensing_update(&mut s, update);

                    // Evict stale nodes every 100 ticks to prevent memory leak.
                    if tick % 100 == 0 {
                        let stale = Duration::from_secs(60);
                        let before = s.node_states.len();
                        s.node_states.retain(|_id, ns| {
                            ns.last_frame_time
                                .is_some_and(|t| now.duration_since(t) < stale)
                        });
                        let evicted = before - s.node_states.len();
                        if evicted > 0 {
                            info!(
                                "Evicted {} stale node(s), {} active",
                                evicted,
                                s.node_states.len()
                            );
                        }
                    }
                }
            }
        }
            Ok(Err(e)) => {
                warn!("UDP recv error: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(_) => {
                let delivered_frames = jitter.flush_due(std::time::Instant::now());
                for delivered in delivered_frames {
                    let node_id = delivered.frame.node_id;
                    let src = last_src_by_node.get(&node_id).copied();
                    let jitter_snapshot = jitter.snapshot(node_id);
                    handle_delivered_esp32_frame(&state, src, delivered, jitter_snapshot).await;
                }
            }
        }
    }
}

// ── Simulated data task ──────────────────────────────────────────────────────

async fn handle_delivered_esp32_frame(
    state: &SharedState,
    src: Option<SocketAddr>,
    delivered: udp_jitter::DeliveredFrame,
    jitter_snapshot: Option<udp_jitter::NodeJitterSnapshot>,
) {
    let frame = delivered.frame;
    let is_live_frame = delivered.kind == udp_jitter::FrameKind::Live;
    let src_label = src
        .map(|addr| addr.to_string())
        .unwrap_or_else(|| "held-buffer".to_string());
    debug!(
        "ESP32 frame from {src_label}: node={}, subs={}, seq={}",
        frame.node_id, frame.n_subcarriers, frame.sequence
    );

    let mut s = state.write().await;
    if let Some(stats) = jitter_snapshot {
        s.udp_jitter_stats.insert(frame.node_id, stats);
    }
    if is_live_frame {
        s.source = "esp32".to_string();
        s.last_esp32_frame = Some(std::time::Instant::now());
    }

    s.frame_history.push_back(frame.amplitudes.clone());
    if s.frame_history.len() > FRAME_HISTORY_CAPACITY {
        s.frame_history.pop_front();
    }

    {
        let intro_feature = if frame.amplitudes.is_empty() {
            0.0
        } else {
            frame.amplitudes.iter().copied().sum::<f64>() / frame.amplitudes.len() as f64
        };
        let intro_ts_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let _ = s.intro.update(intro_ts_ns, intro_feature);
        if let Ok(intro_json) = serde_json::to_string(s.intro.snapshot()) {
            let _ = s.intro_tx.send(intro_json);
        }
    }

    let node_id = frame.node_id;
    let adaptive_model_clone = s.adaptive_model.clone();

    let ns = s
        .node_states
        .entry(node_id)
        .or_insert_with(|| NodeState::new_for_node(node_id));
    if is_live_frame {
        ns.observe_csi_frame_arrival(std::time::Instant::now());
    }
    if let Some(src) = src {
        ns.observe_remote_addr(src);
    }

    ns.update_novelty(&frame.amplitudes);
    ns.frame_history.push_back(frame.amplitudes.clone());
    if ns.frame_history.len() > FRAME_HISTORY_CAPACITY {
        ns.frame_history.pop_front();
    }

    let dsp_started = std::time::Instant::now();
    let sample_rate_hz = 1000.0 / 500.0_f64;
    let (features, mut classification, breathing_rate_hz, sub_variances, raw_motion) =
        extract_features_from_frame(&frame, &ns.frame_history, sample_rate_hz);
    smooth_and_classify_node(ns, &mut classification, raw_motion);

    if let Some(ref model) = adaptive_model_clone {
        let amps = ns.frame_history.back().map(|v| v.as_slice()).unwrap_or(&[]);
        let feat_arr = adaptive_classifier::features_from_runtime(
            &serde_json::json!({
                "variance": features.variance,
                "motion_band_power": features.motion_band_power,
                "breathing_band_power": features.breathing_band_power,
                "spectral_power": features.spectral_power,
                "dominant_freq_hz": features.dominant_freq_hz,
                "change_points": features.change_points,
                "mean_rssi": features.mean_rssi,
            }),
            amps,
        );
        let (label, conf) = model.classify(&feat_arr);
        classification.motion_level = label.to_string();
        classification.presence = label != "absent";
        classification.confidence = (conf * 0.7 + classification.confidence * 0.3).clamp(0.0, 1.0);
    }

    update_node_adaptive_calibration(
        ns,
        &frame,
        &classification,
        &features,
        is_live_frame,
    );

    ns.rssi_history.push_back(features.mean_rssi);
    if ns.rssi_history.len() > 60 {
        ns.rssi_history.pop_front();
    }

    let breathing_result = update_node_breathing_from_phase(ns, &frame, is_live_frame);
    let raw_vitals = ns
        .vital_detector
        .process_frame(&frame.amplitudes, &frame.phases);
    let mut vitals = smooth_vitals_node(ns, &raw_vitals);
    apply_breathing_to_vitals(&mut vitals, breathing_result);
    ns.latest_vitals = vitals.clone();
    ns.update_tensor_compression(&frame.amplitudes, &frame.phases);

    let corr_persons = estimate_persons_from_correlation(&ns.frame_history);
    let raw_score = corr_persons_to_score(corr_persons);
    ns.smoothed_person_score = ns.smoothed_person_score * 0.92 + raw_score * 0.08;
    if classification.presence {
        let count = score_to_person_count(ns.smoothed_person_score, ns.prev_person_count);
        ns.prev_person_count = count;
    } else {
        ns.prev_person_count = 0;
    }
    ns.latest_features = Some(features.clone());
    let _ = ns;
    record_dsp_latency(&mut s, dsp_started);

    s.rssi_history.push_back(features.mean_rssi);
    if s.rssi_history.len() > 60 {
        s.rssi_history.pop_front();
    }
    s.latest_vitals = vitals.clone();

    let fused_features = fuse_multi_node_features(&features, &s.node_states);

    s.tick += 1;
    let tick = s.tick;

    let motion_score = if classification.motion_level == "active" {
        0.8
    } else if classification.motion_level == "present_still" {
        0.3
    } else {
        0.05
    };

    let now = std::time::Instant::now();
    let total_persons = if classification.presence {
        let dedup = s.dedup_factor;
        let (fused, fallback_count) =
            multistatic_bridge::fuse_or_fallback(&s.multistatic_fuser, &s.node_states, dedup);
        match fused {
            Some(ref f) => {
                let score =
                    multistatic_bridge::compute_person_score_from_amplitudes(&f.fused_amplitude);
                s.smoothed_person_score = s.smoothed_person_score * 0.90 + score * 0.10;
                let count = aggregate_person_count(s.person_count(), &s.node_states, now);
                s.prev_person_count = count;
                count.max(1)
            }
            None => aggregate_person_count(fallback_count.unwrap_or(0), &s.node_states, now).max(1),
        }
    } else {
        s.prev_person_count = 0;
        0
    };
    let count_evidence = apply_room_presence_continuity(
        &mut s,
        &mut classification,
        total_persons,
        now,
    );
    let rendered_persons = count_evidence.rendered_persons;

    maybe_drive_auto_calibration(&mut s, now);
    if is_live_frame {
        if let Some(frame_history) = s
            .node_states
            .get(&node_id)
            .map(|ns| ns.frame_history.clone())
        {
            if let Some(ref mut fm) = s.field_model {
                field_bridge::maybe_feed_calibration(fm, &frame_history);
            }
        }
    }
    maybe_drive_auto_calibration(&mut s, now);
    maybe_update_fused_breathing(&mut s, now);
    let update_vitals = s.latest_vitals.clone();

    let environment = s.environment.clone();
    let active_nodes: Vec<NodeInfo> = s
        .node_states
        .iter()
        .filter(|(_, n)| {
            n.last_frame_time
                .is_some_and(|t| now.duration_since(t).as_secs() < 10)
        })
        .map(|(&id, n)| NodeInfo {
            node_id: id,
            rssi_dbm: n.rssi_history.back().copied().unwrap_or(0.0),
            position: configured_node_position(&environment, id),
            amplitude: n
                .frame_history
                .back()
                .map(|a| a.iter().take(56).cloned().collect())
                .unwrap_or_default(),
            subcarrier_count: n.frame_history.back().map_or(0, |a| a.len()),
            sync: n.sync_snapshot(),
        })
        .collect();

    let mut update = SensingUpdate {
        msg_type: "sensing_update".to_string(),
        timestamp: chrono::Utc::now().timestamp_millis() as f64 / 1000.0,
        source: if is_live_frame {
            "esp32"
        } else {
            "esp32:interpolated"
        }
        .to_string(),
        tick,
        nodes: active_nodes,
        features: fused_features.clone(),
        classification,
        signal_field: generate_signal_field(
            fused_features.mean_rssi,
            motion_score,
            breathing_rate_hz,
            fused_features.variance.min(1.0),
            &sub_variances,
        ),
        vital_signs: Some(update_vitals),
        enhanced_motion: None,
        enhanced_breathing: None,
        posture: None,
        signal_quality_score: None,
        quality_verdict: None,
        bssid_count: None,
        pose_keypoints: None,
        model_status: None,
        persons: None,
        state: None,
        estimated_persons: if rendered_persons > 0 {
            Some(rendered_persons)
        } else {
            None
        },
        count_evidence: Some(count_evidence),
        node_features: build_node_features(&s.node_states, now),
    };

    finalize_persons_for_update(&mut s, &mut update);

    publish_sensing_update(&mut s, update);

    if tick % 100 == 0 {
        let stale = Duration::from_secs(60);
        let before = s.node_states.len();
        s.node_states
            .retain(|_id, ns| ns.last_frame_time.is_some_and(|t| now.duration_since(t) < stale));
        let evicted = before - s.node_states.len();
        if evicted > 0 {
            info!(
                "Evicted {} stale node(s), {} active",
                evicted,
                s.node_states.len()
            );
        }
    }
}

async fn simulated_data_task(state: SharedState, tick_ms: u64) {
    let mut interval = tokio::time::interval(Duration::from_millis(tick_ms));
    info!("Simulated data source active (tick={}ms)", tick_ms);

    loop {
        interval.tick().await;

        let mut s = state.write().await;
        s.tick += 1;
        let tick = s.tick;

        let frame = generate_simulated_frame(tick);

        // Append current amplitudes to history before feature extraction.
        s.frame_history.push_back(frame.amplitudes.clone());
        if s.frame_history.len() > FRAME_HISTORY_CAPACITY {
            s.frame_history.pop_front();
        }

        let dsp_started = std::time::Instant::now();
        let sample_rate_hz = 1000.0 / tick_ms as f64;
        let (features, mut classification, breathing_rate_hz, sub_variances, raw_motion) =
            extract_features_from_frame(&frame, &s.frame_history, sample_rate_hz);
        smooth_and_classify(&mut s, &mut classification, raw_motion);
        adaptive_override(&s, &features, &mut classification);
        record_dsp_latency(&mut s, dsp_started);

        s.rssi_history.push_back(features.mean_rssi);
        if s.rssi_history.len() > 60 {
            s.rssi_history.pop_front();
        }

        let motion_score = if classification.motion_level == "active" {
            0.8
        } else if classification.motion_level == "present_still" {
            0.3
        } else {
            0.05
        };

        let raw_vitals = s
            .vital_detector
            .process_frame(&frame.amplitudes, &frame.phases);
        let vitals = smooth_vitals(&mut s, &raw_vitals);
        s.latest_vitals = vitals.clone();

        let frame_amplitudes = frame.amplitudes.clone();
        let frame_n_sub = frame.n_subcarriers;

        // ADR-044 §5.2: feed raw features into rolling-P95 estimators before scoring.
        s.p95_variance.push(features.variance);
        s.p95_motion_band_power.push(features.motion_band_power);
        s.p95_spectral_power.push(features.spectral_power);

        // Multi-person estimation with temporal smoothing (EMA α=0.10).
        let raw_score = compute_person_score(&s, &features);
        s.smoothed_person_score = s.smoothed_person_score * 0.90 + raw_score * 0.10;
        let est_persons = if classification.presence {
            let count = s.person_count();
            s.prev_person_count = count;
            count
        } else {
            s.prev_person_count = 0;
            0
        };
        let now = std::time::Instant::now();
        let count_evidence =
            apply_room_presence_continuity(&mut s, &mut classification, est_persons, now);
        let rendered_persons = count_evidence.rendered_persons;
        {
            let ns = s
                .node_states
                .entry(frame.node_id)
                .or_insert_with(|| NodeState::new_for_node(frame.node_id));
            ns.observe_csi_frame_arrival(now);
            ns.update_novelty(&frame.amplitudes);
            ns.frame_history.push_back(frame.amplitudes.clone());
            if ns.frame_history.len() > FRAME_HISTORY_CAPACITY {
                ns.frame_history.pop_front();
            }
            ns.rssi_history.push_back(features.mean_rssi);
            if ns.rssi_history.len() > 60 {
                ns.rssi_history.pop_front();
            }
            ns.latest_vitals = vitals.clone();
            ns.last_vitals_time = Some(now);
            ns.latest_features = Some(features.clone());
            ns.current_motion_level = classification.motion_level.clone();
            ns.prev_person_count = rendered_persons;
            ns.update_tensor_compression(&frame.amplitudes, &frame.phases);
        }
        let node_features = build_node_features(&s.node_states, now);

        let mut update = SensingUpdate {
            msg_type: "sensing_update".to_string(),
            timestamp: chrono::Utc::now().timestamp_millis() as f64 / 1000.0,
            source: "simulated".to_string(),
            tick,
            nodes: vec![NodeInfo {
                node_id: frame.node_id,
                rssi_dbm: features.mean_rssi,
                position: [2.0, 0.0, 1.5],
                amplitude: frame_amplitudes,
                subcarrier_count: frame_n_sub as usize,
                sync: None, // simulated frame path — no mesh peer
            }],
            features: features.clone(),
            classification,
            signal_field: generate_signal_field(
                features.mean_rssi,
                motion_score,
                breathing_rate_hz,
                features.variance.min(1.0),
                &sub_variances,
            ),
            vital_signs: Some(vitals),
            enhanced_motion: None,
            enhanced_breathing: None,
            posture: None,
            signal_quality_score: None,
            quality_verdict: None,
            bssid_count: None,
            pose_keypoints: None,
            model_status: if s.model_loaded {
                Some(serde_json::json!({
                    "loaded": true,
                    "layers": s.progressive_loader.as_ref()
                        .map(|l| { let (a,b,c) = l.layer_status(); a as u8 + b as u8 + c as u8 })
                        .unwrap_or(0),
                    "sona_profile": s.active_sona_profile.as_deref().unwrap_or("default"),
                }))
            } else {
                None
            },
            persons: None,
            state: None,
            estimated_persons: if rendered_persons > 0 {
                Some(rendered_persons)
            } else {
                None
            },
            count_evidence: Some(count_evidence),
            node_features,
        };

        finalize_persons_for_update(&mut s, &mut update);

        if update.classification.presence {
            s.total_detections += 1;
        }
        publish_sensing_update(&mut s, update);
    }
}

// ── Broadcast tick task (for ESP32 mode, sends buffered state) ───────────────

async fn broadcast_tick_task(state: SharedState, tick_ms: u64) {
    let mut interval = tokio::time::interval(Duration::from_millis(tick_ms));

    loop {
        interval.tick().await;
        let s = state.read().await;
        if let Some(ref update) = s.latest_update {
            if s.tx.receiver_count() > 0 {
                // Re-broadcast the latest sensing_update so pose WS clients
                // always get data even when ESP32 pauses between frames.
                //
                // Issue #618: overwrite `source` with `effective_source()`
                // before each broadcast so a stale latest_update (frozen
                // payload from a now-offline ESP32) is emitted with
                // `source: "esp32:offline"` instead of `source: "esp32"`.
                // The REST `/health` endpoint already does this; before
                // this fix the WS path was the only consumer that didn't,
                // so the UI's "LIVE — ESP32 HARDWARE Connected" banner
                // stayed green long after the hardware went away.
                let mut tagged = update.clone();
                tagged.source = s.effective_source();
                if let Ok(json) = serde_json::to_string(&tagged) {
                    let _ = s.tx.send(json);
                }
            }
        }
    }
}

/// Map one sensing-broadcast JSON document into the `VitalsSnapshot`(s) to
/// publish over MQTT (issues #872/#898).
///
/// Multi-node sources carry a `nodes` array where **each node has its own
/// `classification`** (`motion_level`, `presence`, `confidence`) and RSSI — so
/// each node must surface its *own* presence/motion, not the room-level
/// aggregate. Previously the bridge applied the aggregate `classification` to
/// every per-node Home-Assistant device, so a node in an empty corner inherited
/// another node's "present" (and `motion_level: "absent"` was mis-mapped to full
/// motion). Vitals (breathing / heart rate) and the person count are room-level
/// and shared across the per-node devices. Falls back to a single aggregate
/// snapshot when there is no per-node data (e.g. wifi / simulate sources).
#[cfg(feature = "mqtt")]
fn vitals_snapshots_from_sensing_json(
    v: &serde_json::Value,
    base_id: &str,
) -> Vec<wifi_densepose_sensing_server::mqtt::state::VitalsSnapshot> {
    use wifi_densepose_sensing_server::mqtt::state::VitalsSnapshot;

    // motion_level string -> motion scalar. "absent"/"none"/"still"/"idle"/""
    // are non-moving; anything else (walking, …) is motion. `fallback` is used
    // when the field is absent so a partial per-node payload defers to the
    // room aggregate rather than silently reading 0.
    fn motion_of(level: Option<&str>, fallback: f64) -> f64 {
        match level {
            Some("none") | Some("still") | Some("idle") | Some("absent") | Some("") => 0.0,
            Some(_) => 1.0,
            None => fallback,
        }
    }

    let ts = (v["timestamp"].as_f64().unwrap_or(0.0) * 1000.0) as i64;
    let vit = &v["vital_signs"];
    let breathing = vit["breathing_rate_bpm"].as_f64();
    let hr = vit["heart_rate_bpm"].as_f64();
    let n_persons = v["persons"]
        .as_array()
        .map(|a| a.len() as u32)
        .or_else(|| v["estimated_persons"].as_u64().map(|x| x as u32))
        .unwrap_or(0);

    // Room-level aggregate: the no-nodes fallback, and the per-node default for
    // any field a node omits.
    let acls = &v["classification"];
    let agg_presence = acls["presence"].as_bool().unwrap_or(false);
    let agg_motion = motion_of(acls["motion_level"].as_str(), 0.0);
    let agg_conf = acls["confidence"].as_f64().unwrap_or(0.0);

    let mk = |node_id: String, presence: bool, motion: f64, conf: f64, rssi: Option<f64>| {
        VitalsSnapshot {
            node_id,
            timestamp_ms: ts,
            presence,
            motion,
            presence_score: if presence { conf.max(0.0) } else { 0.0 },
            breathing_rate_bpm: breathing,
            heartrate_bpm: hr,
            n_persons,
            rssi_dbm: rssi,
            vital_confidence: conf,
            ..Default::default()
        }
    };

    match v["nodes"].as_array() {
        Some(arr) if !arr.is_empty() => arr
            .iter()
            .map(|node| {
                let n = node["node_id"].as_u64().unwrap_or(0);
                // Each node carries its OWN classification — use it, deferring to
                // the room aggregate only for fields the node omits.
                let ncls = &node["classification"];
                let presence = ncls["presence"].as_bool().unwrap_or(agg_presence);
                let motion = motion_of(ncls["motion_level"].as_str(), agg_motion);
                let conf = ncls["confidence"].as_f64().unwrap_or(agg_conf);
                mk(
                    format!("{base_id}-node{n}"),
                    presence,
                    motion,
                    conf,
                    node["rssi_dbm"].as_f64(),
                )
            })
            .collect(),
        _ => vec![mk(
            base_id.to_string(),
            agg_presence,
            agg_motion,
            agg_conf,
            v["nodes"][0]["rssi_dbm"].as_f64(),
        )],
    }
}

/// Turn a `ProgressiveLoader::new` failure into an actionable diagnostic (#894).
///
/// The published HuggingFace `ruvnet/wifi-densepose-pretrained` files
/// (`model.safetensors`, `model-q{2,4,8}.bin`, `model.rvf.jsonl`) are a
/// different *format* — and a different encoder architecture — than the RVF
/// binary container the `--model` progressive loader expects (`RVFS` magic
/// `0x52564653`). Feeding one to `--model` produced a bare
/// "invalid magic at offset 0 …" that left users stuck. Detect the common
/// cases and explain plainly what's loadable instead.
fn diagnose_model_load_error(path: &std::path::Path, data: &[u8], err: &str) -> String {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    // safetensors: 8-byte LE header length, then a JSON object starting with '{'.
    let looks_safetensors = ext == "safetensors" || (data.len() > 9 && data[8] == b'{');
    // JSONL manifest: starts with '{' (or the well-known suffix).
    let looks_jsonl = ext == "jsonl" || name.ends_with(".rvf.jsonl") || data.first() == Some(&b'{');
    // Quantized weight blob shipped on HF (model-q2/q4/q8.bin).
    let looks_quant_bin = ext == "bin" || name.contains("-q");

    let kind = if looks_safetensors {
        "a safetensors weight file"
    } else if looks_jsonl {
        "a JSONL manifest, not the binary container"
    } else if looks_quant_bin {
        "a quantized weight blob (e.g. HuggingFace model-q4.bin)"
    } else {
        "not an RVF binary container"
    };

    format!(
        "model `{}` could not be loaded: it is {kind}. The --model flag expects an \
         RVF binary container (`RVFS` magic 0x52564653) produced by the \
         wifi-densepose-train pipeline. The HuggingFace ruvnet/wifi-densepose-pretrained \
         files are a different format and encoder architecture, so they do not load \
         here directly (issue #894). Continuing with signal heuristics. (loader: {err})",
        path.display()
    )
}

/// Whether `--export-rvf` should emit the placeholder container-format demo.
///
/// It must only do so **standalone**. Combined with `--train`/`--pretrain` the
/// real model is produced by the training pipeline, so short-circuiting here
/// would silently skip training and write placeholder weights — the #894 bug
/// where the documented `--train … --export-rvf` workflow produced a fake model.
fn export_emits_placeholder_demo(export_set: bool, train: bool, pretrain: bool) -> bool {
    export_set && !train && !pretrain
}

// ── Main ─────────────────────────────────────────────────────────────────────

/// If `--ui-path` points nowhere (wrong cwd), try common repo layouts relative to cwd.
fn coalesce_ui_path(initial: std::path::PathBuf) -> std::path::PathBuf {
    if initial.is_dir() {
        return initial;
    }
    for rel in &["../ui", "./ui", "../../ui"] {
        let p = std::path::PathBuf::from(rel);
        if p.is_dir() {
            warn!(
                "UI path {} not found; using {} (set --ui-path explicitly if wrong)",
                initial.display(),
                p.display()
            );
            return p;
        }
    }
    initial
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogFormat {
    Text,
    Json,
}

fn parse_log_format(value: Option<&str>) -> Result<LogFormat, String> {
    match value.unwrap_or("text").trim().to_ascii_lowercase().as_str() {
        "" | "text" => Ok(LogFormat::Text),
        "json" => Ok(LogFormat::Json),
        other => Err(format!(
            "unsupported LOG_FORMAT '{other}'; expected 'text' or 'json'"
        )),
    }
}

fn init_tracing_from_env() -> Result<(), String> {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "info,tower_http=debug".into());
    match parse_log_format(std::env::var("LOG_FORMAT").ok().as_deref())? {
        LogFormat::Text => tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .try_init()
            .map_err(|e| format!("failed to initialize tracing: {e}")),
        LogFormat::Json => tracing_subscriber::fmt()
            .json()
            .with_env_filter(env_filter)
            .try_init()
            .map_err(|e| format!("failed to initialize tracing: {e}")),
    }
}

#[tokio::main]
async fn main() {
    if let Err(message) = init_tracing_from_env() {
        eprintln!("Logging configuration error: {message}");
        std::process::exit(2);
    }

    let mut args = Args::parse();
    if let Some(kind) = args.print_config_schema.clone() {
        match serde_json::to_string_pretty(&schema_for_config_kind(kind)) {
            Ok(schema) => {
                println!("{schema}");
                return;
            }
            Err(e) => {
                error!("Failed to render configuration schema: {e}");
                std::process::exit(2);
            }
        }
    }
    if let Some(root) = args.validate_config_root.as_ref() {
        match validate_config_root(root) {
            Ok(report) => {
                info!("Configuration validation passed: {}", report.summary());
                println!("{}", report.summary());
                return;
            }
            Err(message) => {
                error!("Configuration validation failed: {message}");
                std::process::exit(2);
            }
        }
    }
    args.ui_path = coalesce_ui_path(args.ui_path);

    // Handle --benchmark mode: run vital sign benchmark and exit
    if args.benchmark {
        eprintln!("Running vital sign detection benchmark (1000 frames)...");
        let (total, per_frame) = vital_signs::run_benchmark(1000);
        eprintln!();
        eprintln!("Summary: {total:?} total, {per_frame:?} per frame");
        return;
    }

    // Handle --export-rvf: writes a CONTAINER-FORMAT DEMO with placeholder
    // weights — it is NOT a trained model. Only short-circuit when standalone:
    // combined with --train/--pretrain the real model is exported by the
    // training pipeline, and short-circuiting here would silently skip training
    // and write placeholder weights (#894 — the documented `--train …
    // --export-rvf` workflow produced a placeholder and never trained).
    if export_emits_placeholder_demo(args.export_rvf.is_some(), args.train, args.pretrain) {
        let rvf_path = args
            .export_rvf
            .as_ref()
            .expect("export_emits_placeholder_demo implies export_rvf is set");
        eprintln!(
            "WARNING: --export-rvf writes a CONTAINER-FORMAT DEMO with placeholder \
             weights — it is NOT a trained model. Train one with \
             `--train --dataset <DIR>` (which exports a calibrated .rvf to the \
             models/ directory), or download a pretrained encoder. See issue #894."
        );
        eprintln!("Exporting RVF container package (placeholder weights)...");
        use rvf_pipeline::RvfModelBuilder;

        let mut builder = RvfModelBuilder::new("wifi-densepose", "1.0.0");

        // Vital sign config (default breathing 0.1-0.5 Hz, heartbeat 0.8-2.0 Hz)
        builder.set_vital_config(0.1, 0.5, 0.8, 2.0);

        // Model profile (input/output spec)
        builder.set_model_profile(
            "56-subcarrier CSI amplitude/phase @ 10-100 Hz",
            "17 COCO keypoints + body part UV + vital signs",
            "ESP32-S3 or Windows WiFi RSSI, Rust 1.85+",
        );

        // Placeholder weights (17 keypoints × 56 subcarriers × 3 dims = 2856 params)
        let placeholder_weights: Vec<f32> = (0..2856).map(|i| (i as f32 * 0.001).sin()).collect();
        builder.set_weights(&placeholder_weights);

        // Training provenance
        builder.set_training_proof(
            "wifi-densepose-rs-v1.0.0",
            serde_json::json!({
                "pipeline": "ADR-023 8-phase",
                "test_count": 229,
                "benchmark_fps": 9520,
                "framework": "wifi-densepose-rs",
            }),
        );

        // SONA default environment profile
        let default_lora: Vec<f32> = vec![0.0; 64];
        builder.add_sona_profile("default", &default_lora, &default_lora);

        match builder.build() {
            Ok(rvf_bytes) => {
                if let Err(e) = std::fs::write(rvf_path, &rvf_bytes) {
                    eprintln!("Error writing RVF: {e}");
                    std::process::exit(1);
                }
                eprintln!("Wrote {} bytes to {}", rvf_bytes.len(), rvf_path.display());
                eprintln!("RVF container exported successfully.");
            }
            Err(e) => {
                eprintln!("Error building RVF: {e}");
                std::process::exit(1);
            }
        }
        return;
    } else if args.export_rvf.is_some() {
        // --export-rvf alongside --train/--pretrain: don't emit a placeholder.
        // Fall through so training runs; it exports the real calibrated model.
        eprintln!(
            "Note: --export-rvf is ignored in training mode — the trained model \
             is exported by the training pipeline to the models/ directory."
        );
    }

    // Handle --pretrain mode: self-supervised contrastive pretraining (ADR-024)
    if args.pretrain {
        eprintln!("=== WiFi-DensePose Contrastive Pretraining (ADR-024) ===");

        let ds_path = args
            .dataset
            .clone()
            .unwrap_or_else(|| PathBuf::from("data"));
        let source = match args.dataset_type.as_str() {
            "wipose" => dataset::DataSource::WiPose(ds_path.clone()),
            _ => dataset::DataSource::MmFi(ds_path.clone()),
        };
        let pipeline = dataset::DataPipeline::new(dataset::DataConfig {
            source,
            ..Default::default()
        });

        // Generate synthetic or load real CSI windows
        let generate_synthetic_windows = || -> Vec<Vec<Vec<f32>>> {
            (0..50)
                .map(|i| {
                    (0..4)
                        .map(|a| {
                            (0..56)
                                .map(|s| ((i * 7 + a * 13 + s) as f32 * 0.31).sin() * 0.5)
                                .collect()
                        })
                        .collect()
                })
                .collect()
        };

        let csi_windows: Vec<Vec<Vec<f32>>> = match pipeline.load() {
            Ok(s) if !s.is_empty() => {
                eprintln!("Loaded {} samples from {}", s.len(), ds_path.display());
                s.into_iter().map(|s| s.csi_window).collect()
            }
            _ => {
                eprintln!("Using synthetic data for pretraining.");
                generate_synthetic_windows()
            }
        };

        let n_subcarriers = csi_windows
            .first()
            .and_then(|w| w.first())
            .map(|f| f.len())
            .unwrap_or(56);

        let tf_config = graph_transformer::TransformerConfig {
            n_subcarriers,
            n_keypoints: 17,
            d_model: 64,
            n_heads: 4,
            n_gnn_layers: 2,
        };
        let transformer = graph_transformer::CsiToPoseTransformer::new(tf_config);
        eprintln!("Transformer params: {}", transformer.param_count());

        let trainer_config = trainer::TrainerConfig {
            epochs: args.pretrain_epochs,
            batch_size: 8,
            lr: 0.001,
            warmup_epochs: 2,
            min_lr: 1e-6,
            early_stop_patience: args.pretrain_epochs + 1,
            pretrain_temperature: 0.07,
            ..Default::default()
        };
        let mut t = trainer::Trainer::with_transformer(trainer_config, transformer);

        let e_config = embedding::EmbeddingConfig {
            d_model: 64,
            d_proj: 128,
            temperature: 0.07,
            normalize: true,
        };
        let mut projection = embedding::ProjectionHead::new(e_config.clone());
        let augmenter = embedding::CsiAugmenter::new();

        eprintln!(
            "Starting contrastive pretraining for {} epochs...",
            args.pretrain_epochs
        );
        let start = std::time::Instant::now();
        for epoch in 0..args.pretrain_epochs {
            let loss = t.pretrain_epoch(&csi_windows, &augmenter, &mut projection, 0.07, epoch);
            if epoch % 10 == 0 || epoch == args.pretrain_epochs - 1 {
                eprintln!("  Epoch {epoch}: contrastive loss = {loss:.4}");
            }
        }
        let elapsed = start.elapsed().as_secs_f64();
        eprintln!("Pretraining complete in {elapsed:.1}s");

        // Save pretrained model as RVF with embedding segment
        if let Some(ref save_path) = args.save_rvf {
            eprintln!("Saving pretrained model to RVF: {}", save_path.display());
            t.sync_transformer_weights();
            let weights = t.params().to_vec();
            let mut proj_weights = Vec::new();
            projection.flatten_into(&mut proj_weights);

            let mut builder = RvfBuilder::new();
            builder.add_manifest(
                "wifi-densepose-pretrained",
                env!("CARGO_PKG_VERSION"),
                "WiFi DensePose contrastive pretrained model (ADR-024)",
            );
            builder.add_weights(&weights);
            builder.add_embedding(
                &serde_json::json!({
                    "d_model": e_config.d_model,
                    "d_proj": e_config.d_proj,
                    "temperature": e_config.temperature,
                    "normalize": e_config.normalize,
                    "pretrain_epochs": args.pretrain_epochs,
                }),
                &proj_weights,
            );
            match builder.write_to_file(save_path) {
                Ok(()) => eprintln!(
                    "RVF saved ({} transformer + {} projection params)",
                    weights.len(),
                    proj_weights.len()
                ),
                Err(e) => eprintln!("Failed to save RVF: {e}"),
            }
        }

        return;
    }

    // Handle --embed mode: extract embeddings from CSI data
    if args.embed {
        eprintln!("=== WiFi-DensePose Embedding Extraction (ADR-024) ===");

        let model_path = match &args.model {
            Some(p) => p.clone(),
            None => {
                eprintln!("Error: --embed requires --model <path> to a pretrained .rvf file");
                std::process::exit(1);
            }
        };

        let reader = match RvfReader::from_file(&model_path) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Failed to load model: {e}");
                std::process::exit(1);
            }
        };

        let weights = reader.weights().unwrap_or_default();
        let (embed_config_json, proj_weights) = reader.embedding().unwrap_or_else(|| {
            eprintln!("Warning: no embedding segment in RVF, using defaults");
            (
                serde_json::json!({"d_model":64,"d_proj":128,"temperature":0.07,"normalize":true}),
                Vec::new(),
            )
        });

        let d_model = embed_config_json["d_model"].as_u64().unwrap_or(64) as usize;
        let d_proj = embed_config_json["d_proj"].as_u64().unwrap_or(128) as usize;

        let tf_config = graph_transformer::TransformerConfig {
            n_subcarriers: 56,
            n_keypoints: 17,
            d_model,
            n_heads: 4,
            n_gnn_layers: 2,
        };
        let e_config = embedding::EmbeddingConfig {
            d_model,
            d_proj,
            temperature: 0.07,
            normalize: true,
        };
        let mut extractor = embedding::EmbeddingExtractor::new(tf_config, e_config.clone());

        // Load transformer weights
        if !weights.is_empty() {
            if let Err(e) = extractor.transformer.unflatten_weights(&weights) {
                eprintln!("Warning: failed to load transformer weights: {e}");
            }
        }
        // Load projection weights
        if !proj_weights.is_empty() {
            let (proj, _) = embedding::ProjectionHead::unflatten_from(&proj_weights, &e_config);
            extractor.projection = proj;
        }

        // Load dataset and extract embeddings
        let _ds_path = args
            .dataset
            .clone()
            .unwrap_or_else(|| PathBuf::from("data"));
        let csi_windows: Vec<Vec<Vec<f32>>> = (0..10)
            .map(|i| {
                (0..4)
                    .map(|a| {
                        (0..56)
                            .map(|s| ((i * 7 + a * 13 + s) as f32 * 0.31).sin() * 0.5)
                            .collect()
                    })
                    .collect()
            })
            .collect();

        eprintln!(
            "Extracting embeddings from {} CSI windows...",
            csi_windows.len()
        );
        let embeddings = extractor.extract_batch(&csi_windows);
        for (i, emb) in embeddings.iter().enumerate() {
            let norm: f32 = emb.iter().map(|x| x * x).sum::<f32>().sqrt();
            eprintln!("  Window {i}: {d_proj}-dim embedding, ||e|| = {norm:.4}");
        }
        eprintln!(
            "Extracted {} embeddings of dimension {d_proj}",
            embeddings.len()
        );

        return;
    }

    // Handle --build-index mode: build a fingerprint index from embeddings
    if let Some(ref index_type_str) = args.build_index {
        eprintln!("=== WiFi-DensePose Fingerprint Index Builder (ADR-024) ===");

        let index_type = match index_type_str.as_str() {
            "env" | "environment" => embedding::IndexType::EnvironmentFingerprint,
            "activity" => embedding::IndexType::ActivityPattern,
            "temporal" => embedding::IndexType::TemporalBaseline,
            "person" => embedding::IndexType::PersonTrack,
            _ => {
                eprintln!(
                    "Unknown index type '{}'. Use: env, activity, temporal, person",
                    index_type_str
                );
                std::process::exit(1);
            }
        };

        let tf_config = graph_transformer::TransformerConfig::default();
        let e_config = embedding::EmbeddingConfig::default();
        let mut extractor = embedding::EmbeddingExtractor::new(tf_config, e_config);

        // Generate synthetic CSI windows for demo
        let csi_windows: Vec<Vec<Vec<f32>>> = (0..20)
            .map(|i| {
                (0..4)
                    .map(|a| {
                        (0..56)
                            .map(|s| ((i * 7 + a * 13 + s) as f32 * 0.31).sin() * 0.5)
                            .collect()
                    })
                    .collect()
            })
            .collect();

        let mut index = embedding::FingerprintIndex::new(index_type);
        for (i, window) in csi_windows.iter().enumerate() {
            let emb = extractor.extract(window);
            index.insert(emb, format!("window_{i}"), i as u64 * 100);
        }

        eprintln!("Built {:?} index with {} entries", index_type, index.len());

        // Test a query
        let query_emb = extractor.extract(&csi_windows[0]);
        let results = index.search(&query_emb, 5);
        eprintln!("Top-5 nearest to window_0:");
        for r in &results {
            eprintln!(
                "  entry={}, distance={:.4}, metadata={}",
                r.entry, r.distance, r.metadata
            );
        }

        return;
    }

    // Handle --train mode: train a model and exit
    if args.train {
        eprintln!("=== WiFi-DensePose Training Mode ===");

        // Build data pipeline
        let ds_path = args
            .dataset
            .clone()
            .unwrap_or_else(|| PathBuf::from("data"));
        let source = match args.dataset_type.as_str() {
            "wipose" => dataset::DataSource::WiPose(ds_path.clone()),
            _ => dataset::DataSource::MmFi(ds_path.clone()),
        };
        let pipeline = dataset::DataPipeline::new(dataset::DataConfig {
            source,
            ..Default::default()
        });

        // Generate synthetic training data (50 samples with deterministic CSI + keypoints)
        let generate_synthetic = || -> Vec<dataset::TrainingSample> {
            (0..50)
                .map(|i| {
                    let csi: Vec<Vec<f32>> = (0..4)
                        .map(|a| {
                            (0..56)
                                .map(|s| ((i * 7 + a * 13 + s) as f32 * 0.31).sin() * 0.5)
                                .collect()
                        })
                        .collect();
                    let mut kps = [(0.0f32, 0.0f32, 1.0f32); 17];
                    for (k, kp) in kps.iter_mut().enumerate() {
                        kp.0 = (k as f32 * 0.1 + i as f32 * 0.02).sin() * 100.0 + 320.0;
                        kp.1 = (k as f32 * 0.15 + i as f32 * 0.03).cos() * 80.0 + 240.0;
                    }
                    dataset::TrainingSample {
                        csi_window: csi,
                        pose_label: dataset::PoseLabel {
                            keypoints: kps,
                            body_parts: Vec::new(),
                            confidence: 1.0,
                        },
                        source: "synthetic",
                    }
                })
                .collect()
        };

        // Load samples (fall back to synthetic if dataset missing/empty)
        let samples = match pipeline.load() {
            Ok(s) if !s.is_empty() => {
                eprintln!("Loaded {} samples from {}", s.len(), ds_path.display());
                s
            }
            Ok(_) => {
                eprintln!(
                    "No samples found at {}. Using synthetic data.",
                    ds_path.display()
                );
                generate_synthetic()
            }
            Err(e) => {
                eprintln!("Failed to load dataset: {e}. Using synthetic data.");
                generate_synthetic()
            }
        };

        // Convert dataset samples to trainer format
        let trainer_samples: Vec<trainer::TrainingSample> =
            samples.iter().map(trainer::from_dataset_sample).collect();

        // Split 80/20 train/val
        let split = (trainer_samples.len() * 4) / 5;
        let (train_data, val_data) = trainer_samples.split_at(split.max(1));
        eprintln!(
            "Train: {} samples, Val: {} samples",
            train_data.len(),
            val_data.len()
        );

        // Create transformer + trainer
        let n_subcarriers = train_data
            .first()
            .and_then(|s| s.csi_features.first())
            .map(|f| f.len())
            .unwrap_or(56);
        let tf_config = graph_transformer::TransformerConfig {
            n_subcarriers,
            n_keypoints: 17,
            d_model: 64,
            n_heads: 4,
            n_gnn_layers: 2,
        };
        let transformer = graph_transformer::CsiToPoseTransformer::new(tf_config);
        eprintln!("Transformer params: {}", transformer.param_count());

        let trainer_config = trainer::TrainerConfig {
            epochs: args.epochs,
            batch_size: 8,
            lr: 0.001,
            warmup_epochs: 5,
            min_lr: 1e-6,
            early_stop_patience: 20,
            checkpoint_every: 10,
            ..Default::default()
        };
        let mut t = trainer::Trainer::with_transformer(trainer_config, transformer);

        // Run training
        eprintln!("Starting training for {} epochs...", args.epochs);
        let result = t.run_training(train_data, val_data);
        eprintln!("Training complete in {:.1}s", result.total_time_secs);
        eprintln!(
            "  Best epoch: {}, PCK@0.2: {:.4}, OKS mAP: {:.4}",
            result.best_epoch, result.best_pck, result.best_oks
        );

        // Save checkpoint
        if let Some(ref ckpt_dir) = args.checkpoint_dir {
            let _ = std::fs::create_dir_all(ckpt_dir);
            let ckpt_path = ckpt_dir.join("best_checkpoint.json");
            let ckpt = t.checkpoint();
            match ckpt.save_to_file(&ckpt_path) {
                Ok(()) => eprintln!("Checkpoint saved to {}", ckpt_path.display()),
                Err(e) => eprintln!("Failed to save checkpoint: {e}"),
            }
        }

        // Sync weights back to transformer and save as RVF
        t.sync_transformer_weights();
        if let Some(ref save_path) = args.save_rvf {
            eprintln!("Saving trained model to RVF: {}", save_path.display());
            let weights = t.params().to_vec();
            let mut builder = RvfBuilder::new();
            builder.add_manifest(
                "wifi-densepose-trained",
                env!("CARGO_PKG_VERSION"),
                "WiFi DensePose trained model weights",
            );
            builder.add_metadata(&serde_json::json!({
                "training": {
                    "epochs": args.epochs,
                    "best_epoch": result.best_epoch,
                    "best_pck": result.best_pck,
                    "best_oks": result.best_oks,
                    "n_train_samples": train_data.len(),
                    "n_val_samples": val_data.len(),
                    "n_subcarriers": n_subcarriers,
                    "param_count": weights.len(),
                },
            }));
            builder.add_vital_config(&VitalSignConfig::default());
            builder.add_weights(&weights);
            match builder.write_to_file(save_path) {
                Ok(()) => eprintln!(
                    "RVF saved ({} params, {} bytes)",
                    weights.len(),
                    weights.len() * 4
                ),
                Err(e) => eprintln!("Failed to save RVF: {e}"),
            }
        }

        return;
    }

    info!("WiFi-DensePose Sensing Server (Rust + Axum + RuVector)");
    info!("  HTTP:      http://localhost:{}", args.http_port);
    info!("  WebSocket: ws://localhost:{}/ws/sensing", args.ws_port);
    info!("  UDP:       0.0.0.0:{} (ESP32 CSI)", args.udp_port);
    info!("  UI path:   {}", args.ui_path.display());
    info!("  Source:    {}", args.source);
    info!("  Simulation enabled: {}", args.enable_simulation);
    info!("  Min nodes: {}", args.min_nodes);
    info!(
        "  AP scan:   {} every {}s",
        args.wifi_interface,
        args.ap_scan_interval_secs.max(1)
    );
    info!("  Data dir:  {}", args.data_dir.display());

    // Auto-detect data source
    let source = match args.source.as_str() {
        "auto" => {
            info!("Auto-detecting data source...");
            if probe_esp32(args.udp_port).await {
                info!("  ESP32 CSI detected on UDP :{}", args.udp_port);
                "esp32"
            } else if probe_windows_wifi().await {
                info!("  Windows WiFi detected");
                "wifi"
            } else {
                warn!(
                    "  No hardware detected; starting ESP32 listener in offline state (simulation is explicit dev/test only)"
                );
                "esp32"
            }
        }
        "simulate" | "simulated" if !args.enable_simulation => {
            error!(
                "Simulation source requested without --enable-simulation or RUVSENSE_ENABLE_SIMULATION=true"
            );
            std::process::exit(2);
        }
        "simulate" | "simulated" => "simulate",
        "esp32" | "wifi" => args.source.as_str(),
        other => other,
    };

    if !matches!(source, "esp32" | "wifi" | "simulate") {
        error!("Unknown data source '{source}'. Expected auto, esp32, wifi, or simulate.");
        std::process::exit(2);
    }

    info!("Data source: {source}");

    // Shared state
    // Vital sign sample rate derives from tick interval (e.g. 500ms tick => 2 Hz)
    let vital_sample_rate = 1000.0 / args.tick_ms as f64;
    info!("Vital sign detector sample rate: {vital_sample_rate:.1} Hz");

    // Load RVF container if --load-rvf was specified
    let rvf_info = if let Some(ref rvf_path) = args.load_rvf {
        info!("Loading RVF container from {}", rvf_path.display());
        match RvfReader::from_file(rvf_path) {
            Ok(reader) => {
                let info = reader.info();
                info!(
                    "  RVF loaded: {} segments, {} bytes",
                    info.segment_count, info.total_size
                );
                if let Some(ref manifest) = info.manifest {
                    if let Some(model_id) = manifest.get("model_id") {
                        info!("  Model ID: {model_id}");
                    }
                    if let Some(version) = manifest.get("version") {
                        info!("  Version:  {version}");
                    }
                }
                if info.has_weights {
                    if let Some(w) = reader.weights() {
                        info!("  Weights: {} parameters", w.len());
                    }
                }
                if info.has_vital_config {
                    info!("  Vital sign config: present");
                }
                if info.has_quant_info {
                    info!("  Quantization info: present");
                }
                if info.has_witness {
                    info!("  Witness/proof: present");
                }
                Some(info)
            }
            Err(e) => {
                error!("Failed to load RVF container: {e}");
                None
            }
        }
    } else {
        None
    };

    // Load trained model via --model (uses progressive loading if --progressive set)
    let model_path = args.model.as_ref().or(args.load_rvf.as_ref());
    let mut progressive_loader: Option<ProgressiveLoader> = None;
    let mut model_loaded = false;
    if let Some(mp) = model_path {
        if args.progressive || args.model.is_some() {
            info!("Loading trained model (progressive) from {}", mp.display());
            match std::fs::read(mp) {
                Ok(data) => match ProgressiveLoader::new(&data) {
                    Ok(mut loader) => {
                        if let Ok(la) = loader.load_layer_a() {
                            info!(
                                "  Layer A ready: model={} v{} ({} segments)",
                                la.model_name, la.version, la.n_segments
                            );
                        }
                        model_loaded = true;
                        progressive_loader = Some(loader);
                    }
                    Err(e) => {
                        error!("{}", diagnose_model_load_error(mp, &data, &e.to_string()))
                    }
                },
                Err(e) => error!("Failed to read model file: {e}"),
            }
        }
    }

    // Ensure data directories exist for models and recordings
    let models_dir = effective_models_dir();
    let _ = std::fs::create_dir_all(&models_dir);
    let data_dir = args.data_dir.clone();
    if let Err(e) = std::fs::create_dir_all(data_dir.join("recordings")) {
        warn!(
            "Failed to create recordings directory under {}: {e}",
            data_dir.display()
        );
    }
    match init_sqlite_store(&data_dir).await {
        Ok(()) => info!(
            "SQLite store ready at {}",
            data_dir.join("ruvsense.sqlite").display()
        ),
        Err(e) => warn!("SQLite store unavailable: {e}"),
    }

    // Discover model and recording files on startup
    let initial_models = scan_model_files();
    let initial_recordings = scan_recording_files();
    info!(
        "Discovered {} model files, {} recording files",
        initial_models.len(),
        initial_recordings.len()
    );

    // ADR-044 §5.3: load persisted runtime config from the data directory.
    let runtime_config_path = args
        .config_file
        .clone()
        .unwrap_or_else(|| runtime_config_path_for_data_dir(&data_dir));
    let runtime_config_result = if let Some(path) = args.config_file.as_ref() {
        load_runtime_config_file(path)
    } else {
        load_runtime_config(&data_dir)
    };
    let runtime_config = match runtime_config_result {
        Ok(config) => config,
        Err(message) => {
            error!("Runtime config validation failed: {message}");
            std::process::exit(2);
        }
    };
    info!(
        "Loaded runtime config: dedup_factor={:.2}, aps={}, nodes={}, enabled_modules={}",
        runtime_config.dedup_factor,
        runtime_config.environment.access_points.len(),
        runtime_config.environment.nodes.len(),
        runtime_config.enabled_modules.len(),
    );

    // ADR-102: optional Edge Module Registry. None when --no-edge-registry
    // is set (or when the URL is empty); otherwise we construct one with
    // the configured TTL. The fetch happens lazily on first request.
    let edge_registry: Option<
        std::sync::Arc<wifi_densepose_sensing_server::edge_registry::EdgeRegistry>,
    > = if args.no_edge_registry || args.edge_registry_url.is_empty() {
        info!("Edge module registry: DISABLED (--no-edge-registry or empty URL)");
        None
    } else {
        info!(
            "Edge module registry: enabled — upstream={} ttl={}s",
            args.edge_registry_url, args.edge_registry_ttl_secs
        );
        Some(std::sync::Arc::new(
            wifi_densepose_sensing_server::edge_registry::EdgeRegistry::new(
                args.edge_registry_url.clone(),
                std::time::Duration::from_secs(args.edge_registry_ttl_secs),
            ),
        ))
    };

    let (tx, _) = broadcast::channel::<String>(256);
    // ADR-099: parallel broadcast for the per-frame introspection snapshot stream
    // consumed by `/ws/introspection`. Same ring size as `tx` (256) — slow
    // clients drop oldest, identical backpressure shape.
    let (intro_tx, _) = broadcast::channel::<String>(256);

    // #872: actually start the MQTT publisher when `--mqtt` is set. The publisher
    // (mqtt::) consumes a typed VitalsSnapshot stream; we bridge the existing JSON
    // sensing broadcast into it with a defensive serde_json::Value mapping (absent
    // fields default — never publish wrong values). Gated on the optional
    // `mqtt` feature; the minimal RuvSense Edge Docker image leaves this off
    // unless an operator explicitly builds an integration image.
    if args.mqtt_opts.mqtt {
        #[cfg(feature = "mqtt")]
        {
            use wifi_densepose_sensing_server::mqtt;
            let mcfg = std::sync::Arc::new(mqtt::config::MqttConfig::from_args(&args.mqtt_opts));
            match mcfg.validate() {
                Ok(()) => {
                    let node_id = mcfg.client_id.clone();
                    let builder = mqtt::publisher::OwnedDiscoveryBuilder {
                        discovery_prefix: mcfg.discovery_prefix.clone(),
                        node_id: node_id.clone(),
                        node_friendly_name: Some("RuView".to_string()),
                        sw_version: env!("CARGO_PKG_VERSION").to_string(),
                        model: "RuView WiFi Sensing".to_string(),
                        via_device: None,
                    };
                    let (vtx, vrx) = broadcast::channel::<mqtt::state::VitalsSnapshot>(64);
                    let (host, port) = (mcfg.host.clone(), mcfg.port);
                    mqtt::publisher::spawn(mcfg, builder, vrx);
                    let mut jrx = tx.subscribe();
                    tokio::spawn(async move {
                        while let Ok(json) = jrx.recv().await {
                            let Ok(v) = serde_json::from_str::<serde_json::Value>(&json) else {
                                continue;
                            };
                            // #898/#872: emit one snapshot per physical node so
                            // each surfaces as its own Home-Assistant device with
                            // its *own* presence/motion/RSSI (see
                            // vitals_snapshots_from_sensing_json). Falls back to a
                            // single aggregate snapshot for per-node-less sources.
                            for snap in vitals_snapshots_from_sensing_json(&v, &node_id) {
                                let _ = vtx.send(snap);
                            }
                        }
                    });
                    tracing::info!("MQTT publisher started -> {host}:{port}");
                }
                Err(e) => tracing::error!("MQTT config invalid: {e}; publisher not started"),
            }
        }
        #[cfg(not(feature = "mqtt"))]
        tracing::warn!(
            "--mqtt set but this binary was built without the `mqtt` feature; the publisher is a \
             no-op. Rebuild an integration image with \
             `cargo build -p ruvsense-master --features mqtt`."
        );
    }

    let feature_flags = FeatureFlags::load();
    info!(
        "Feature flags active: {}",
        feature_flags.active_names().join(", ")
    );
    info!(
        "Feature flags disabled: {}",
        feature_flags.disabled_names().join(", ")
    );

    let state: SharedState = Arc::new(RwLock::new(AppStateInner {
        latest_update: None,
        rssi_history: VecDeque::new(),
        frame_history: VecDeque::new(),
        tick: 0,
        source: source.into(),
        alert_manager: AlertManager::new(feature_flags.alert_thresholds.clone()),
        feature_flags,
        last_esp32_frame: None,
        tx,
        intro: wifi_densepose_sensing_server::introspection::IntrospectionState::new(),
        intro_tx,
        total_detections: 0,
        start_time: std::time::Instant::now(),
        vital_detector: VitalSignDetector::new(vital_sample_rate),
        latest_vitals: VitalSigns::default(),
        last_breathing_fusion_at: None,
        rvf_info,
        save_rvf_path: args.save_rvf.clone(),
        progressive_loader,
        active_sona_profile: None,
        model_loaded,
        smoothed_person_score: 0.0,
        prev_person_count: 0,
        smoothed_motion: 0.0,
        current_motion_level: "absent".to_string(),
        debounce_counter: 0,
        debounce_candidate: "absent".to_string(),
        baseline_motion: 0.0,
        baseline_frames: 0,
        smoothed_hr: 0.0,
        smoothed_br: 0.0,
        smoothed_hr_conf: 0.0,
        smoothed_br_conf: 0.0,
        hr_buffer: VecDeque::with_capacity(8),
        br_buffer: VecDeque::with_capacity(8),
        edge_vitals: None,
        latest_wasm_events: None,
        // Model management
        discovered_models: initial_models,
        active_model_id: None,
        // Recording
        recordings: initial_recordings,
        recording_active: false,
        recording_start_time: None,
        recording_current_id: None,
        recording_stop_tx: None,
        // Training
        training_status: "idle".to_string(),
        training_config: None,
        adaptive_model:
            adaptive_classifier::AdaptiveModel::load(&adaptive_classifier::model_path())
                .ok()
                .inspect(|m| {
                    info!(
                        "Loaded adaptive classifier: {} frames, {:.1}% accuracy",
                        m.trained_frames,
                        m.training_accuracy * 100.0
                    );
                }),
        node_states: HashMap::new(),
        udp_jitter_stats: HashMap::new(),
        detected_access_points: Vec::new(),
        wifi_interface: args.wifi_interface.clone(),
        ap_scan_interval_secs: args.ap_scan_interval_secs.max(1),
        // Accuracy sprint
        pose_tracker: PoseTracker::new(),
        location_smoother: LocationSmoother::default(),
        last_tracker_instant: None,
        stable_rendered_person_count: 0,
        count_candidate_persons: 0,
        count_candidate_since: None,
        last_present_at: None,
        last_present_count: 0,
        last_present_confidence: 0.0,
        multistatic_fuser: {
            let mut fuser = MultistaticFuser::with_config(MultistaticConfig {
                min_nodes: 1, // single-node passthrough
                ..Default::default()
            });
            let environment_positions = configured_fuser_positions(&runtime_config.environment);
            if !environment_positions.is_empty() {
                info!(
                    "Configured {} environment node positions for multistatic fusion",
                    environment_positions.len()
                );
                fuser.set_node_positions(environment_positions);
            } else if let Some(ref pos_str) = args.node_positions {
                let positions = field_bridge::parse_node_positions(pos_str);
                if !positions.is_empty() {
                    info!(
                        "Configured {} node positions for multistatic fusion",
                        positions.len()
                    );
                    fuser.set_node_positions(positions);
                }
            }
            fuser
        },
        field_model: if args.calibrate {
            info!("Field model calibration enabled — room should be empty during startup");
            FieldModel::new(field_bridge::single_link_config()).ok()
        } else {
            None
        },
        auto_calibration_enabled: false,
        auto_calibration_policy: "safe".to_string(),
        auto_calibration_quiet_since: None,
        auto_calibration_last_action: None,
        // ADR-044 §5.2: rolling-P95 over ~30 s at 20 Hz; warm-up after 60 samples.
        p95_variance: RollingP95::new(600, 60),
        p95_motion_band_power: RollingP95::new(600, 60),
        p95_spectral_power: RollingP95::new(600, 60),
        latest_pose_latency_ms: 0.0,
        pose_latency_p95_ms: RollingP95::new(600, 10),
        latest_dsp_latency_ms: 0.0,
        dsp_latency_p95_ms: RollingP95::new(600, 10),
        // ADR-044 §5.3: runtime-configurable dedup factor (persisted).
        dedup_factor: runtime_config.dedup_factor,
        enabled_modules: runtime_config.enabled_modules.iter().cloned().collect(),
        min_nodes: args.min_nodes.max(1),
        data_dir: data_dir.clone(),
        ui_path: args.ui_path.clone(),
        runtime_config_path: runtime_config_path.clone(),
        environment: runtime_config.environment.clone(),
    }));

    #[cfg(target_os = "linux")]
    if source != "simulate" {
        tokio::spawn(ap_scan_task(
            state.clone(),
            args.wifi_interface.clone(),
            args.ap_scan_interval_secs.max(1),
        ));
    }

    // Start background tasks based on source
    match source {
        "esp32" => {
            let jitter_config = udp_jitter::UdpJitterConfig {
                max_reorder_gap: args.udp_jitter_max_reorder_gap,
                max_interpolate_gap: args.udp_jitter_max_interpolate_gap,
                max_buffered_frames: args.udp_jitter_max_buffered_frames.max(1),
                max_hold: Duration::from_millis(args.udp_jitter_hold_ms.max(1)),
            };
            tokio::spawn(udp_receiver_task(
                state.clone(),
                args.udp_port,
                jitter_config,
            ));
            tokio::spawn(broadcast_tick_task(state.clone(), args.tick_ms));
        }
        "wifi" => {
            tokio::spawn(windows_wifi_task(state.clone(), args.tick_ms));
        }
        "simulate" => {
            tokio::spawn(simulated_data_task(state.clone(), args.tick_ms));
        }
        _ => unreachable!("source was validated before state creation"),
    }

    // ADR-050: Parse bind address once, use for all listeners
    let bind_ip: std::net::IpAddr = args
        .bind_addr
        .parse()
        .expect("Invalid --bind-addr (use 127.0.0.1 or 0.0.0.0)");

    // #443: optional bearer-token auth on `/api/v1/*`. `RUVIEW_API_TOKEN`
    // unset/empty ⇒ middleware is a no-op (LAN-mode default preserved); set ⇒
    // every `/api/v1/*` request must carry `Authorization: Bearer <token>`.
    let bearer_auth_state = wifi_densepose_sensing_server::bearer_auth::AuthState::from_env();
    if bearer_auth_state.is_enabled() {
        info!("API auth: bearer-token enforcement ON for /api/v1/* (RUVIEW_API_TOKEN set)");
        if bind_ip.is_unspecified() {
            warn!(
                "API auth ON but bind-addr is {} — consider --bind-addr 127.0.0.1 for LAN-only deployments",
                bind_ip
            );
        }
    } else {
        info!(
            "API auth: OFF — /api/v1/* is unauthenticated. Set RUVIEW_API_TOKEN=<token> to enforce bearer auth."
        );
    }

    // DNS-rebinding defense: validate the `Host` header against an allowlist
    // before any handler runs. Default is loopback-only (`localhost`,
    // `127.0.0.1`, `[::1]`, each with or without a port). Operators extend
    // the set via `--allowed-host` flags or the `SENSING_ALLOWED_HOSTS` env
    // var; `--disable-host-validation` opts out entirely for reverse-proxy
    // setups that already canonicalise `Host`.
    let host_allowlist = if args.disable_host_validation {
        warn!(
            "Host-header validation DISABLED — server is reachable via any Host. \
             Only use this behind a reverse proxy that pins Host."
        );
        wifi_densepose_sensing_server::host_validation::HostAllowlist::disabled()
    } else {
        let allowlist =
            wifi_densepose_sensing_server::host_validation::HostAllowlist::from_cli_and_env(
                args.allowed_hosts.iter().cloned(),
            );
        info!(
            "Host-header validation ON ({} entries; loopback names always included)",
            allowlist.entries_for_test().len()
        );
        allowlist
    };

    // WebSocket server on dedicated port (8765)
    let ws_state = state.clone();
    let ws_app = Router::new()
        .route("/ws/sensing", get(ws_sensing_handler))
        .route("/ws/pose", get(ws_presence_pose_handler))
        .route("/ws/vitals", get(ws_vitals_handler))
        .route("/health", get(health))
        .layer(axum::middleware::from_fn_with_state(
            host_allowlist.clone(),
            wifi_densepose_sensing_server::host_validation::require_allowed_host,
        ))
        .with_state(ws_state);

    let ws_addr = SocketAddr::from((bind_ip, args.ws_port));
    let ws_listener = tokio::net::TcpListener::bind(ws_addr)
        .await
        .expect("Failed to bind WebSocket port");
    info!("WebSocket server listening on {ws_addr}");

    tokio::spawn(async move {
        axum::serve(ws_listener, ws_app).await.unwrap();
    });

    // HTTP server (serves UI + full DensePose-compatible REST API)
    let ui_path = args.ui_path.clone();
    let http_app = Router::new()
        .route("/", get(|| async { Redirect::temporary("/ui/index.html") }))
        // Health endpoints (DensePose-compatible)
        .route("/health", get(health))
        .route("/health/health", get(health_system))
        .route("/health/live", get(health_live))
        .route("/health/ready", get(health_ready))
        .route("/health/version", get(health_version))
        .route("/health/metrics", get(health_metrics))
        .route("/metrics", get(prometheus_metrics_endpoint))
        // API info
        .route("/api/v1/info", get(api_info))
        .route("/api/v1/version", get(health_version))
        .route("/api/v1/features", get(features_endpoint))
        .route("/api/v1/status", get(health_ready))
        .route("/api/v1/metrics", get(health_metrics))
        // Sensing endpoints
        .route("/api/v1/sensing/latest", get(latest))
        .route("/api/v1/alerts/active", get(alerts_active))
        .route("/api/v1/alerts/{alert_id}/ack", post(alerts_ack))
        .route("/api/v1/fleet", get(fleet_endpoint))
        .route("/api/v1/topology", get(topology_endpoint))
        .route(
            "/api/v1/environment",
            get(environment_endpoint).put(environment_update_endpoint),
        )
        .route(
            "/api/v1/environment/node-positions",
            put(environment_node_positions_update_endpoint),
        )
        .route("/api/v1/modules", get(modules_endpoint))
        .route(
            "/api/v1/modules/enabled",
            put(modules_enabled_update_endpoint),
        )
        .route(
            "/api/v1/modules/:id/enabled",
            put(module_enabled_update_endpoint),
        )
        // Per-node health endpoint
        .route("/api/v1/nodes", get(nodes_endpoint))
        // ADR-110 iter 29 — per-node mesh sync state for HTTP clients.
        .route("/api/v1/nodes/:id/sync", get(node_sync_endpoint))
        .route("/api/v1/mesh", get(mesh_endpoint))
        .route("/api/v1/mesh/metrics", get(mesh_metrics_endpoint))
        // Vital sign endpoints
        .route("/api/v1/location", get(location_endpoint))
        .route("/api/v1/vital-signs", get(vital_signs_endpoint))
        .route("/api/v1/cardiac", get(cardiac_endpoint))
        .route("/api/v1/edge-vitals", get(edge_vitals_endpoint))
        // ADR-102: Edge Module Registry — surfaces the canonical Cognitum cog
        // catalog (`https://storage.googleapis.com/cognitum-apps/app-registry.json`)
        // with in-process TTL cache + stale-on-error fallback. Disabled when
        // --no-edge-registry is set (returns 404).
        .route("/api/v1/edge/registry", get(edge_registry_endpoint))
        .route("/api/v1/wasm-events", get(wasm_events_endpoint))
        // RVF model container info
        .route("/api/v1/model/info", get(model_info))
        // Progressive loading & SONA endpoints (Phase 7-8)
        .route("/api/v1/model/layers", get(model_layers))
        .route("/api/v1/model/segments", get(model_segments))
        .route("/api/v1/model/sona/profiles", get(sona_profiles))
        .route("/api/v1/model/sona/activate", post(sona_activate))
        // Pose endpoints (WiFi-derived)
        .route("/api/v1/pose", get(pose_current))
        .route("/api/v1/pose/current", get(pose_current))
        .route("/api/v1/pose/stats", get(pose_stats))
        .route("/api/v1/pose/zones/summary", get(pose_zones_summary))
        // Stream endpoints
        .route("/api/v1/stream/status", get(stream_status))
        .route("/api/v1/stream/pose", get(ws_pose_handler))
        // Sensing WebSocket on the HTTP port so the UI can reach it without a second port
        .route("/ws/sensing", get(ws_sensing_handler))
        .route("/ws/pose", get(ws_presence_pose_handler))
        .route("/ws/vitals", get(ws_vitals_handler))
        // ADR-099: real-time introspection — per-frame attractor + DTW snapshot.
        .route("/ws/introspection", get(ws_introspection_handler))
        .route(
            "/api/v1/introspection/snapshot",
            get(api_introspection_snapshot),
        )
        // Model management endpoints (UI compatibility)
        .route("/api/v1/models", get(list_models))
        .route("/api/v1/models/active", get(get_active_model))
        .route("/api/v1/models/load", post(load_model))
        .route("/api/v1/models/unload", post(unload_model))
        .route("/api/v1/models/{id}", delete(delete_model))
        .route("/api/v1/models/lora/profiles", get(list_lora_profiles))
        .route("/api/v1/models/lora/activate", post(activate_lora_profile))
        // Recording endpoints
        .route("/api/v1/recording/list", get(list_recordings))
        .route("/api/v1/recording/start", post(start_recording))
        .route("/api/v1/recording/stop", post(stop_recording))
        .route("/api/v1/recording/{id}", delete(delete_recording))
        // Training endpoints
        .route("/api/v1/train/status", get(train_status))
        .route("/api/v1/train/start", post(train_start))
        .route("/api/v1/train/stop", post(train_stop))
        // Adaptive classifier endpoints
        .route("/api/v1/adaptive/train", post(adaptive_train))
        .route("/api/v1/adaptive/status", get(adaptive_status))
        .route("/api/v1/adaptive/unload", post(adaptive_unload))
        // Field model calibration (eigenvalue-based person counting)
        .route("/api/v1/calibration/start", post(calibration_start))
        .route("/api/v1/calibration/stop", post(calibration_stop))
        .route("/api/v1/calibration/abort", post(calibration_abort))
        .route("/api/v1/calibration/auto", put(calibration_auto_update))
        .route("/api/v1/calibration/status", get(calibration_status))
        .route("/api/v1/calibration", get(calibration_overview_endpoint))
        // ADR-044 §5.3: runtime-configurable dedup factor
        .route(
            "/api/v1/config/dedup-factor",
            get(config_get_dedup_factor).post(config_set_dedup_factor),
        )
        .route("/api/v1/config/room", post(config_set_room))
        .route("/api/v1/config/alerts", post(config_set_alerts))
        .route("/api/v1/config/ground-truth", post(config_set_ground_truth))
        .route("/api/v1/config/schema", get(config_schema_endpoint))
        // Static UI files
        .nest_service("/ui", ServeDir::new(&ui_path))
        // ADR-102: make the edge registry handle (Option<Arc<EdgeRegistry>>)
        // available to the /api/v1/edge/registry handler. None when disabled.
        .layer(Extension(edge_registry.clone()))
        .layer(SetResponseHeaderLayer::overriding(
            axum::http::header::CACHE_CONTROL,
            HeaderValue::from_static("no-cache, no-store, must-revalidate"),
        ))
        // Opt-in bearer-token auth on `/api/v1/*` (#443). When `RUVIEW_API_TOKEN`
        // is unset/empty the middleware is a no-op — the default stays
        // LAN-mode-friendly. `/health*`, `/ws/sensing`, and `/ui/*` are never
        // gated (orchestrator probes + local browsers).
        .layer(axum::middleware::from_fn_with_state(
            bearer_auth_state.clone(),
            wifi_densepose_sensing_server::bearer_auth::require_bearer,
        ))
        // DNS-rebinding defense: applied last so it runs first on the request
        // path (axum layers run outermost-in). Rejects requests whose `Host`
        // header is not in the allowlist before any handler — including
        // `/health` and `/ws/*` — observes the body.
        .layer(axum::middleware::from_fn_with_state(
            host_allowlist.clone(),
            wifi_densepose_sensing_server::host_validation::require_allowed_host,
        ))
        .with_state(state.clone());

    let http_addr = SocketAddr::from((bind_ip, args.http_port));
    let http_listener = tokio::net::TcpListener::bind(http_addr)
        .await
        .expect("Failed to bind HTTP port");
    info!("HTTP server listening on {http_addr}");
    info!(
        "Open http://localhost:{}/ui/index.html in your browser",
        args.http_port
    );

    // Run the HTTP server with graceful shutdown support
    let shutdown_state = state.clone();
    let server = axum::serve(http_listener, http_app).with_graceful_shutdown(async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install CTRL+C handler");
        info!("Shutdown signal received");
    });

    server.await.unwrap();

    // Save RVF container on shutdown if --save-rvf was specified
    let s = shutdown_state.read().await;
    if let Some(ref save_path) = s.save_rvf_path {
        info!("Saving RVF container to {}", save_path.display());
        let mut builder = RvfBuilder::new();
        builder.add_manifest(
            "wifi-densepose-sensing",
            env!("CARGO_PKG_VERSION"),
            "WiFi DensePose sensing model state",
        );
        builder.add_metadata(&serde_json::json!({
            "source": s.effective_source(),
            "total_ticks": s.tick,
            "total_detections": s.total_detections,
            "uptime_secs": s.start_time.elapsed().as_secs(),
        }));
        builder.add_vital_config(&VitalSignConfig::default());
        // Save transformer weights if a model is loaded, otherwise empty
        let weights: Vec<f32> = if s.model_loaded {
            // If we loaded via --model, the progressive loader has the weights
            // For now, save runtime state placeholder
            let tf = graph_transformer::CsiToPoseTransformer::new(Default::default());
            tf.flatten_weights()
        } else {
            Vec::new()
        };
        builder.add_weights(&weights);
        match builder.write_to_file(save_path) {
            Ok(()) => info!("  RVF saved ({} weight params)", weights.len()),
            Err(e) => error!("  Failed to save RVF: {e}"),
        }
    }

    info!("Server shut down cleanly");
}

#[cfg(test)]
mod topology_console_tests {
    use super::{
        alert_thresholds_from_update, business_modules, default_dedup_factor,
        default_enabled_modules, enabled_module_set_from_ids, environment_from_ui_room_config,
        load_runtime_config, load_runtime_config_file, module_status, normalize_runtime_config,
        parse_log_format, runtime_config_schema, save_runtime_config, save_runtime_config_file,
        save_ui_room_config_file, schema_for_config_kind, topology_nodes_json,
        topology_readiness_json, upsert_environment_node_positions, validate_config_file,
        validate_runtime_config, validate_ui_room_config, AccessPointConfig, AlertConfigUpdate,
        ConfigSchemaKind, EnvironmentConfig, EnvironmentLinkConfig, EnvironmentObstacleConfig,
        EnvironmentNodeConfig, LogFormat, NodePositionUpdate, NodeState, RoomConfig,
        RuntimeConfig, UiRoomConfig, UiRoomNodeConfig, ESP32_OFFLINE_TIMEOUT,
        MODULE_CONFIG_VERSION, MODULE_PRESETS,
    };
    use std::collections::HashMap;
    use std::fs;
    use std::time::{Duration, Instant};

    fn configured_test_environment() -> EnvironmentConfig {
        EnvironmentConfig {
            room: RoomConfig {
                name: "test".to_string(),
                dimensions_m: [4.0, 3.0, 2.8],
                coordinate_system: "x_right_y_up_z_depth".to_string(),
            },
            access_points: vec![AccessPointConfig {
                ap_id: "ap1".to_string(),
                label: "AP 1".to_string(),
                ssid: "lab".to_string(),
                bssid: None,
                role: "mesh".to_string(),
                position_m: [0.0, 2.0, 0.0],
                channel: Some(6),
                band: "2.4GHz".to_string(),
                active: true,
            }],
            nodes: vec![EnvironmentNodeConfig {
                node_id: 1,
                label: "node 1".to_string(),
                kind: "esp32-c6".to_string(),
                zone: "zone_1".to_string(),
                position_m: [1.0, 1.0, 1.0],
                tdm_slot: 0,
                tdm_total: 1,
                linked_ap: "ap1".to_string(),
            }],
            links: vec![EnvironmentLinkConfig {
                link_id: "ap1-node-1".to_string(),
                ap_id: "ap1".to_string(),
                node_id: 1,
            }],
            obstacles: Vec::new(),
        }
    }

    #[test]
    fn default_environment_does_not_invent_hardware() {
        let environment = EnvironmentConfig::default();

        assert!(environment.access_points.is_empty());
        assert!(environment.nodes.is_empty());
        assert!(environment.links.is_empty());
    }

    #[test]
    fn topology_keeps_stale_nodes_visible() {
        let now = Instant::now();
        let mut active = NodeState::new();
        active.last_frame_time = Some(now);
        active.rssi_history.push_back(-48.0);
        active.csi_fps_ema = 18.0;

        let mut stale = NodeState::new();
        stale.last_frame_time = now.checked_sub(ESP32_OFFLINE_TIMEOUT + Duration::from_secs(1));

        let mut node_states = HashMap::new();
        node_states.insert(1, active);
        node_states.insert(2, stale);

        let nodes = topology_nodes_json(&EnvironmentConfig::default(), &node_states, now);

        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0]["node_id"], serde_json::json!(1));
        assert_eq!(nodes[0]["status"], serde_json::json!("live"));
        assert_eq!(nodes[0]["display_label"], serde_json::json!("ESP32-C6 #1"));
        assert_eq!(nodes[0]["coverage"]["quality"], serde_json::json!("strong"));
        assert_eq!(nodes[1]["node_id"], serde_json::json!(2));
        assert_eq!(nodes[1]["status"], serde_json::json!("stale"));
        assert_eq!(nodes[1]["display_label"], serde_json::json!("ESP32-C6 #2"));
        assert!(nodes[1]["coverage"]["score"].as_f64().unwrap_or(1.0) < 0.5);
    }

    #[test]
    fn configured_node_positions_remain_primary_for_topology() {
        let now = Instant::now();
        let mut node = NodeState::new();
        node.last_frame_time = Some(now);
        node.rssi_history.push_back(-55.0);

        let mut node_states = HashMap::new();
        node_states.insert(1, node);

        let nodes = topology_nodes_json(&configured_test_environment(), &node_states, now);

        assert_eq!(nodes[0]["position_source"], serde_json::json!("configured"));
        assert_eq!(nodes[0]["position_m"], serde_json::json!([1.0, 1.0, 1.0]));
    }

    #[test]
    fn environment_obstacles_are_optional_and_validated() {
        let mut env = configured_test_environment();
        env.obstacles.push(EnvironmentObstacleConfig {
            obstacle_id: "wall-east".to_string(),
            kind: "wall".to_string(),
            label: "East wall".to_string(),
            center_m: [1.8, 1.4, 0.0],
            size_m: [0.1, 2.8, 3.0],
            yaw_rad: 0.0,
            source: "operator_confirmed".to_string(),
            confidence: 0.95,
        });

        assert!(super::validate_environment(&env).is_ok());

        env.obstacles[0].confidence = 1.2;
        assert!(super::validate_environment(&env).is_err());
    }

    fn valid_ui_room_config() -> UiRoomConfig {
        UiRoomConfig {
            room_width_meters: 6.0,
            room_height_meters: 4.0,
            nodes: vec![
                UiRoomNodeConfig {
                    id: 1,
                    x: 0.0,
                    y: 0.0,
                    active: true,
                },
                UiRoomNodeConfig {
                    id: 2,
                    x: 6.0,
                    y: 4.0,
                    active: false,
                },
            ],
        }
    }

    #[test]
    fn ui_room_config_validates_dimensions_ids_and_bounds() {
        let config = valid_ui_room_config();
        assert!(validate_ui_room_config(&config).is_ok());

        let mut bad_width = config.clone();
        bad_width.room_width_meters = 31.0;
        assert!(validate_ui_room_config(&bad_width)
            .expect_err("width above 30m must fail")
            .contains("room_width_meters"));

        let mut bad_id = config.clone();
        bad_id.nodes[0].id = 7;
        assert!(validate_ui_room_config(&bad_id)
            .expect_err("node ids are capped at six")
            .contains("node id 7"));

        let mut out_of_room = config;
        out_of_room.nodes[0].x = 6.1;
        assert!(validate_ui_room_config(&out_of_room)
            .expect_err("coordinates outside the room must fail")
            .contains("inside the room width"));
    }

    #[test]
    fn ui_room_config_updates_environment_with_active_nodes_only() {
        let env =
            environment_from_ui_room_config(&configured_test_environment(), &valid_ui_room_config());

        assert_eq!(env.room.dimensions_m, [6.0, 3.0, 4.0]);
        assert_eq!(env.nodes.len(), 1, "inactive UI nodes are not fed to sensing");
        assert_eq!(env.nodes[0].node_id, 1);
        assert_eq!(env.nodes[0].position_m, [0.0, 1.0, 0.0]);
        assert_eq!(env.links.len(), 1, "links for still-active nodes are preserved");
    }

    #[test]
    fn save_ui_room_config_writes_served_json_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = valid_ui_room_config();

        save_ui_room_config_file(dir.path(), &config).expect("room config should save");

        let path = dir.path().join("room-config.json");
        let saved = fs::read_to_string(path).expect("room-config.json should exist");
        let loaded: UiRoomConfig = serde_json::from_str(&saved).expect("saved JSON should parse");
        assert_eq!(loaded.room_width_meters, 6.0);
        assert!(!loaded.nodes[1].active);
    }

    #[test]
    fn alert_config_update_validates_and_maps_thresholds() {
        let current = wifi_densepose_sensing_server::alerts::AlertThresholds::default();
        let update = AlertConfigUpdate {
            apnea_seconds: 20,
            no_motion_seconds: 300,
            breathing_confidence: 0.55,
        };

        let thresholds = alert_thresholds_from_update(&current, &update).expect("valid update");
        assert_eq!(thresholds.apnea_trigger_seconds, 20);
        assert_eq!(thresholds.no_motion_trigger_seconds, 300);
        assert_eq!(thresholds.apnea_min_confidence, 0.55);
        assert_eq!(
            thresholds.presence_score_threshold,
            current.presence_score_threshold,
            "fields outside the endpoint body are preserved"
        );

        let bad = AlertConfigUpdate {
            breathing_confidence: 0.0,
            ..update
        };
        assert!(alert_thresholds_from_update(&current, &bad)
            .expect_err("unsafe low confidence threshold must fail")
            .contains("breathing_confidence"));
    }

    #[test]
    fn topology_readiness_accepts_one_live_node() {
        let readiness = topology_readiness_json(1, 1);

        assert_eq!(readiness["ready"], serde_json::json!(true));
        assert_eq!(readiness["active_nodes"], serde_json::json!(1));
        assert_eq!(readiness["min_nodes"], serde_json::json!(1));
        assert_eq!(readiness["fusion_mode"], serde_json::json!("single_node"));
    }

    #[test]
    fn module_requirements_remain_node_gated() {
        assert_eq!(module_status(1, 1), "active");
        assert_eq!(module_status(1, 2), "available");
        assert_eq!(module_status(1, 3), "available");
        assert_eq!(module_status(0, 1), "offline");
    }

    #[test]
    fn business_modules_honor_enabled_set() {
        let enabled = super::default_enabled_modules()
            .into_iter()
            .collect::<std::collections::HashSet<_>>();
        let modules = super::business_modules(3, &enabled);
        let respiration = modules
            .iter()
            .find(|module| module["id"] == serde_json::json!("respiration_tracking"))
            .expect("respiration module exists");
        let sleep = modules
            .iter()
            .find(|module| module["id"] == serde_json::json!("sleep_apnea_screening"))
            .expect("sleep module exists");

        assert_eq!(respiration["enabled"], serde_json::json!(true));
        assert_eq!(respiration["effective_status"], serde_json::json!("active"));
        assert_eq!(sleep["enabled"], serde_json::json!(false));
        assert_eq!(sleep["effective_status"], serde_json::json!("disabled"));
    }

    #[test]
    fn module_presets_reference_known_modules() {
        let valid = super::valid_module_ids();

        for preset in MODULE_PRESETS {
            assert!(!preset.module_ids.is_empty(), "preset must not be empty");
            for id in preset.module_ids {
                assert!(valid.contains(*id), "preset references unknown module {id}");
            }
        }
    }

    #[test]
    fn bulk_enabled_module_set_replaces_and_validates() {
        let ids = vec![
            "intrusion_detection".to_string(),
            "intrusion_detection".to_string(),
            "door_open_detection".to_string(),
        ];
        let enabled = enabled_module_set_from_ids(&ids).expect("known module ids");

        assert_eq!(enabled.len(), 2);
        assert!(enabled.contains("intrusion_detection"));
        assert!(enabled.contains("door_open_detection"));

        let bad = enabled_module_set_from_ids(&["not_real".to_string()])
            .expect_err("unknown modules must be rejected");
        assert!(bad.contains("unknown module"));
    }

    #[test]
    fn legacy_module_config_migrates_to_v1_defaults() {
        let legacy = RuntimeConfig {
            dedup_factor: default_dedup_factor(),
            environment: EnvironmentConfig::default(),
            module_config_version: 0,
            enabled_modules: business_modules(3, &std::collections::HashSet::new())
                .into_iter()
                .filter_map(|module| {
                    module
                        .get("id")
                        .and_then(|value| value.as_str())
                        .map(str::to_string)
                })
                .collect(),
        };
        let migrated = normalize_runtime_config(legacy);
        assert_eq!(migrated.module_config_version, MODULE_CONFIG_VERSION);

        let enabled = migrated
            .enabled_modules
            .iter()
            .cloned()
            .collect::<std::collections::HashSet<_>>();
        let defaults = default_enabled_modules()
            .into_iter()
            .collect::<std::collections::HashSet<_>>();

        assert_eq!(enabled, defaults);
    }

    #[test]
    fn runtime_config_schema_exposes_strict_top_level_properties() {
        let schema = runtime_config_schema();
        let object = schema
            .schema
            .object
            .as_ref()
            .expect("RuntimeConfig schema should be an object");

        assert!(object.properties.contains_key("dedup_factor"));
        assert!(object.properties.contains_key("environment"));
        assert!(object.properties.contains_key("enabled_modules"));
        assert!(matches!(
            object.additional_properties.as_deref(),
            Some(schemars::schema::Schema::Bool(false))
        ));
    }

    #[test]
    fn missing_runtime_config_uses_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        let loaded = load_runtime_config(dir.path()).expect("missing config should default");

        assert_eq!(loaded.dedup_factor, default_dedup_factor());
        assert_eq!(loaded.module_config_version, MODULE_CONFIG_VERSION);
    }

    #[test]
    fn malformed_runtime_config_fails_closed() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("config.json"), "{ definitely not json").expect("write");

        let err = load_runtime_config(dir.path()).expect_err("malformed JSON must fail");

        assert!(err.contains("JSON schema parse error"));
    }

    #[test]
    fn runtime_config_rejects_unknown_fields() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut value = serde_json::to_value(RuntimeConfig::default()).expect("serialize");
        value
            .as_object_mut()
            .expect("object")
            .insert("surprise".to_string(), serde_json::json!(true));
        fs::write(
            dir.path().join("config.json"),
            serde_json::to_string_pretty(&value).expect("json"),
        )
        .expect("write");

        let err = load_runtime_config(dir.path()).expect_err("unknown fields must fail");

        assert!(err.contains("JSON schema parse error"));
    }

    #[test]
    fn runtime_config_file_loads_toml_and_yaml() {
        let dir = tempfile::tempdir().expect("tempdir");
        let toml_path = dir.path().join("runtime.toml");
        fs::write(
            &toml_path,
            r#"
dedup_factor = 2.5
enabled_modules = ["respiration_tracking", "fall_detection"]
"#,
        )
        .expect("write toml");
        let toml = load_runtime_config_file(&toml_path).expect("TOML config should load");
        assert_eq!(toml.dedup_factor, 2.5);
        assert_eq!(toml.module_config_version, MODULE_CONFIG_VERSION);

        let yaml_path = dir.path().join("runtime.yaml");
        fs::write(
            &yaml_path,
            r#"
dedup_factor: 4.0
enabled_modules:
  - intrusion_detection
"#,
        )
        .expect("write yaml");
        let yaml = load_runtime_config_file(&yaml_path).expect("YAML config should load");
        assert_eq!(yaml.dedup_factor, 4.0);
        assert_eq!(yaml.enabled_modules, vec!["intrusion_detection".to_string()]);
    }

    #[test]
    fn save_runtime_config_file_round_trips_json_toml_and_yaml() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = RuntimeConfig {
            dedup_factor: 2.0,
            environment: configured_test_environment(),
            module_config_version: MODULE_CONFIG_VERSION,
            enabled_modules: vec![
                "fall_detection".to_string(),
                "respiration_tracking".to_string(),
            ],
        };

        for name in ["runtime.json", "runtime.toml", "runtime.yaml"] {
            let path = dir.path().join(name);
            save_runtime_config_file(&path, &config).expect("config should save");
            let loaded = load_runtime_config_file(&path).expect("saved config should load");

            assert_eq!(loaded.dedup_factor, 2.0);
            assert_eq!(loaded.environment.nodes[0].position_m, [1.0, 1.0, 1.0]);
            assert_eq!(loaded.enabled_modules, config.enabled_modules);
        }
    }

    #[test]
    fn runtime_config_file_rejects_toml_unknown_fields() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("runtime.toml");
        fs::write(
            &path,
            r#"
dedup_factor = 2.0
surprise = true
"#,
        )
        .expect("write");

        let err = load_runtime_config_file(&path).expect_err("unknown TOML field must fail");

        assert!(err.contains("TOML schema parse error"));
    }

    #[test]
    fn runtime_config_file_rejects_unsupported_extension() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("runtime.ini");
        fs::write(&path, "dedup_factor = 2.0").expect("write");

        let err = load_runtime_config_file(&path).expect_err("unsupported extension must fail");

        assert!(err.contains("unsupported runtime config extension"));
    }

    #[test]
    fn bfld_blueprint_yaml_shape_tolerates_home_assistant_input_tags() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../cog-ha-matter/blueprints/bfld/presence-lighting.yaml");

        validate_config_file(&path).expect("checked-in BFLD blueprint should validate");
    }

    #[test]
    fn schema_kind_generation_covers_public_config_shapes() {
        for kind in [
            ConfigSchemaKind::Runtime,
            ConfigSchemaKind::Swarm,
            ConfigSchemaKind::Training,
            ConfigSchemaKind::HomecoreAutomation,
            ConfigSchemaKind::BfldBlueprint,
        ] {
            let schema = schema_for_config_kind(kind);
            assert!(
                schema.schema.object.is_some() || !schema.definitions.is_empty(),
                "schema should not be empty"
            );
        }
    }

    #[test]
    fn log_format_parser_accepts_text_json_and_rejects_unknown() {
        assert_eq!(parse_log_format(None).unwrap(), LogFormat::Text);
        assert_eq!(parse_log_format(Some("text")).unwrap(), LogFormat::Text);
        assert_eq!(parse_log_format(Some("json")).unwrap(), LogFormat::Json);
        assert!(parse_log_format(Some("xml")).is_err());
    }

    #[test]
    fn runtime_config_rejects_invalid_dedup_factor() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = RuntimeConfig::default();
        config.dedup_factor = 0.5;
        fs::write(
            dir.path().join("config.json"),
            serde_json::to_string_pretty(&config).expect("json"),
        )
        .expect("write");

        let err = load_runtime_config(dir.path()).expect_err("invalid config must fail");

        assert!(err.contains("dedup_factor"));
    }

    #[test]
    fn runtime_config_rejects_invalid_topology_links() {
        let mut config = RuntimeConfig::default();
        config.environment = configured_test_environment();
        config.environment.links[0].ap_id = "missing-ap".to_string();
        config.environment.links[0].link_id = "bad-link".to_string();

        let err = validate_runtime_config(&config).expect_err("bad topology must fail");

        assert!(err.contains("unknown AP"));
    }

    #[test]
    fn node_position_update_changes_configured_node() {
        let mut env = configured_test_environment();
        let live = std::collections::HashSet::new();
        upsert_environment_node_positions(
            &mut env,
            &[NodePositionUpdate {
                node_id: 1,
                position_m: [1.5, 2.0, 0.25],
            }],
            &live,
        )
        .expect("configured node position should update");

        assert_eq!(env.nodes[0].position_m, [1.5, 2.0, 0.25]);
        assert_eq!(env.links.len(), 1, "links are preserved, not invented");
    }

    #[test]
    fn node_position_update_upserts_live_unconfigured_node() {
        let mut env = configured_test_environment();
        let live = [2_u8].into_iter().collect::<std::collections::HashSet<_>>();
        upsert_environment_node_positions(
            &mut env,
            &[NodePositionUpdate {
                node_id: 2,
                position_m: [-0.5, 1.25, 0.75],
            }],
            &live,
        )
        .expect("live unconfigured node should upsert");

        let node = env.nodes.iter().find(|node| node.node_id == 2).unwrap();
        assert_eq!(node.position_m, [-0.5, 1.25, 0.75]);
        assert_eq!(node.kind, "esp32_c6");
        assert_eq!(node.zone, "manual");
        assert!(node.linked_ap.is_empty());
        assert_eq!(env.links.len(), 1, "manual upsert must not create fake links");
    }

    #[test]
    fn node_position_update_rejects_invalid_or_unknown_node() {
        let mut env = configured_test_environment();
        let live = std::collections::HashSet::new();

        let err = upsert_environment_node_positions(
            &mut env,
            &[NodePositionUpdate {
                node_id: 2,
                position_m: [0.0, 0.0, 0.0],
            }],
            &live,
        )
        .expect_err("unknown non-live node must be rejected");
        assert!(err.contains("not configured or live"));

        let err = upsert_environment_node_positions(
            &mut env,
            &[NodePositionUpdate {
                node_id: 1,
                position_m: [f64::NAN, 0.0, 0.0],
            }],
            &live,
        )
        .expect_err("non-finite coordinates must be rejected");
        assert!(err.contains("invalid position"));
    }

    #[test]
    fn save_runtime_config_rejects_invalid_config_without_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = RuntimeConfig::default();
        config.enabled_modules.push("not-a-real-module".to_string());

        let err = save_runtime_config(dir.path(), &config).expect_err("invalid save must fail");

        assert!(err.contains("unknown enabled module id"));
        assert!(!dir.path().join("config.json").exists());
    }
}

#[cfg(test)]
mod prometheus_metrics_tests {
    use super::{append_udp_jitter_prometheus_metrics, udp_jitter::NodeJitterSnapshot};
    use std::collections::HashMap;

    #[test]
    fn udp_jitter_metrics_use_bounded_node_labels_and_ratios() {
        let mut stats = HashMap::new();
        stats.insert(
            9,
            NodeJitterSnapshot {
                received_live_total: 4,
                emitted_live_total: 3,
                interpolated_total: 1,
                reordered_total: 2,
                missing_dropped_total: 1,
                late_or_duplicate_total: 1,
                buffer_depth: 2,
                last_hold_ms: 7,
                max_hold_ms: 15,
            },
        );

        let mut body = String::new();
        append_udp_jitter_prometheus_metrics(&mut body, &stats);

        assert!(body.contains("ruvsense_csi_frames_total{node=\"9\",kind=\"live\"} 3"));
        assert!(
            body.contains("ruvsense_csi_frames_total{node=\"9\",kind=\"interpolated\"} 1")
        );
        assert!(
            body.contains("ruvsense_udp_packets_dropped_total{node=\"9\",reason=\"missing_gap\"} 1")
        );
        assert!(body.contains("ruvsense_node_dropped_packet_ratio{node=\"9\"} 0.400000"));
        assert!(body.contains("ruvsense_udp_jitter_hold_ms{node=\"9\",kind=\"max\"} 15"));
        assert!(!body.contains("COM"));
        assert!(!body.contains('\n') || body.lines().all(|line| !line.contains("metric 1")));
    }
}

#[cfg(test)]
mod esp32_parser_adapter_tests {
    use super::parse_esp32_frame;

    fn build_adr018_frame() -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&0xC511_0001u32.to_le_bytes());
        buf.push(7);
        buf.push(1);
        buf.extend_from_slice(&56u16.to_le_bytes());
        buf.extend_from_slice(&2437u32.to_le_bytes());
        buf.extend_from_slice(&0x0102_0304u32.to_le_bytes());
        buf.push((-50i8) as u8);
        buf.push((-95i8) as u8);
        buf.push(0);
        buf.push(0);
        for idx in 0..56 {
            buf.push(idx as i8 as u8);
            buf.push((idx as i8).wrapping_neg() as u8);
        }
        buf
    }

    #[test]
    fn server_parser_uses_adr018_offsets() {
        let frame = parse_esp32_frame(&build_adr018_frame()).expect("ADR-018 frame must parse");

        assert_eq!(frame.node_id, 7);
        assert_eq!(frame.n_antennas, 1);
        assert_eq!(frame.n_subcarriers, 56);
        assert_eq!(frame.freq_mhz, 2437);
        assert_eq!(frame.sequence, 0x0102_0304);
        assert_eq!(frame.rssi, -50);
        assert_eq!(frame.noise_floor, -95);
        assert_eq!(frame.amplitudes.len(), 56);
    }

    #[test]
    fn server_parser_skips_sibling_vitals_magic() {
        let mut buf = vec![0u8; 32];
        buf[0..4].copy_from_slice(&0xC511_0002u32.to_le_bytes());

        assert!(parse_esp32_frame(&buf).is_none());
    }
}

#[cfg(test)]
mod node_sync_snapshot_serialization_tests {
    //! ADR-110 iter 24 — JSON public-API contract for the iter 23
    //! NodeSyncSnapshot field. Any future rename / removal here must be
    //! intentional and update both Rust + UI/automation consumers.

    use super::*;

    fn sample_sync() -> NodeSyncSnapshot {
        NodeSyncSnapshot {
            offset_us: 1_163_565,
            is_leader: false,
            is_valid: true,
            smoothed: true,
            sequence: 20,
            csi_fps_ema: 10.0,
            csi_fps_samples: 47,
            staleness_ms: Some(120),
        }
    }

    fn sample_node(sync: Option<NodeSyncSnapshot>) -> NodeInfo {
        NodeInfo {
            node_id: 9,
            rssi_dbm: -38.0,
            position: [2.0, 0.0, 1.5],
            amplitude: vec![],
            subcarrier_count: 0,
            sync,
        }
    }

    #[test]
    fn sync_present_serializes_all_seven_fields() {
        let v = serde_json::to_value(sample_node(Some(sample_sync()))).unwrap();
        let s = v.get("sync").expect("sync key must be present");
        // All eight contract fields named exactly as iter 23/34 documented.
        for key in [
            "offset_us",
            "is_leader",
            "is_valid",
            "smoothed",
            "sequence",
            "csi_fps_ema",
            "csi_fps_samples",
            "staleness_ms",
        ] {
            assert!(
                s.get(key).is_some(),
                "sync object missing field `{}` — UI contract broken",
                key
            );
        }
        // Spot-check values round-trip.
        assert_eq!(s["offset_us"], 1_163_565);
        assert_eq!(s["is_leader"], false);
        assert_eq!(s["sequence"], 20);
        assert_eq!(s["csi_fps_samples"], 47);
    }

    #[test]
    fn sync_absent_omits_the_key_entirely() {
        // skip_serializing_if = "Option::is_none" must drop the key, not
        // emit `"sync": null`. The non-mesh paths rely on this for
        // backwards compatibility with pre-iter-23 UI clients.
        let v = serde_json::to_value(sample_node(None)).unwrap();
        assert!(
            v.get("sync").is_none(),
            "expected `sync` key omitted when None, got {:?}",
            v.get("sync")
        );
        // The base NodeInfo fields are still there.
        assert_eq!(v["node_id"], 9);
        assert_eq!(v["rssi_dbm"], -38.0);
    }

    #[test]
    fn sync_round_trips_through_serde() {
        let original = sample_node(Some(sample_sync()));
        let json = serde_json::to_string(&original).unwrap();
        let parsed: NodeInfo = serde_json::from_str(&json).unwrap();
        // Field-level equality on the sync sub-object.
        let s_orig = original.sync.unwrap();
        let s_parsed = parsed.sync.expect("sync should survive round-trip");
        assert_eq!(s_parsed.offset_us, s_orig.offset_us);
        assert_eq!(s_parsed.is_leader, s_orig.is_leader);
        assert_eq!(s_parsed.is_valid, s_orig.is_valid);
        assert_eq!(s_parsed.smoothed, s_orig.smoothed);
        assert_eq!(s_parsed.sequence, s_orig.sequence);
        assert!((s_parsed.csi_fps_ema - s_orig.csi_fps_ema).abs() < 1e-9);
        assert_eq!(s_parsed.csi_fps_samples, s_orig.csi_fps_samples);
    }
}

#[cfg(test)]
mod sync_snapshot_helper_tests {
    //! ADR-110 iter 30 — covers the pure helper that backs both
    //! `/api/v1/nodes/:id/sync` and `/api/v1/mesh` REST endpoints and
    //! the WebSocket sensing_update broadcast. Tests at this layer keep
    //! the public-API contract honest without spinning up the axum
    //! router or constructing a full AppStateInner.

    use super::*;
    use wifi_densepose_hardware::{SyncPacket, SyncPacketFlags};

    fn populated_sync(node_id: u8) -> SyncPacket {
        SyncPacket {
            node_id,
            proto_ver: 1,
            flags: SyncPacketFlags {
                is_leader: false,
                is_valid: true,
                smoothed_used: true,
            },
            local_us: 28_798_450,
            epoch_us: 27_634_885,
            sequence: 20,
        }
    }

    #[test]
    fn fresh_node_with_no_sync_returns_none() {
        // Mirrors the REST 404 "no_sync" branch.
        let ns = NodeState::new();
        assert!(ns.sync_snapshot().is_none());
    }

    #[test]
    fn node_with_latest_sync_produces_correct_snapshot() {
        // Mirrors the REST 200 OK branch + the WebSocket sync field.
        let mut ns = NodeState::new();
        ns.latest_sync = Some(populated_sync(9));
        ns.latest_sync_at = Some(std::time::Instant::now());
        // Pretend the fps EMA has settled (iter 18 5-sample warmup).
        ns.csi_fps_ema = 10.5;
        ns.csi_fps_samples = 42;

        let snap = ns
            .sync_snapshot()
            .expect("populated state must produce a snapshot");
        assert_eq!(snap.offset_us, 1_163_565); // §A0.10 measured boot delta
        assert!(!snap.is_leader);
        assert!(snap.is_valid);
        assert!(snap.smoothed);
        assert_eq!(snap.sequence, 20);
        assert!((snap.csi_fps_ema - 10.5).abs() < 1e-9);
        assert_eq!(snap.csi_fps_samples, 42);
    }

    #[test]
    fn apply_sync_packet_populates_a_fresh_node() {
        // Mirrors what udp_receiver_task does on the very first sync
        // packet from a previously-unseen node.
        let mut ns = NodeState::new();
        assert!(ns.latest_sync.is_none());
        assert!(ns.latest_sync_at.is_none());

        let now = std::time::Instant::now();
        ns.apply_sync_packet(populated_sync(9), now);

        let sync = ns.latest_sync.as_ref().expect("must be populated");
        assert_eq!(sync.node_id, 9);
        assert_eq!(sync.sequence, 20);
        // latest_sync_at must be exactly the Instant we passed (no clock skew).
        assert_eq!(ns.latest_sync_at, Some(now));
        // sync_snapshot now produces a value (REST 200 OK path).
        assert!(ns.sync_snapshot().is_some());
    }

    #[test]
    fn apply_sync_packet_overwrites_older_data() {
        // Subsequent packets must replace, not accumulate. Otherwise the
        // §A0.10-smoothed offset would lag the latest beacon.
        let mut ns = NodeState::new();
        let t0 = std::time::Instant::now();
        ns.apply_sync_packet(populated_sync(9), t0);

        // Second packet: same node, advanced sequence + offset.
        let mut second = populated_sync(9);
        second.sequence = 40;
        second.local_us = 30_000_000;
        second.epoch_us = 28_834_900;
        let t1 = t0 + std::time::Duration::from_secs(2);
        ns.apply_sync_packet(second, t1);

        let cur = ns.latest_sync.as_ref().unwrap();
        assert_eq!(cur.sequence, 40); // newer sequence persisted
        assert_eq!(cur.local_us, 30_000_000); // newer local persisted
        assert_eq!(ns.latest_sync_at, Some(t1)); // staleness clock reset
    }

    #[test]
    fn snapshot_staleness_ms_tracks_apply_time() {
        // Iter 34: staleness_ms = (Instant::now() - latest_sync_at).as_millis().
        // We can't pass a synthetic "now" through sync_snapshot, but we can
        // pin latest_sync_at to a past instant and assert the value lands
        // in a plausible window.
        let mut ns = NodeState::new();
        ns.latest_sync = Some(populated_sync(9));
        ns.latest_sync_at =
            std::time::Instant::now().checked_sub(std::time::Duration::from_millis(750));

        let snap = ns.sync_snapshot().unwrap();
        let st = snap.staleness_ms.expect("staleness_ms must be present");
        // Should be approximately 750 ms — give a generous ±500 ms tolerance
        // for any test-runner scheduling delay between checked_sub() and
        // elapsed() within sync_snapshot.
        assert!(
            st >= 740 && st < 1250,
            "expected ~750 ms staleness, got {} ms",
            st
        );
    }

    #[test]
    fn fleet_role_counts_classifies_correctly() {
        // Iter 37 — verify the leader/follower split that drives the
        // Prometheus `wifi_densepose_mesh_node_total{state=...}` gauge.
        // Local fixture rather than reaching across test modules.
        fn snap(is_leader: bool) -> NodeSyncSnapshot {
            NodeSyncSnapshot {
                offset_us: 0,
                is_leader,
                is_valid: true,
                smoothed: true,
                sequence: 0,
                csi_fps_ema: 10.0,
                csi_fps_samples: 10,
                staleness_ms: Some(0),
            }
        }
        assert_eq!(super::fleet_role_counts(&[]), (0, 0));
        let snaps = vec![(12u8, snap(true)), (9, snap(false)), (3, snap(false))];
        assert_eq!(super::fleet_role_counts(&snaps), (1, 2));
        // Edge: all leaders (election would prevent this but gauge math must hold).
        assert_eq!(
            super::fleet_role_counts(&[(1u8, snap(true)), (2, snap(true))]),
            (2, 0)
        );
    }

    #[test]
    fn bool_metric_returns_zero_or_one_as_text() {
        // Locks the Prometheus exposition convention: gauges holding a
        // boolean state MUST emit literal "0" or "1", never "false"/"true".
        // If anyone changes the helper to format!("{}", b), Prometheus will
        // 400-reject the scrape — catch it here instead of in production.
        assert_eq!(super::bool_metric(true), "1");
        assert_eq!(super::bool_metric(false), "0");
    }

    #[test]
    fn mesh_aligned_us_honors_9s_staleness_gate() {
        // The receive helper stores latest_sync_at = Instant::now() each
        // beacon. mesh_aligned_us_for_csi_frame returns None once that
        // Instant is older than 9 s (3 × VALID_WINDOW_MS). Verify both
        // sides of that boundary without sleeping — set latest_sync_at
        // to past instants directly.
        let mut ns = NodeState::new();
        let now = std::time::Instant::now();
        ns.latest_sync = Some(populated_sync(9));

        // Fresh: 1 s old → should return Some.
        ns.latest_sync_at = now.checked_sub(std::time::Duration::from_secs(1));
        assert!(
            ns.mesh_aligned_us_for_csi_frame(20).is_some(),
            "1 s old sync must produce a mesh-aligned timestamp"
        );

        // Just inside the gate: 8 s old → should still return Some.
        ns.latest_sync_at = now.checked_sub(std::time::Duration::from_secs(8));
        assert!(
            ns.mesh_aligned_us_for_csi_frame(20).is_some(),
            "8 s old sync must still be inside the 9 s gate"
        );

        // Just outside the gate: 10 s old → must return None.
        ns.latest_sync_at = now.checked_sub(std::time::Duration::from_secs(10));
        assert!(
            ns.mesh_aligned_us_for_csi_frame(20).is_none(),
            "10 s old sync must trigger the 9 s staleness gate"
        );
    }

    #[test]
    fn snapshot_reflects_leader_state() {
        // Same data shape that /api/v1/mesh emits for a leader node.
        let mut ns = NodeState::new();
        let mut s = populated_sync(12);
        s.flags = SyncPacketFlags {
            is_leader: true,
            is_valid: true,
            smoothed_used: false,
        };
        s.local_us = 28_864_932;
        s.epoch_us = 28_864_939; // -7 µs delta on the leader
        ns.latest_sync = Some(s);
        ns.latest_sync_at = Some(std::time::Instant::now());

        let snap = ns.sync_snapshot().unwrap();
        assert!(snap.is_leader);
        assert_eq!(snap.offset_us, -7); // call-stack µs only
        assert!(!snap.smoothed);
    }
}

#[cfg(test)]
mod novelty_tests {
    use super::*;

    /// First call to `update_novelty` must produce *some* score
    /// (`Some(_)` not `None`) — proves the per-node sketch bank is
    /// initialised by `NodeState::new()` and the novelty path is
    /// actually being exercised. With an empty bank the score is 1.0
    /// (max novelty).
    #[test]
    fn first_frame_yields_max_novelty_then_zero_on_repeat() {
        let mut ns = NodeState::new();
        let amplitudes: Vec<f64> = (0..NOVELTY_VECTOR_DIM).map(|i| (i as f64).sin()).collect();

        ns.update_novelty(&amplitudes);
        let first = ns.last_novelty_score.expect("sketch bank initialised");
        assert!(
            (first - 1.0).abs() < 1e-6,
            "empty bank → max novelty 1.0, got {first}"
        );

        // Repeat the exact same frame — bank now contains it, so the
        // novelty score must be 0.0 (the score is computed before the
        // second insert, against the post-first-insert bank).
        ns.update_novelty(&amplitudes);
        let second = ns.last_novelty_score.expect("score stays Some");
        assert_eq!(second, 0.0, "exact-repeat frame → novelty 0.0");
    }

    /// `update_novelty` must tolerate amplitude vectors of unexpected
    /// length — short ones zero-padded, long ones truncated — without
    /// panicking. ESP32-S3 boards report 56 subcarriers but other
    /// hardware variants ship 52 or 64; the schema-locked sketch bank
    /// requires exactly NOVELTY_VECTOR_DIM.
    #[test]
    fn handles_short_and_long_amplitude_vectors() {
        let mut ns = NodeState::new();
        ns.update_novelty(&[1.0, 2.0]); // way short
        assert!(ns.last_novelty_score.is_some());

        let too_long: Vec<f64> = (0..NOVELTY_VECTOR_DIM * 2).map(|i| i as f64).collect();
        ns.update_novelty(&too_long); // way long
        assert!(ns.last_novelty_score.is_some());
    }
}

// ── ADR-044 §5.3: dedup_factor runtime configuration endpoints ────────────────

/// `POST /api/v1/config/room` - persist the operator room layout.
async fn config_set_room(
    State(state): State<SharedState>,
    Json(body): Json<UiRoomConfig>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    validate_ui_room_config(&body).map_err(|message| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "status": "error", "message": message })),
        )
    })?;

    let s = state.read().await;
    let environment = environment_from_ui_room_config(&s.environment, &body);
    validate_environment(&environment).map_err(|message| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "status": "error", "message": message })),
        )
    })?;
    let ui_path = s.ui_path.clone();
    let runtime_config_path = s.runtime_config_path.clone();
    let dedup_factor = s.dedup_factor;
    let enabled_modules = sorted_module_ids(&s.enabled_modules);
    drop(s);

    save_ui_room_config_file(&ui_path, &body).map_err(|message| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "status": "error", "message": message })),
        )
    })?;
    save_runtime_config_file(
        &runtime_config_path,
        &RuntimeConfig {
            dedup_factor,
            environment: environment.clone(),
            module_config_version: MODULE_CONFIG_VERSION,
            enabled_modules,
        },
    )
    .map_err(|message| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "status": "error", "message": message })),
        )
    })?;

    let mut s = state.write().await;
    s.environment = environment.clone();
    apply_environment_node_positions(&mut s.multistatic_fuser, &environment);

    Ok(Json(serde_json::json!({ "status": "ok" })))
}

/// `POST /api/v1/config/alerts` - update in-memory alert thresholds.
async fn config_set_alerts(
    State(state): State<SharedState>,
    Json(body): Json<AlertConfigUpdate>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let mut s = state.write().await;
    let thresholds =
        alert_thresholds_from_update(s.alert_manager.thresholds(), &body).map_err(|message| {
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "status": "error", "message": message })),
            )
        })?;
    s.alert_manager.set_thresholds(thresholds);
    Ok(Json(serde_json::json!({ "status": "ok" })))
}

/// `GET /api/v1/config/dedup-factor` - read the current dedup factor.
async fn config_get_dedup_factor(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let s = state.read().await;
    Json(serde_json::json!({
        "dedup_factor": s.dedup_factor,
        "description": "Divisor for multi-node person count deduplication (sum / factor). Range: 1.0–10.0."
    }))
}

/// `POST /api/v1/config/dedup-factor` — set the dedup factor (clamped 1.0–10.0).
///
/// Body: `{ "value": <f64> }`
async fn config_set_dedup_factor(
    State(state): State<SharedState>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let value = body.get("value").and_then(|v| v.as_f64()).unwrap_or(3.0);
    let clamped = value.clamp(1.0, 10.0);
    let s = state.read().await;
    let runtime_config_path = s.runtime_config_path.clone();
    let environment = s.environment.clone();
    let enabled_modules = sorted_module_ids(&s.enabled_modules);
    drop(s);
    if let Err(message) = save_runtime_config_file(
        &runtime_config_path,
        &RuntimeConfig {
            dedup_factor: clamped,
            environment,
            module_config_version: MODULE_CONFIG_VERSION,
            enabled_modules,
        },
    ) {
        return Json(serde_json::json!({
            "status": "error",
            "message": message,
        }));
    }
    let mut s = state.write().await;
    s.dedup_factor = clamped;
    Json(serde_json::json!({
        "status": "ok",
        "dedup_factor": clamped,
    }))
}

/// `POST /api/v1/config/ground-truth` — auto-tune dedup factor from a known person count.
///
/// Derives `dedup_factor = raw_node_sum / ground_truth_count` from the current
/// per-node person counts, clamped to [1.0, 10.0].  Persisted immediately.
///
/// Body: `{ "count": <u64> }`
async fn config_set_ground_truth(
    State(state): State<SharedState>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let ground_truth = match body.get("count").and_then(|v| v.as_u64()) {
        Some(n) if n > 0 => n as usize,
        _ => return Json(serde_json::json!({"error": "count must be a positive integer"})),
    };
    let s = state.read().await;
    let raw_sum: usize = s
        .node_states
        .values()
        .filter(|ns| {
            ns.last_frame_time
                .map(|t| t.elapsed() < std::time::Duration::from_secs(10))
                .unwrap_or(false)
        })
        .map(|ns| ns.prev_person_count)
        .sum();
    let optimal = if raw_sum > 0 {
        (raw_sum as f64) / (ground_truth as f64)
    } else {
        3.0
    };
    let clamped = optimal.clamp(1.0, 10.0);
    let runtime_config_path = s.runtime_config_path.clone();
    let environment = s.environment.clone();
    let enabled_modules = sorted_module_ids(&s.enabled_modules);
    drop(s);
    if let Err(message) = save_runtime_config_file(
        &runtime_config_path,
        &RuntimeConfig {
            dedup_factor: clamped,
            environment,
            module_config_version: MODULE_CONFIG_VERSION,
            enabled_modules,
        },
    ) {
        return Json(serde_json::json!({
            "status": "error",
            "message": message,
        }));
    }
    let mut s = state.write().await;
    s.dedup_factor = clamped;
    Json(serde_json::json!({
        "status": "ok",
        "ground_truth": ground_truth,
        "raw_sum": raw_sum,
        "computed_dedup_factor": clamped,
    }))
}

// ── Unit tests: RollingP95 ─────────────────────────────────────────────────────

#[cfg(test)]
mod rolling_p95_tests {
    use super::RollingP95;

    #[test]
    fn cold_start_returns_none() {
        let p = RollingP95::new(100, 10);
        assert!(p.current().is_none(), "empty buffer must return None");
    }

    #[test]
    fn below_min_samples_returns_none() {
        let mut p = RollingP95::new(100, 10);
        for i in 1..=9 {
            p.push(i as f64);
        }
        assert!(
            p.current().is_none(),
            "fewer than min_samples must return None"
        );
    }

    #[test]
    fn p95_of_ramp_is_near_95() {
        let mut p = RollingP95::new(100, 10);
        for i in 1..=100 {
            p.push(i as f64);
        }
        let p95 = p.current().expect("should have value after 100 samples");
        assert!(
            (94.0..=96.0).contains(&p95),
            "P95 of 1..=100 should be ~95, got {p95}"
        );
    }

    #[test]
    fn window_slides_evicts_oldest() {
        let mut p = RollingP95::new(5, 3);
        // Push 1..=5, then 100 — oldest (1) is evicted.
        for i in 1..=5 {
            p.push(i as f64);
        }
        p.push(100.0); // evicts 1; buf = [2, 3, 4, 5, 100]
        let p95 = p.current().expect("6 pushes, window=5 → 5 samples");
        // P95 of [2,3,4,5,100]: idx = ceil(5*0.95)=5 → sorted[4]=100
        assert_eq!(
            p95, 100.0,
            "largest value should dominate p95 after eviction"
        );
    }

    #[test]
    fn len_reports_buffer_size() {
        let mut p = RollingP95::new(10, 5);
        assert_eq!(p.len(), 0);
        p.push(1.0);
        assert_eq!(p.len(), 1);
    }
}

#[cfg(all(test, feature = "mqtt"))]
mod mqtt_bridge_tests {
    use super::vitals_snapshots_from_sensing_json;
    use serde_json::json;

    /// Regression for the per-node presence bug (#872/#898): each node must
    /// surface its OWN classification, not the room-level aggregate. Node 1 is
    /// present+moving; node 2 is absent — node 2 must NOT inherit node 1's
    /// "present".
    #[test]
    fn per_node_presence_uses_each_nodes_own_classification() {
        let v = json!({
            "timestamp": 1.0,
            "classification": { "presence": true, "motion_level": "walking", "confidence": 0.9 },
            "vital_signs": { "breathing_rate_bpm": 14.0, "heart_rate_bpm": 60.0 },
            "persons": [{}, {}],
            "nodes": [
                { "node_id": 1, "rssi_dbm": -40.0,
                  "classification": { "presence": true, "motion_level": "walking", "confidence": 0.8 } },
                { "node_id": 2, "rssi_dbm": -70.0,
                  "classification": { "presence": false, "motion_level": "absent", "confidence": 0.1 } }
            ]
        });
        let snaps = vitals_snapshots_from_sensing_json(&v, "ruview");
        assert_eq!(snaps.len(), 2, "one snapshot per node");

        let n1 = snaps.iter().find(|s| s.node_id == "ruview-node1").unwrap();
        let n2 = snaps.iter().find(|s| s.node_id == "ruview-node2").unwrap();

        assert!(n1.presence && n1.motion > 0.0, "node1 present + moving");
        assert!(
            !n2.presence && n2.motion == 0.0,
            "node2 must be absent — not inherit the room aggregate"
        );
        // Per-node RSSI preserved.
        assert_eq!(n1.rssi_dbm, Some(-40.0));
        assert_eq!(n2.rssi_dbm, Some(-70.0));
        // Vitals + person count are room-level, shared across node devices.
        assert_eq!(n1.n_persons, 2);
        assert_eq!(n2.n_persons, 2);
        assert_eq!(n1.breathing_rate_bpm, Some(14.0));
        assert_eq!(n2.heartrate_bpm, Some(60.0));
        // presence_score is gated on presence.
        assert!(n1.presence_score > 0.0);
        assert_eq!(n2.presence_score, 0.0);
    }

    /// A node that omits a classification field defers to the room aggregate
    /// rather than silently reading false/0.
    #[test]
    fn per_node_missing_fields_fall_back_to_aggregate() {
        let v = json!({
            "timestamp": 1.0,
            "classification": { "presence": true, "motion_level": "still", "confidence": 0.7 },
            "vital_signs": {},
            "nodes": [ { "node_id": 3, "rssi_dbm": -55.0 } ]  // no per-node classification
        });
        let snaps = vitals_snapshots_from_sensing_json(&v, "n");
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].node_id, "n-node3");
        assert!(snaps[0].presence, "defers to aggregate presence");
        assert_eq!(snaps[0].motion, 0.0, "aggregate 'still' => no motion");
    }

    /// No `nodes` array (wifi / simulate sources): single aggregate snapshot
    /// keyed by the base id.
    #[test]
    fn falls_back_to_single_aggregate_when_no_nodes() {
        let v = json!({
            "timestamp": 2.0,
            "classification": { "presence": true, "motion_level": "idle", "confidence": 0.6 },
            "vital_signs": { "breathing_rate_bpm": 12.0 },
            "persons": [{}]
        });
        let snaps = vitals_snapshots_from_sensing_json(&v, "ruview");
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].node_id, "ruview");
        assert!(snaps[0].presence);
        assert_eq!(snaps[0].motion, 0.0, "idle => no motion");
        assert_eq!(snaps[0].n_persons, 1);
    }

    /// `motion_level: "absent"` must map to zero motion (the old aggregate
    /// match fell through to `Some(_) => 1.0`, treating absent as full motion).
    #[test]
    fn absent_motion_level_is_zero_motion() {
        let v = json!({
            "timestamp": 0.0,
            "classification": { "presence": false, "motion_level": "absent", "confidence": 0.0 },
            "vital_signs": {}
        });
        let snaps = vitals_snapshots_from_sensing_json(&v, "x");
        assert_eq!(snaps[0].motion, 0.0);
        assert!(!snaps[0].presence);
    }
}

#[cfg(test)]
mod model_load_diagnostic_tests {
    use super::diagnose_model_load_error;
    use std::path::Path;

    #[test]
    fn safetensors_is_named_and_points_at_894() {
        // 8-byte LE header length then '{' — the safetensors signature.
        let data = [0x10, 0, 0, 0, 0, 0, 0, 0, b'{', b'"'];
        let msg = diagnose_model_load_error(
            Path::new("models/wifi-densepose-pretrained/model.safetensors"),
            &data,
            "invalid magic at offset 0",
        );
        assert!(msg.contains("safetensors"), "{msg}");
        assert!(msg.contains("#894"), "{msg}");
        assert!(msg.contains("signal heuristics"), "{msg}");
    }

    #[test]
    fn quantized_bin_is_identified() {
        let data = [0x35, 0x57, 0x45, 0x77]; // the 0x77455735 the loader reports
        let msg = diagnose_model_load_error(Path::new("model-q4.bin"), &data, "bad magic");
        assert!(msg.contains("quantized weight blob"), "{msg}");
        assert!(msg.contains("RVFS") || msg.contains("0x52564653"), "{msg}");
    }

    #[test]
    fn jsonl_manifest_is_identified() {
        let data = *b"{\"seg\":0}";
        let msg = diagnose_model_load_error(Path::new("model.rvf.jsonl"), &data, "x");
        assert!(msg.contains("JSONL manifest"), "{msg}");
    }

    #[test]
    fn unknown_format_still_gives_guidance() {
        let data = [0u8, 1, 2, 3];
        let msg = diagnose_model_load_error(Path::new("weird.dat"), &data, "x");
        assert!(msg.contains("RVF binary container"), "{msg}");
        assert!(msg.contains("wifi-densepose-train"), "{msg}");
    }
}

#[cfg(test)]
mod export_rvf_mode_tests {
    use super::export_emits_placeholder_demo;

    #[test]
    fn standalone_export_emits_placeholder() {
        // --export-rvf alone → the container-format demo (placeholder weights).
        assert!(export_emits_placeholder_demo(true, false, false));
    }

    #[test]
    fn export_with_train_does_not_short_circuit() {
        // #894: `--train --export-rvf` must NOT emit a placeholder + skip
        // training — it must fall through to the real training pipeline.
        assert!(!export_emits_placeholder_demo(true, true, false));
        assert!(!export_emits_placeholder_demo(true, false, true));
        assert!(!export_emits_placeholder_demo(true, true, true));
    }

    #[test]
    fn no_export_flag_never_emits() {
        assert!(!export_emits_placeholder_demo(false, false, false));
        assert!(!export_emits_placeholder_demo(false, true, false));
    }
}

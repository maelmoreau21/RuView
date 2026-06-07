//! Rolling stale-baseline detector for ADR-135 calibration drift.

use std::collections::VecDeque;

use super::calibration::{
    BaselineCalibration, CalibrationConfig, CalibrationDeviationScore, CalibrationError,
    CalibrationRecorder,
};

/// Rolling drift detector configuration.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CalibrationDriftConfig {
    /// Non-overlapping window length in frames. Default: 300 frames.
    pub window_frames: usize,
    /// A completed window is stale when its median drift score is strictly above this value.
    pub drift_threshold: f32,
    /// Consecutive stale windows required before recommending recalibration.
    pub confirmed_windows: usize,
}

impl Default for CalibrationDriftConfig {
    fn default() -> Self {
        Self {
            window_frames: 300,
            drift_threshold: 4.0,
            confirmed_windows: 3,
        }
    }
}

impl CalibrationDriftConfig {
    fn normalized(self) -> Self {
        let default = Self::default();
        Self {
            window_frames: self.window_frames.max(1),
            drift_threshold: if self.drift_threshold.is_finite() {
                self.drift_threshold
            } else {
                default.drift_threshold
            },
            confirmed_windows: self.confirmed_windows.max(1),
        }
    }
}

/// Recommendation-only baseline drift state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalibrationDriftDecision {
    /// Baseline appears healthy, or not enough frames have completed a window.
    Healthy,
    /// One or more completed windows exceeded the drift threshold.
    Warn,
    /// Sustained drift exceeded the configured confirmation count.
    RecommendRecalibration,
}

/// Tracks calibration drift over ADR-135 rolling windows.
#[derive(Debug, Clone)]
pub struct CalibrationDriftDetector {
    config: CalibrationDriftConfig,
    current_window: VecDeque<f32>,
    frames_observed: u64,
    consecutive_drift_windows: usize,
    last_window_drift_score: f32,
    decision: CalibrationDriftDecision,
}

impl CalibrationDriftDetector {
    /// Create a detector using the provided configuration.
    pub fn new(config: CalibrationDriftConfig) -> Self {
        let config = config.normalized();
        Self {
            config,
            current_window: VecDeque::with_capacity(config.window_frames),
            frames_observed: 0,
            consecutive_drift_windows: 0,
            last_window_drift_score: 0.0,
            decision: CalibrationDriftDecision::Healthy,
        }
    }

    /// Ingest one deviation score and return the current drift decision.
    pub fn update(&mut self, score: CalibrationDeviationScore) -> CalibrationDriftDecision {
        self.update_drift_score(score.drift_score)
    }

    /// Ingest one scalar drift score and return the current drift decision.
    pub fn update_drift_score(&mut self, drift_score: f32) -> CalibrationDriftDecision {
        self.frames_observed += 1;
        self.current_window.push_back(sanitize_score(drift_score));

        if self.current_window.len() == self.config.window_frames {
            self.last_window_drift_score = median_window(&self.current_window);
            if self.last_window_drift_score > self.config.drift_threshold {
                self.consecutive_drift_windows += 1;
            } else {
                self.consecutive_drift_windows = 0;
            }
            self.current_window.clear();
            self.decision = self.window_decision();
        }

        self.decision
    }

    /// Reset rolling state after an operator completes recalibration.
    pub fn reset(&mut self) {
        self.current_window.clear();
        self.frames_observed = 0;
        self.consecutive_drift_windows = 0;
        self.last_window_drift_score = 0.0;
        self.decision = CalibrationDriftDecision::Healthy;
    }

    /// Detector configuration after normalization.
    pub fn config(&self) -> CalibrationDriftConfig {
        self.config
    }

    /// Total frames ingested since creation or reset.
    pub fn frames_observed(&self) -> u64 {
        self.frames_observed
    }

    /// Consecutive completed windows above threshold.
    pub fn confirmed_windows(&self) -> usize {
        self.consecutive_drift_windows
    }

    /// Median drift score from the most recently completed window.
    pub fn last_window_drift_score(&self) -> f32 {
        self.last_window_drift_score
    }

    /// Most recent recommendation-only decision.
    pub fn decision(&self) -> CalibrationDriftDecision {
        self.decision
    }

    fn window_decision(&self) -> CalibrationDriftDecision {
        if self.consecutive_drift_windows >= self.config.confirmed_windows {
            CalibrationDriftDecision::RecommendRecalibration
        } else if self.consecutive_drift_windows > 0 {
            CalibrationDriftDecision::Warn
        } else {
            CalibrationDriftDecision::Healthy
        }
    }
}

impl Default for CalibrationDriftDetector {
    fn default() -> Self {
        Self::new(CalibrationDriftConfig::default())
    }
}

/// Configuration for automatic quiet-room baseline refresh.
#[derive(Debug, Clone)]
pub struct AdaptiveCalibrationConfig {
    /// Baseline capture configuration used for initial and replacement baselines.
    pub calibration: CalibrationConfig,
    /// Rolling drift detector configuration.
    pub drift: CalibrationDriftConfig,
    /// Maximum short-window runtime variance accepted as "quiet".
    pub quiet_variance_threshold: f32,
    /// Maximum mean candidate amplitude variance accepted for promotion.
    pub max_candidate_amp_variance: f32,
    /// Maximum per-subcarrier Von Mises phase dispersion accepted for promotion.
    pub max_phase_dispersion: f32,
}

impl Default for AdaptiveCalibrationConfig {
    fn default() -> Self {
        Self {
            calibration: CalibrationConfig::ht20(),
            drift: CalibrationDriftConfig::default(),
            quiet_variance_threshold: 0.25,
            max_candidate_amp_variance: 0.05,
            max_phase_dispersion: 0.3,
        }
    }
}

/// Runtime state for adaptive calibration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdaptiveCalibrationStatus {
    /// No baseline has been promoted yet.
    CollectingInitial,
    /// An active baseline is being monitored for drift.
    Monitoring,
    /// Sustained quiet-room drift is collecting a replacement candidate.
    CollectingCandidate,
    /// A candidate was rejected and collection will restart on quiet frames.
    CandidateRejected,
}

/// Decision returned by one monitor update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdaptiveCalibrationDecision {
    /// Waiting for quiet, presence-free frames.
    WaitingForQuiet,
    /// No recalibration action was needed.
    Healthy,
    /// Drift exceeded one or more windows, but not the confirmation count.
    DriftWarn,
    /// A replacement candidate is being collected.
    CandidateCollecting,
    /// The initial boot baseline was promoted.
    InitialBaselinePromoted,
    /// A replacement baseline was promoted.
    CandidatePromoted,
    /// A candidate failed stability checks and was rejected.
    CandidateRejected,
}

/// Snapshot of adaptive calibration monitor state for metrics and APIs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AdaptiveCalibrationSnapshot {
    pub status: AdaptiveCalibrationStatus,
    pub last_decision: AdaptiveCalibrationDecision,
    pub frames_observed: u64,
    pub candidate_frames: u32,
    pub confirmed_drift_windows: usize,
    pub last_window_drift_score: f32,
    pub promotions: u64,
    pub rejections: u64,
    pub active_baseline_frames: Option<u64>,
}

/// Quiet-room adaptive baseline monitor.
#[derive(Debug, Clone)]
pub struct AdaptiveCalibrationMonitor {
    config: AdaptiveCalibrationConfig,
    active_baseline: Option<BaselineCalibration>,
    drift: CalibrationDriftDetector,
    candidate: Option<CalibrationRecorder>,
    frames_observed: u64,
    status: AdaptiveCalibrationStatus,
    last_decision: AdaptiveCalibrationDecision,
    promotions: u64,
    rejections: u64,
}

impl AdaptiveCalibrationMonitor {
    /// Create a monitor using the supplied configuration.
    pub fn new(config: AdaptiveCalibrationConfig) -> Self {
        Self {
            drift: CalibrationDriftDetector::new(config.drift),
            config,
            active_baseline: None,
            candidate: None,
            frames_observed: 0,
            status: AdaptiveCalibrationStatus::CollectingInitial,
            last_decision: AdaptiveCalibrationDecision::WaitingForQuiet,
            promotions: 0,
            rejections: 0,
        }
    }

    /// Ingest one live CSI frame.
    ///
    /// `presence_detected` must come from the caller's occupancy gate. The
    /// `short_window_variance` value should be low only when the room is
    /// stable; high values suppress candidate collection to avoid folding
    /// motion into the baseline.
    pub fn update(
        &mut self,
        frame: &wifi_densepose_core::types::CsiFrame,
        presence_detected: bool,
        short_window_variance: f32,
    ) -> Result<AdaptiveCalibrationDecision, CalibrationError> {
        self.frames_observed = self.frames_observed.saturating_add(1);
        let quiet = !presence_detected
            && short_window_variance.is_finite()
            && short_window_variance <= self.config.quiet_variance_threshold;

        let decision = if self.active_baseline.is_none() {
            self.update_initial(frame, quiet)?
        } else {
            self.update_candidate(frame, quiet)?
        };
        self.last_decision = decision;
        Ok(decision)
    }

    /// Active baseline, if one has been promoted.
    pub fn active_baseline(&self) -> Option<&BaselineCalibration> {
        self.active_baseline.as_ref()
    }

    /// Snapshot state for external reporting.
    pub fn snapshot(&self) -> AdaptiveCalibrationSnapshot {
        AdaptiveCalibrationSnapshot {
            status: self.status,
            last_decision: self.last_decision,
            frames_observed: self.frames_observed,
            candidate_frames: self
                .candidate
                .as_ref()
                .map(CalibrationRecorder::frames_recorded)
                .unwrap_or(0),
            confirmed_drift_windows: self.drift.confirmed_windows(),
            last_window_drift_score: self.drift.last_window_drift_score(),
            promotions: self.promotions,
            rejections: self.rejections,
            active_baseline_frames: self.active_baseline.as_ref().map(|b| b.frame_count),
        }
    }

    fn update_initial(
        &mut self,
        frame: &wifi_densepose_core::types::CsiFrame,
        quiet: bool,
    ) -> Result<AdaptiveCalibrationDecision, CalibrationError> {
        self.status = AdaptiveCalibrationStatus::CollectingInitial;
        if !quiet {
            self.candidate = None;
            return Ok(AdaptiveCalibrationDecision::WaitingForQuiet);
        }

        self.record_candidate(frame)?;
        if self.candidate_ready() {
            let baseline = self.finish_candidate()?;
            if self.candidate_is_stable(&baseline) {
                self.promote_baseline(baseline);
                Ok(AdaptiveCalibrationDecision::InitialBaselinePromoted)
            } else {
                self.reject_candidate();
                Ok(AdaptiveCalibrationDecision::CandidateRejected)
            }
        } else {
            Ok(AdaptiveCalibrationDecision::WaitingForQuiet)
        }
    }

    fn update_candidate(
        &mut self,
        frame: &wifi_densepose_core::types::CsiFrame,
        quiet: bool,
    ) -> Result<AdaptiveCalibrationDecision, CalibrationError> {
        let score = self
            .active_baseline
            .as_ref()
            .expect("checked by caller")
            .deviation(frame)?;
        let drift_decision = self.drift.update(score);

        if !quiet {
            self.candidate = None;
            self.status = AdaptiveCalibrationStatus::Monitoring;
            return Ok(match drift_decision {
                CalibrationDriftDecision::Healthy => AdaptiveCalibrationDecision::Healthy,
                CalibrationDriftDecision::Warn => AdaptiveCalibrationDecision::DriftWarn,
                CalibrationDriftDecision::RecommendRecalibration => {
                    AdaptiveCalibrationDecision::WaitingForQuiet
                }
            });
        }

        match drift_decision {
            CalibrationDriftDecision::Healthy => {
                self.candidate = None;
                self.status = AdaptiveCalibrationStatus::Monitoring;
                Ok(AdaptiveCalibrationDecision::Healthy)
            }
            CalibrationDriftDecision::Warn => {
                self.status = AdaptiveCalibrationStatus::Monitoring;
                Ok(AdaptiveCalibrationDecision::DriftWarn)
            }
            CalibrationDriftDecision::RecommendRecalibration => {
                self.status = AdaptiveCalibrationStatus::CollectingCandidate;
                self.record_candidate(frame)?;
                if self.candidate_ready() {
                    let baseline = self.finish_candidate()?;
                    if self.candidate_is_stable(&baseline) {
                        self.promote_baseline(baseline);
                        Ok(AdaptiveCalibrationDecision::CandidatePromoted)
                    } else {
                        self.reject_candidate();
                        Ok(AdaptiveCalibrationDecision::CandidateRejected)
                    }
                } else {
                    Ok(AdaptiveCalibrationDecision::CandidateCollecting)
                }
            }
        }
    }

    fn record_candidate(
        &mut self,
        frame: &wifi_densepose_core::types::CsiFrame,
    ) -> Result<(), CalibrationError> {
        self.candidate
            .get_or_insert_with(|| CalibrationRecorder::new(self.config.calibration))
            .record(frame)?;
        Ok(())
    }

    fn candidate_ready(&self) -> bool {
        self.candidate
            .as_ref()
            .is_some_and(|rec| rec.frames_recorded() >= self.config.calibration.min_frames)
    }

    fn finish_candidate(&mut self) -> Result<BaselineCalibration, CalibrationError> {
        self.candidate
            .take()
            .expect("candidate_ready checked")
            .finalize()
    }

    fn candidate_is_stable(&self, baseline: &BaselineCalibration) -> bool {
        if baseline.subcarriers.is_empty() {
            return false;
        }
        let mean_amp_variance = baseline
            .subcarriers
            .iter()
            .map(|sc| sc.amp_variance)
            .sum::<f32>()
            / baseline.subcarriers.len() as f32;
        let max_phase_dispersion = baseline
            .subcarriers
            .iter()
            .map(|sc| sc.phase_dispersion)
            .fold(0.0_f32, f32::max);

        mean_amp_variance <= self.config.max_candidate_amp_variance
            && max_phase_dispersion <= self.config.max_phase_dispersion
    }

    fn promote_baseline(&mut self, baseline: BaselineCalibration) {
        self.active_baseline = Some(baseline);
        self.drift.reset();
        self.status = AdaptiveCalibrationStatus::Monitoring;
        self.promotions = self.promotions.saturating_add(1);
    }

    fn reject_candidate(&mut self) {
        self.candidate = None;
        self.drift.reset();
        self.status = AdaptiveCalibrationStatus::CandidateRejected;
        self.rejections = self.rejections.saturating_add(1);
    }
}

impl Default for AdaptiveCalibrationMonitor {
    fn default() -> Self {
        Self::new(AdaptiveCalibrationConfig::default())
    }
}

fn sanitize_score(score: f32) -> f32 {
    if score.is_finite() {
        score.max(0.0)
    } else {
        0.0
    }
}

fn median_window(window: &VecDeque<f32>) -> f32 {
    let mut values: Vec<f32> = window.iter().copied().collect();
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = values.len() / 2;
    if values.len() % 2 == 0 {
        (values[mid - 1] + values[mid]) / 2.0
    } else {
        values[mid]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndarray::Array2;
    use num_complex::Complex64;
    use wifi_densepose_core::types::{
        AntennaConfig, CsiFrame, CsiMetadata, DeviceId, FrequencyBand,
    };

    fn test_config() -> CalibrationDriftConfig {
        CalibrationDriftConfig {
            window_frames: 4,
            drift_threshold: 4.0,
            confirmed_windows: 3,
        }
    }

    fn deviation(drift_score: f32) -> CalibrationDeviationScore {
        CalibrationDeviationScore {
            amplitude_z_median: drift_score.sqrt(),
            amplitude_z_max: drift_score.sqrt(),
            phase_drift_median: 0.0,
            drift_score,
            motion_flagged: false,
        }
    }

    #[test]
    fn defaults_match_adr_135() {
        let cfg = CalibrationDriftConfig::default();
        assert_eq!(cfg.window_frames, 300);
        assert_eq!(cfg.drift_threshold, 4.0);
        assert_eq!(cfg.confirmed_windows, 3);
    }

    #[test]
    fn healthy_when_windows_stay_below_threshold() {
        let mut detector = CalibrationDriftDetector::new(test_config());
        let mut decision = CalibrationDriftDecision::Healthy;
        for _ in 0..8 {
            decision = detector.update_drift_score(1.5);
        }

        assert_eq!(decision, CalibrationDriftDecision::Healthy);
        assert_eq!(detector.confirmed_windows(), 0);
        assert!(detector.last_window_drift_score() < 4.0);
    }

    #[test]
    fn warns_after_one_confirmed_drift_window() {
        let mut detector = CalibrationDriftDetector::new(test_config());
        let mut decision = CalibrationDriftDecision::Healthy;
        for _ in 0..4 {
            decision = detector.update_drift_score(4.5);
        }

        assert_eq!(decision, CalibrationDriftDecision::Warn);
        assert_eq!(detector.confirmed_windows(), 1);
        assert!(detector.last_window_drift_score() > 4.0);
    }

    #[test]
    fn recommends_only_after_three_confirmed_windows() {
        let mut detector = CalibrationDriftDetector::new(test_config());
        let mut decision = CalibrationDriftDecision::Healthy;
        for _ in 0..12 {
            decision = detector.update_drift_score(5.0);
        }

        assert_eq!(decision, CalibrationDriftDecision::RecommendRecalibration);
        assert_eq!(detector.confirmed_windows(), 3);
        assert_eq!(detector.frames_observed(), 12);
    }

    #[test]
    fn threshold_is_strictly_greater_than_four() {
        let mut detector = CalibrationDriftDetector::new(test_config());
        for _ in 0..4 {
            detector.update_drift_score(4.0);
        }

        assert_eq!(detector.decision(), CalibrationDriftDecision::Healthy);
        assert_eq!(detector.confirmed_windows(), 0);
    }

    #[test]
    fn deviation_api_recommends_until_explicit_operator_reset() {
        let mut detector = CalibrationDriftDetector::new(test_config());
        for _ in 0..12 {
            detector.update(deviation(5.0));
        }

        assert_eq!(
            detector.decision(),
            CalibrationDriftDecision::RecommendRecalibration
        );
        assert_eq!(detector.confirmed_windows(), 3);

        detector.reset();

        assert_eq!(detector.decision(), CalibrationDriftDecision::Healthy);
        assert_eq!(detector.confirmed_windows(), 0);
        assert_eq!(detector.frames_observed(), 0);
    }

    fn adaptive_config(min_frames: u32) -> AdaptiveCalibrationConfig {
        let mut calibration = CalibrationConfig::ht20();
        calibration.num_active = 2;
        calibration.num_subcarriers = 2;
        calibration.min_frames = min_frames;
        AdaptiveCalibrationConfig {
            calibration,
            drift: CalibrationDriftConfig {
                window_frames: 1,
                drift_threshold: 4.0,
                confirmed_windows: 1,
            },
            quiet_variance_threshold: 0.1,
            max_candidate_amp_variance: 0.01,
            max_phase_dispersion: 0.05,
        }
    }

    fn csi_frame(amp: f64, phase: f64) -> CsiFrame {
        let data = Array2::from_shape_vec(
            (1, 2),
            vec![
                Complex64::from_polar(amp, phase),
                Complex64::from_polar(amp, phase),
            ],
        )
        .unwrap();
        let mut meta =
            CsiMetadata::new(DeviceId::new("adaptive-test"), FrequencyBand::Band2_4GHz, 6);
        meta.bandwidth_mhz = 20;
        meta.antenna_config = AntennaConfig::new(1, 1);
        CsiFrame::new(meta, data)
    }

    #[test]
    fn adaptive_monitor_promotes_initial_quiet_baseline() {
        let mut monitor = AdaptiveCalibrationMonitor::new(adaptive_config(3));
        let mut decision = AdaptiveCalibrationDecision::WaitingForQuiet;
        for _ in 0..3 {
            decision = monitor.update(&csi_frame(1.0, 0.0), false, 0.0).unwrap();
        }

        assert_eq!(decision, AdaptiveCalibrationDecision::InitialBaselinePromoted);
        assert!(monitor.active_baseline().is_some());
        assert_eq!(monitor.snapshot().promotions, 1);
        assert_eq!(monitor.snapshot().status, AdaptiveCalibrationStatus::Monitoring);
    }

    #[test]
    fn adaptive_monitor_promotes_sustained_quiet_drift_candidate() {
        let mut monitor = AdaptiveCalibrationMonitor::new(adaptive_config(2));
        for _ in 0..2 {
            monitor.update(&csi_frame(1.0, 0.0), false, 0.0).unwrap();
        }

        let first = monitor.update(&csi_frame(3.0, 0.0), false, 0.0).unwrap();
        assert_eq!(first, AdaptiveCalibrationDecision::CandidateCollecting);
        let promoted = monitor.update(&csi_frame(3.0, 0.0), false, 0.0).unwrap();

        assert_eq!(promoted, AdaptiveCalibrationDecision::CandidatePromoted);
        let baseline = monitor.active_baseline().unwrap();
        assert!((baseline.subcarriers[0].amp_mean - 3.0).abs() < 1e-6);
        assert_eq!(monitor.snapshot().promotions, 2);
    }

    #[test]
    fn adaptive_monitor_does_not_collect_candidate_when_presence_detected() {
        let mut monitor = AdaptiveCalibrationMonitor::new(adaptive_config(2));
        for _ in 0..2 {
            monitor.update(&csi_frame(1.0, 0.0), false, 0.0).unwrap();
        }

        let decision = monitor.update(&csi_frame(3.0, 0.0), true, 0.0).unwrap();

        assert_eq!(decision, AdaptiveCalibrationDecision::WaitingForQuiet);
        assert_eq!(monitor.snapshot().candidate_frames, 0);
        assert_eq!(monitor.snapshot().promotions, 1);
    }

    #[test]
    fn adaptive_monitor_rejects_high_phase_dispersion_candidate() {
        let mut monitor = AdaptiveCalibrationMonitor::new(adaptive_config(4));
        for _ in 0..4 {
            monitor.update(&csi_frame(1.0, 0.0), false, 0.0).unwrap();
        }

        let phases = [0.0, std::f64::consts::PI, 0.0, std::f64::consts::PI];
        let mut decision = AdaptiveCalibrationDecision::Healthy;
        for phase in phases {
            decision = monitor.update(&csi_frame(3.0, phase), false, 0.0).unwrap();
        }

        assert_eq!(decision, AdaptiveCalibrationDecision::CandidateRejected);
        assert_eq!(monitor.snapshot().rejections, 1);
        assert_eq!(monitor.snapshot().status, AdaptiveCalibrationStatus::CandidateRejected);
    }
}

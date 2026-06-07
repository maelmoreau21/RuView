//! Rolling stale-baseline detector for ADR-135 calibration drift.

use std::collections::VecDeque;

use super::calibration::CalibrationDeviationScore;

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
}

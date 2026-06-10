use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::warn;
use wifi_densepose_sensing_server::alerts::AlertThresholds;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct FeatureFlags {
    #[serde(default)]
    pub stable: StableFeatures,
    #[serde(default)]
    pub beta: BetaFeatures,
    #[serde(default)]
    pub alert_thresholds: AlertThresholds,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct StableFeatures {
    #[serde(default = "enabled")]
    pub presence_detection: bool,
    #[serde(default = "enabled")]
    pub motion_detection: bool,
    #[serde(default = "enabled")]
    pub breathing_rate: bool,
    #[serde(default = "enabled")]
    pub zone_localization: bool,
    #[serde(default = "enabled")]
    pub fall_detection: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct BetaFeatures {
    #[serde(default)]
    pub skeleton_pose_estimation: bool,
    #[serde(default)]
    pub cardiac_arrest_detection: bool,
    #[serde(default)]
    pub precise_heart_rate: bool,
    #[serde(default)]
    pub precise_person_counting: bool,
    #[serde(default)]
    pub densepose_3d: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum BetaFeature {
    SkeletonPoseEstimation,
    CardiacArrestDetection,
    PreciseHeartRate,
    PrecisePersonCounting,
    Densepose3d,
}

impl BetaFeature {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::SkeletonPoseEstimation => "skeleton_pose_estimation",
            Self::CardiacArrestDetection => "cardiac_arrest_detection",
            Self::PreciseHeartRate => "precise_heart_rate",
            Self::PrecisePersonCounting => "precise_person_counting",
            Self::Densepose3d => "densepose_3d",
        }
    }

    fn from_str(value: &str) -> Option<Self> {
        match value.trim() {
            "skeleton_pose_estimation" | "pose" | "pose_estimation" => {
                Some(Self::SkeletonPoseEstimation)
            }
            "cardiac_arrest_detection" | "cardiac" | "cardiac_arrhythmia" => {
                Some(Self::CardiacArrestDetection)
            }
            "precise_heart_rate" | "heart_rate" => Some(Self::PreciseHeartRate),
            "precise_person_counting" | "person_counting" | "multi_person_counting" => {
                Some(Self::PrecisePersonCounting)
            }
            "densepose_3d" | "densepose" | "3d" => Some(Self::Densepose3d),
            _ => None,
        }
    }
}

impl Default for FeatureFlags {
    fn default() -> Self {
        Self {
            stable: StableFeatures::default(),
            beta: BetaFeatures::default(),
            alert_thresholds: AlertThresholds::default(),
        }
    }
}

impl Default for StableFeatures {
    fn default() -> Self {
        Self {
            presence_detection: true,
            motion_detection: true,
            breathing_rate: true,
            zone_localization: true,
            fall_detection: true,
        }
    }
}

impl Default for BetaFeatures {
    fn default() -> Self {
        Self {
            skeleton_pose_estimation: false,
            cardiac_arrest_detection: false,
            precise_heart_rate: false,
            precise_person_counting: false,
            densepose_3d: false,
        }
    }
}

impl FeatureFlags {
    pub(crate) fn load() -> Self {
        let mut flags = match find_config_path()
            .and_then(|path| load_from_path(&path).map(|flags| (path, flags)))
        {
            Some((path, flags)) => {
                tracing::info!("Feature flags loaded from {}", path.display());
                flags
            }
            None => {
                tracing::warn!(
                    "Feature flags file config/features.toml not found or invalid; using fail-closed beta defaults"
                );
                Self::default()
            }
        };
        flags.apply_beta_env();
        flags
    }

    pub(crate) fn beta_enabled(&self, feature: BetaFeature) -> bool {
        match feature {
            BetaFeature::SkeletonPoseEstimation => self.beta.skeleton_pose_estimation,
            BetaFeature::CardiacArrestDetection => self.beta.cardiac_arrest_detection,
            BetaFeature::PreciseHeartRate => self.beta.precise_heart_rate,
            BetaFeature::PrecisePersonCounting => self.beta.precise_person_counting,
            BetaFeature::Densepose3d => self.beta.densepose_3d,
        }
    }

    pub(crate) fn as_json(&self) -> serde_json::Value {
        serde_json::json!({
            "stable": self.stable_map(),
            "beta": self.beta_map(),
            "alert_thresholds": &self.alert_thresholds,
        })
    }

    pub(crate) fn active_names(&self) -> Vec<String> {
        self.stable_map()
            .into_iter()
            .chain(self.beta_map())
            .filter_map(|(name, enabled)| enabled.then_some(name))
            .collect()
    }

    pub(crate) fn disabled_names(&self) -> Vec<String> {
        self.stable_map()
            .into_iter()
            .chain(self.beta_map())
            .filter_map(|(name, enabled)| (!enabled).then_some(name))
            .collect()
    }

    fn apply_beta_env(&mut self) {
        let Ok(raw) = std::env::var("RUVSENSE_BETA_FEATURES") else {
            return;
        };
        for token in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            match BetaFeature::from_str(token) {
                Some(BetaFeature::SkeletonPoseEstimation) => self.beta.skeleton_pose_estimation = true,
                Some(BetaFeature::CardiacArrestDetection) => self.beta.cardiac_arrest_detection = true,
                Some(BetaFeature::PreciseHeartRate) => self.beta.precise_heart_rate = true,
                Some(BetaFeature::PrecisePersonCounting) => self.beta.precise_person_counting = true,
                Some(BetaFeature::Densepose3d) => self.beta.densepose_3d = true,
                None => warn!("Ignoring unknown beta feature in RUVSENSE_BETA_FEATURES: {token}"),
            }
        }
    }

    fn stable_map(&self) -> BTreeMap<String, bool> {
        BTreeMap::from([
            ("presence_detection".to_string(), self.stable.presence_detection),
            ("motion_detection".to_string(), self.stable.motion_detection),
            ("breathing_rate".to_string(), self.stable.breathing_rate),
            ("zone_localization".to_string(), self.stable.zone_localization),
            ("fall_detection".to_string(), self.stable.fall_detection),
        ])
    }

    fn beta_map(&self) -> BTreeMap<String, bool> {
        BTreeMap::from([
            (
                "skeleton_pose_estimation".to_string(),
                self.beta.skeleton_pose_estimation,
            ),
            (
                "cardiac_arrest_detection".to_string(),
                self.beta.cardiac_arrest_detection,
            ),
            ("precise_heart_rate".to_string(), self.beta.precise_heart_rate),
            (
                "precise_person_counting".to_string(),
                self.beta.precise_person_counting,
            ),
            ("densepose_3d".to_string(), self.beta.densepose_3d),
        ])
    }
}

fn enabled() -> bool {
    true
}

fn find_config_path() -> Option<PathBuf> {
    let mut candidates = vec![
        PathBuf::from("config/features.toml"),
        PathBuf::from("../config/features.toml"),
        PathBuf::from("../../config/features.toml"),
    ];
    candidates.push(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../..")
            .join("config/features.toml"),
    );
    candidates.into_iter().find(|path| path.is_file())
}

fn load_from_path(path: &Path) -> Option<FeatureFlags> {
    let content = std::fs::read_to_string(path)
        .map_err(|err| warn!("Failed to read {}: {err}", path.display()))
        .ok()?;
    toml::from_str::<FeatureFlags>(&content)
        .map_err(|err| warn!("Failed to parse {}: {err}", path.display()))
        .ok()
}

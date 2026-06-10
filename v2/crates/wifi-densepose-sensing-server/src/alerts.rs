use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AlertTrigger {
    Apnea,
    NoMotionExtended,
    PersonDetected,
    PersonLeft,
}

impl AlertTrigger {
    fn cooldown(self) -> Duration {
        match self {
            Self::Apnea => Duration::from_secs(60),
            Self::NoMotionExtended => Duration::from_secs(300),
            Self::PersonDetected | Self::PersonLeft => Duration::from_secs(30),
        }
    }

    fn id_prefix(self) -> &'static str {
        match self {
            Self::Apnea => "apnea",
            Self::NoMotionExtended => "no_motion_extended",
            Self::PersonDetected => "person_detected",
            Self::PersonLeft => "person_left",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertThresholds {
    #[serde(default = "default_apnea_trigger_seconds")]
    pub apnea_trigger_seconds: u64,
    #[serde(default = "default_apnea_min_confidence")]
    pub apnea_min_confidence: f64,
    #[serde(default = "default_no_motion_trigger_seconds")]
    pub no_motion_trigger_seconds: u64,
    #[serde(default = "default_presence_score_threshold")]
    pub presence_score_threshold: f64,
    #[serde(default = "default_motion_energy_threshold")]
    pub motion_energy_threshold: f64,
}

impl Default for AlertThresholds {
    fn default() -> Self {
        Self {
            apnea_trigger_seconds: default_apnea_trigger_seconds(),
            apnea_min_confidence: default_apnea_min_confidence(),
            no_motion_trigger_seconds: default_no_motion_trigger_seconds(),
            presence_score_threshold: default_presence_score_threshold(),
            motion_energy_threshold: default_motion_energy_threshold(),
        }
    }
}

fn default_apnea_trigger_seconds() -> u64 {
    15
}

fn default_apnea_min_confidence() -> f64 {
    0.30
}

fn default_no_motion_trigger_seconds() -> u64 {
    120
}

fn default_presence_score_threshold() -> f64 {
    0.70
}

fn default_motion_energy_threshold() -> f64 {
    0.02
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AlertSeverity {
    Info,
    Warning,
    Critical,
}

#[derive(Debug, Clone, Serialize)]
pub struct AlertEvent {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub alert_id: String,
    pub severity: AlertSeverity,
    pub title: String,
    pub message: String,
    pub person_id: u32,
    pub confidence: f64,
    pub timestamp_ms: u64,
    pub requires_ack: bool,
    #[serde(skip)]
    pub trigger: AlertTrigger,
    #[serde(skip)]
    pub acknowledged: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ActiveAlertsResponse {
    pub alerts: Vec<AlertEvent>,
    pub critical_count: usize,
    pub warning_count: usize,
}

#[derive(Debug, Clone)]
pub struct AlertSample {
    pub person_id: u32,
    pub breathing_bpm: Option<f64>,
    pub breathing_confidence: f64,
    pub presence_score: f64,
    pub motion_energy: f64,
    pub timestamp_ms: u64,
}

#[derive(Debug, Clone)]
pub struct AlertManager {
    thresholds: AlertThresholds,
    apnea_since: Option<Duration>,
    no_motion_since: Option<Duration>,
    person_left_since: Option<Duration>,
    previous_presence_score: Option<f64>,
    last_emitted: HashMap<AlertTrigger, Duration>,
    active_by_trigger: HashMap<AlertTrigger, String>,
    active_alerts: BTreeMap<String, AlertEvent>,
    acknowledged_triggers: HashSet<AlertTrigger>,
    next_sequence: u64,
}

impl AlertManager {
    pub fn new(thresholds: AlertThresholds) -> Self {
        Self {
            thresholds,
            apnea_since: None,
            no_motion_since: None,
            person_left_since: None,
            previous_presence_score: None,
            last_emitted: HashMap::new(),
            active_by_trigger: HashMap::new(),
            active_alerts: BTreeMap::new(),
            acknowledged_triggers: HashSet::new(),
            next_sequence: 1,
        }
    }

    pub fn evaluate(&mut self, sample: &AlertSample) -> Vec<AlertEvent> {
        let now = Duration::from_millis(sample.timestamp_ms);
        let mut emitted = Vec::new();

        if let Some(alert) = self.evaluate_apnea(sample, now) {
            emitted.push(alert);
        }
        if let Some(alert) = self.evaluate_no_motion(sample, now) {
            emitted.push(alert);
        }
        if let Some(alert) = self.evaluate_person_detected(sample, now) {
            emitted.push(alert);
        }
        if let Some(alert) = self.evaluate_person_left(sample, now) {
            emitted.push(alert);
        }

        self.previous_presence_score = Some(sample.presence_score);
        emitted
    }

    pub fn acknowledge(&mut self, alert_id: &str) -> bool {
        let Some(alert) = self.active_alerts.get_mut(alert_id) else {
            return false;
        };
        alert.acknowledged = true;
        self.acknowledged_triggers.insert(alert.trigger);
        true
    }

    pub fn active_response(&self) -> ActiveAlertsResponse {
        let alerts: Vec<_> = self
            .active_alerts
            .values()
            .filter(|alert| !alert.acknowledged)
            .cloned()
            .collect();
        let critical_count = alerts
            .iter()
            .filter(|alert| alert.severity == AlertSeverity::Critical)
            .count();
        let warning_count = alerts
            .iter()
            .filter(|alert| alert.severity == AlertSeverity::Warning)
            .count();
        ActiveAlertsResponse {
            alerts,
            critical_count,
            warning_count,
        }
    }

    fn evaluate_apnea(&mut self, sample: &AlertSample, now: Duration) -> Option<AlertEvent> {
        let reliable_low_breathing = sample
            .breathing_bpm
            .is_some_and(|bpm| bpm < 4.0)
            && sample.breathing_confidence > self.thresholds.apnea_min_confidence;

        if !reliable_low_breathing {
            self.apnea_since = None;
            self.clear_trigger(AlertTrigger::Apnea);
            return None;
        }

        let started = *self.apnea_since.get_or_insert(now);
        let elapsed = now.saturating_sub(started);
        if elapsed <= Duration::from_secs(self.thresholds.apnea_trigger_seconds) {
            return None;
        }

        let severity = if elapsed > Duration::from_secs(30) {
            AlertSeverity::Critical
        } else {
            AlertSeverity::Warning
        };
        let title = match severity {
            AlertSeverity::Critical => "Apnée confirmée",
            AlertSeverity::Warning => "Apnée possible",
            AlertSeverity::Info => "Apnée possible",
        };
        self.emit_or_repeat(
            AlertTrigger::Apnea,
            severity,
            title,
            format!(
                "Aucune respiration détectée depuis {}s",
                elapsed.as_secs()
            ),
            sample,
            now,
        )
    }

    fn evaluate_no_motion(&mut self, sample: &AlertSample, now: Duration) -> Option<AlertEvent> {
        let still_present = sample.presence_score > self.thresholds.presence_score_threshold
            && sample.motion_energy < self.thresholds.motion_energy_threshold;

        if !still_present {
            self.no_motion_since = None;
            self.clear_trigger(AlertTrigger::NoMotionExtended);
            return None;
        }

        let started = *self.no_motion_since.get_or_insert(now);
        let elapsed = now.saturating_sub(started);
        if elapsed <= Duration::from_secs(self.thresholds.no_motion_trigger_seconds) {
            return None;
        }

        self.emit_or_repeat(
            AlertTrigger::NoMotionExtended,
            AlertSeverity::Warning,
            "Absence de mouvement prolongée",
            format!("Aucun mouvement détecté depuis {}s", elapsed.as_secs()),
            sample,
            now,
        )
    }

    fn evaluate_person_detected(
        &mut self,
        sample: &AlertSample,
        now: Duration,
    ) -> Option<AlertEvent> {
        let crossed = self
            .previous_presence_score
            .is_some_and(|previous| previous < 0.5 && sample.presence_score > 0.7);
        if !crossed {
            return None;
        }

        self.clear_trigger(AlertTrigger::PersonLeft);
        self.emit_or_repeat(
            AlertTrigger::PersonDetected,
            AlertSeverity::Info,
            "Personne détectée",
            "Une présence fiable vient d'être détectée".to_string(),
            sample,
            now,
        )
    }

    fn evaluate_person_left(&mut self, sample: &AlertSample, now: Duration) -> Option<AlertEvent> {
        let previous_present = self
            .previous_presence_score
            .is_some_and(|previous| previous > 0.7);

        if previous_present && sample.presence_score < 0.3 {
            self.person_left_since = Some(now);
            return None;
        }

        if sample.presence_score >= 0.3 {
            self.person_left_since = None;
            return None;
        }

        let started = self.person_left_since?;
        let elapsed = now.saturating_sub(started);
        if elapsed <= Duration::from_secs(5) {
            return None;
        }

        self.clear_trigger(AlertTrigger::PersonDetected);
        self.emit_or_repeat(
            AlertTrigger::PersonLeft,
            AlertSeverity::Info,
            "Personne partie",
            "La présence fiable a disparu".to_string(),
            sample,
            now,
        )
    }

    fn emit_or_repeat(
        &mut self,
        trigger: AlertTrigger,
        severity: AlertSeverity,
        title: &str,
        message: String,
        sample: &AlertSample,
        now: Duration,
    ) -> Option<AlertEvent> {
        let active_id = self.active_by_trigger.get(&trigger).cloned();
        let active = active_id
            .as_deref()
            .and_then(|id| self.active_alerts.get(id));
        let severity_changed = active.is_some_and(|alert| alert.severity != severity);

        if !severity_changed
            && (active.is_some_and(|alert| alert.acknowledged)
                || self.acknowledged_triggers.contains(&trigger))
        {
            return None;
        }

        if !severity_changed && !self.cooldown_elapsed(trigger, now) {
            return None;
        }
        if severity_changed {
            self.acknowledged_triggers.remove(&trigger);
        }

        let alert_id = active_id.unwrap_or_else(|| self.next_alert_id(trigger));
        let alert = AlertEvent {
            msg_type: "alert".to_string(),
            alert_id: alert_id.clone(),
            severity,
            title: title.to_string(),
            message,
            person_id: sample.person_id,
            confidence: self.alert_confidence(sample),
            timestamp_ms: sample.timestamp_ms,
            requires_ack: false,
            trigger,
            acknowledged: false,
        };

        self.last_emitted.insert(trigger, now);
        self.active_by_trigger.insert(trigger, alert_id.clone());
        self.active_alerts.insert(alert_id, alert.clone());
        Some(alert)
    }

    fn cooldown_elapsed(&self, trigger: AlertTrigger, now: Duration) -> bool {
        self.last_emitted
            .get(&trigger)
            .map_or(true, |last| now.saturating_sub(*last) >= trigger.cooldown())
    }

    fn clear_trigger(&mut self, trigger: AlertTrigger) {
        if let Some(alert_id) = self.active_by_trigger.remove(&trigger) {
            self.active_alerts.remove(&alert_id);
        }
        self.acknowledged_triggers.remove(&trigger);
    }

    fn next_alert_id(&mut self, trigger: AlertTrigger) -> String {
        let id = format!("{}_{:03}", trigger.id_prefix(), self.next_sequence);
        self.next_sequence += 1;
        id
    }

    fn alert_confidence(&self, sample: &AlertSample) -> f64 {
        sample
            .breathing_confidence
            .max(sample.presence_score)
            .clamp(0.0, 1.0)
    }
}

impl Default for AlertManager {
    fn default() -> Self {
        Self::new(AlertThresholds::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_at(timestamp_ms: u64) -> AlertSample {
        AlertSample {
            person_id: 1,
            breathing_bpm: Some(12.0),
            breathing_confidence: 0.9,
            presence_score: 0.9,
            motion_energy: 0.5,
            timestamp_ms,
        }
    }

    #[test]
    fn breathing_bpm_null_does_not_trigger_apnea() {
        let mut manager = AlertManager::default();
        let mut sample = sample_at(0);
        sample.breathing_bpm = None;
        assert!(manager.evaluate(&sample).is_empty());

        sample.timestamp_ms = 20_000;
        assert!(manager.evaluate(&sample).is_empty());
    }

    #[test]
    fn low_breathing_confidence_does_not_trigger_apnea() {
        let mut manager = AlertManager::default();
        let mut sample = sample_at(0);
        sample.breathing_bpm = Some(2.0);
        sample.breathing_confidence = 0.1;
        assert!(manager.evaluate(&sample).is_empty());

        sample.timestamp_ms = 20_000;
        assert!(manager.evaluate(&sample).is_empty());
    }

    #[test]
    fn reliable_low_breathing_for_twenty_seconds_triggers_apnea() {
        let mut manager = AlertManager::default();
        let mut sample = sample_at(0);
        sample.breathing_bpm = Some(2.0);
        sample.breathing_confidence = 0.8;
        assert!(manager.evaluate(&sample).is_empty());

        sample.timestamp_ms = 20_000;
        let alerts = manager.evaluate(&sample);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].alert_id, "apnea_001");
        assert_eq!(alerts[0].severity, AlertSeverity::Warning);
    }

    #[test]
    fn absent_person_does_not_trigger_no_motion() {
        let mut manager = AlertManager::default();
        let mut sample = sample_at(0);
        sample.presence_score = 0.3;
        sample.motion_energy = 0.0;
        assert!(manager.evaluate(&sample).is_empty());

        sample.timestamp_ms = 130_000;
        assert!(manager.evaluate(&sample).is_empty());
    }
}

const MAX_ALERTS = 20;
const DEDUPE_MS = 30000;
let audioContext = null;
let alertContext = { source: 'dashboard' };
const recentKeys = new Map();

function ensureState() {
  if (!window.RS) window.RS = {};
  if (!Array.isArray(window.RS.alerts)) window.RS.alerts = [];
  return window.RS;
}

function canAlert(causeData) {
  if (causeData === undefined || causeData === null) return false;
  return alertContext.source !== 'dashboard' || Boolean(ensureState().connected);
}

function dispatchUpdate() {
  if (typeof window.dispatchEvent !== 'function' || typeof window.CustomEvent !== 'function') return;
  window.dispatchEvent(new window.CustomEvent('rs:update'));
  window.dispatchEvent(new window.CustomEvent('ruvsense:alerts-changed', {
    detail: { source: alertContext.source },
  }));
}

function getAudioContext() {
  const AudioCtor = window.AudioContext || window.webkitAudioContext;
  if (!AudioCtor) return null;
  if (!audioContext) audioContext = new AudioCtor();
  return audioContext;
}

function playCriticalBeep() {
  try {
    const ctx = getAudioContext();
    if (!ctx) return;
    if (ctx.state === 'suspended') void ctx.resume();
    for (let i = 0; i < 3; i += 1) {
      const oscillator = ctx.createOscillator();
      const gain = ctx.createGain();
      const start = ctx.currentTime + i * 0.42;
      oscillator.type = 'sine';
      oscillator.frequency.value = 880;
      gain.gain.setValueAtTime(0.0001, start);
      gain.gain.exponentialRampToValueAtTime(0.12, start + 0.02);
      gain.gain.exponentialRampToValueAtTime(0.0001, start + 0.3);
      oscillator.connect(gain);
      gain.connect(ctx.destination);
      oscillator.start(start);
      oscillator.stop(start + 0.32);
    }
  } catch {}
}

function notifyCritical(type, message) {
  try {
    if (!('Notification' in window) || Notification.permission !== 'granted') return;
    new Notification('RuvSense', {
      body: message || type || 'Alerte critique',
      tag: `ruvsense-${type || 'critical'}`,
    });
  } catch {}
}

export function addAlert(type, message, severity = 'info', causeData = message) {
  if (!canAlert(causeData)) return null;

  const state = ensureState();
  const normalizedSeverity = ['info', 'warning', 'critical'].includes(severity) ? severity : 'info';
  const key = `${type || 'info'}:${normalizedSeverity}:${message || '--'}`;
  const now = Date.now();
  const last = recentKeys.get(key) || 0;
  if (now - last < DEDUPE_MS) return null;
  recentKeys.set(key, now);

  const alert = {
    created_at: now,
    time: now,
    type: type || 'info',
    title: String(type || 'info').toUpperCase(),
    message: message || '--',
    severity: normalizedSeverity,
  };

  state.alerts.unshift(alert);
  state.alerts = state.alerts.slice(0, MAX_ALERTS);

  if (normalizedSeverity === 'critical') {
    playCriticalBeep();
    notifyCritical(type, message);
  }

  dispatchUpdate();
  return alert;
}

export function clearAlerts() {
  const state = ensureState();
  state.alerts = [];
  recentKeys.clear();
  dispatchUpdate();
}

export function getAlerts(options = {}) {
  const limit = Number(options.limit || MAX_ALERTS);
  return ensureState().alerts.slice(0, Number.isFinite(limit) ? limit : MAX_ALERTS);
}

export function initAlerts(options = {}) {
  alertContext = {
    ...alertContext,
    ...options,
  };
  ensureState();
  return window.RSAlerts;
}

function collectIncomingAlerts(snapshot) {
  return [
    snapshot?.alerts,
    snapshot?.latest?.alerts,
    snapshot?.vitals?.alerts,
    snapshot?.edgeVitals?.alerts,
  ].filter(Array.isArray).flat();
}

export function processAlertState(snapshot) {
  if (!snapshot || typeof snapshot !== 'object') return [];
  const created = [];

  for (const incoming of collectIncomingAlerts(snapshot)) {
    created.push(addAlert(
      incoming.type || incoming.title || 'alert',
      incoming.message || incoming.title || 'Alerte active',
      incoming.severity || 'warning',
      incoming,
    ));
  }

  const latest = snapshot.latest || snapshot;
  const classification = latest.classification || snapshot.classification || {};
  if (classification.fall_detected) {
    created.push(addAlert('fall', 'Chute détectée', 'warning', classification));
  }

  const vitalSigns = latest.vital_signs || snapshot.vitals?.vital_signs || snapshot.vitals || {};
  const breathing = Number(vitalSigns.breathing_rate_bpm ?? vitalSigns.breathing_bpm);
  if (Number.isFinite(breathing) && breathing > 0 && breathing < 4) {
    created.push(addAlert('apnea', 'Respiration critique', 'critical', vitalSigns));
  }

  return created.filter(Boolean);
}

window.addEventListener('pointerdown', () => {
  try {
    const ctx = getAudioContext();
    if (ctx?.state === 'suspended') void ctx.resume();
  } catch {}
}, { passive: true });

window.RSAlerts = {
  addAlert,
  clearAlerts,
  getAlerts,
  initAlerts,
  processAlertState,
};
window.addAlert = addAlert;

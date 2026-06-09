const CHANNEL_NAME = 'ruvsense';
const STORAGE_KEY = 'ruvsense:alerts';
const INSTANCE_ID = `${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}`;
const ALERT_TTL_MS = 24 * 60 * 60 * 1000;

let channel = null;
let sourceName = 'ui';
let initialized = false;
let audioContext = null;
let notificationRequest = null;
const alerts = new Map();

function now() {
  return Date.now();
}

function normalizeText(value) {
  return String(value ?? '')
    .normalize('NFD')
    .replace(/[\u0300-\u036f]/g, '')
    .toLowerCase();
}

function firstNumber(...values) {
  for (const value of values) {
    const n = Number(value);
    if (Number.isFinite(n)) return n;
  }
  return null;
}

function firstText(...values) {
  return values.find((value) => value !== undefined && value !== null && value !== '');
}

function clampPercent(value) {
  const n = Number(value);
  if (!Number.isFinite(n)) return null;
  return n <= 1 ? Math.round(n * 100) : Math.round(n);
}

function heartRateOf(source = {}) {
  return firstNumber(
    source.heart_rate_bpm,
    source.heartrate_bpm,
    source.hr_proxy_bpm,
    source.heart_rate,
    source.hr,
  );
}

function breathingRateOf(source = {}) {
  return firstNumber(
    source.breathing_rate_bpm,
    source.breathing_bpm,
    source.breathing_rate,
    source.respiration_bpm,
    source.br,
  );
}

function confidenceOf(...sources) {
  for (const source of sources) {
    const value = firstNumber(
      source?.signal_quality,
      source?.presence_score,
      source?.breathing_confidence,
      source?.heartbeat_confidence,
      source?.confidence,
    );
    if (value != null) return value;
  }
  return null;
}

function statusTextOf(...sources) {
  return normalizeText(sources.map((source) => {
    if (!source || typeof source !== 'object') return source;
    return [
      source.status,
      source.state,
      source.health_status,
      source.vital_status,
      source.alert_status,
      source.event_type,
      source.type,
      source.reason,
    ].filter(Boolean).join(' ');
  }).filter(Boolean).join(' '));
}

function frameFromInput(input) {
  if (!input || typeof input !== 'object') return {};
  if (input.latest || input.vitals || input.edgeVitals || input.pose || input.location) {
    return input.latest && typeof input.latest === 'object' ? input.latest : {};
  }
  return input;
}

function stateFromInput(input) {
  if (!input || typeof input !== 'object') return {};
  return input.latest || input.vitals || input.edgeVitals || input.pose || input.location ? input : { latest: input };
}

function vitalsEntries(input) {
  const state = stateFromInput(input);
  const frame = frameFromInput(input);
  const entries = [];
  const add = (id, source, context = {}) => {
    if (!source || typeof source !== 'object') return;
    const heart = heartRateOf(source);
    const breathing = breathingRateOf(source);
    const confidence = confidenceOf(source, context);
    const status = statusTextOf(source, context);
    if (heart == null && breathing == null && confidence == null && !status) return;
    entries.push({ id: id || 'global', heart, breathing, confidence, status, context });
  };

  add('frame', frame.vital_signs, frame);
  add('latest', frame, frame);
  add('rest', state.vitals?.vital_signs || state.vitals, state.vitals);
  add('edge', state.edgeVitals?.edge_vitals || state.edgeVitals, state.edgeVitals);

  const persons = Array.isArray(frame.persons) ? frame.persons : [];
  persons.forEach((person, index) => {
    const id = String(person?.id ?? person?.track_id ?? person?.person_id ?? `person_${index + 1}`);
    add(id, person?.vital_signs, person);
    add(id, person?.vitals, person);
    add(id, person, person);
  });

  return entries;
}

function hasCriticalWord(textValue) {
  return /\b(critical|critique|danger|emergency|urgence)\b/.test(textValue);
}

function hasApneaWord(textValue) {
  return /\b(apnea|apnee|apneic|respiratory_arrest|respiratory arrest)\b/.test(textValue);
}

function hasCardiacArrestWord(textValue) {
  return /\b(cardiac_arrest|cardiac arrest|heart_stop|heart stop|asystole|arret_cardiaque|arret cardiaque)\b/.test(textValue);
}

function hasFallWord(textValue) {
  return /\b(fall|fallen|falling|chute|collapsed|on_floor)\b/.test(textValue);
}

function alertId(type, subject) {
  return `health:${type}:${subject || 'global'}`;
}

function alertTitle(type) {
  if (type === 'apnea') return 'APNEE';
  if (type === 'cardiac_arrest') return 'ARRET CARDIAQUE';
  if (type === 'fall') return 'CHUTE';
  return 'ALERTE';
}

function alertMessage(type, entry = {}) {
  const subject = entry.id && entry.id !== 'global' ? ` ${entry.id}` : '';
  if (type === 'apnea') {
    const rate = entry.breathing != null ? ` (${entry.breathing.toFixed(1)} bpm)` : '';
    return `Respiration critique${subject}${rate}.`;
  }
  if (type === 'cardiac_arrest') {
    const rate = entry.heart != null ? ` (${Math.round(entry.heart)} bpm)` : '';
    return `Rythme cardiaque critique${subject}${rate}.`;
  }
  if (type === 'fall') return `Chute detectee${subject}.`;
  return `Etat critique detecte${subject}.`;
}

function derivedAlerts(input) {
  const state = stateFromInput(input);
  const frame = frameFromInput(input);
  const results = [];
  const vitals = vitalsEntries(input);

  for (const entry of vitals) {
    const status = entry.status || '';
    if (hasApneaWord(status) || entry.context?.apnea_detected === true || (entry.breathing != null && entry.breathing <= 3)) {
      results.push({
        id: alertId('apnea', entry.id),
        type: 'apnea',
        severity: 'critical',
        title: alertTitle('apnea'),
        message: alertMessage('apnea', entry),
      });
    }
    if (
      hasCardiacArrestWord(status)
      || entry.context?.cardiac_arrest === true
      || entry.context?.heart_stopped === true
      || (entry.heart != null && entry.heart <= 5)
    ) {
      results.push({
        id: alertId('cardiac_arrest', entry.id),
        type: 'cardiac_arrest',
        severity: 'critical',
        title: alertTitle('cardiac_arrest'),
        message: alertMessage('cardiac_arrest', entry),
      });
    }
    if (hasCriticalWord(status) && !hasApneaWord(status) && !hasCardiacArrestWord(status)) {
      results.push({
        id: alertId('critical', entry.id),
        type: 'critical',
        severity: 'critical',
        title: alertTitle('critical'),
        message: alertMessage('critical', entry),
      });
    }
  }

  const frameStatus = statusTextOf(frame, frame.classification, state.pose);
  const fallDetected = Boolean(
    frame?.classification?.fall_detected
    || frame?.fall_detected
    || state.pose?.fall_detected
    || hasFallWord(frameStatus),
  );
  if (fallDetected) {
    results.push({
      id: alertId('fall', 'global'),
      type: 'fall',
      severity: 'warning',
      title: alertTitle('fall'),
      message: alertMessage('fall'),
    });
  }

  const persons = Array.isArray(frame.persons) ? frame.persons : [];
  persons.forEach((person, index) => {
    const pose = normalizeText(firstText(person?.pose, person?.posture));
    const subject = String(person?.id ?? person?.track_id ?? person?.person_id ?? `person_${index + 1}`);
    if (hasFallWord(pose) || person?.fall_detected === true || Number(person?.fallProgress ?? person?.fall_progress ?? 0) >= 0.8) {
      results.push({
        id: alertId('fall', subject),
        type: 'fall',
        severity: 'warning',
        title: alertTitle('fall'),
        message: alertMessage('fall', { id: subject }),
      });
    }
  });

  return [...new Map(results.map((alert) => [alert.id, alert])).values()];
}

function abnormalVitals(input) {
  return vitalsEntries(input).some((entry) => {
    if (entry.breathing != null && (entry.breathing < 8 || entry.breathing > 28)) return true;
    if (entry.heart != null && (entry.heart < 50 || entry.heart > 130)) return true;
    const confidence = clampPercent(entry.confidence);
    return confidence != null && confidence < 40;
  });
}

export function assessGlobalHealth(input) {
  const detected = derivedAlerts(input);
  const hasCritical = detected.some((alert) => alert.severity === 'critical');
  if (hasCritical) {
    return { level: 'critical', label: 'CRITIQUE', reason: detected.find((alert) => alert.severity === 'critical')?.message || '' };
  }
  if (detected.length || abnormalVitals(input)) {
    return { level: 'warning', label: 'ANOMALIE', reason: detected[0]?.message || 'Vital signe hors plage.' };
  }
  return { level: 'normal', label: 'NORMAL', reason: 'Tous les vitaux connus sont normaux.' };
}

function loadAlerts() {
  alerts.clear();
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    const parsed = raw ? JSON.parse(raw) : [];
    const cutoff = now() - ALERT_TTL_MS;
    for (const alert of Array.isArray(parsed) ? parsed : []) {
      if (Number(alert.created_at || 0) >= cutoff) alerts.set(alert.id, alert);
    }
  } catch {
    alerts.clear();
  }
}

function persistAlerts() {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify([...alerts.values()]));
  } catch {}
}

function injectStyles() {
  if (document.getElementById('ruvsense-alert-styles')) return;
  const style = document.createElement('style');
  style.id = 'ruvsense-alert-styles';
  style.textContent = `
    .global-health-badge {
      min-height: 28px;
      display: inline-flex;
      align-items: center;
      justify-content: center;
      gap: 6px;
      border: 1px solid rgba(255,255,255,0.14);
      border-radius: 8px;
      padding: 5px 10px;
      color: #05120d;
      background: #39d18f;
      font: 800 11px Inter, system-ui, sans-serif;
      letter-spacing: 0;
      white-space: nowrap;
      box-shadow: 0 0 16px rgba(57, 209, 143, 0.22);
    }
    .global-health-badge--warning {
      background: #ffb020;
      color: #1d1200;
      box-shadow: 0 0 18px rgba(255, 176, 32, 0.24);
    }
    .global-health-badge--critical {
      background: #ff3040;
      color: #fff;
      border-color: rgba(255,255,255,0.3);
      animation: ruvsense-health-blink 0.85s infinite;
      box-shadow: 0 0 24px rgba(255, 48, 64, 0.36);
    }
    .ruvsense-floating-health {
      position: fixed;
      top: 14px;
      right: 14px;
      z-index: 95;
    }
    .ruvsense-alert-tray {
      position: fixed;
      top: 78px;
      right: 18px;
      z-index: 96;
      width: min(340px, calc(100vw - 28px));
      display: grid;
      gap: 8px;
      pointer-events: none;
    }
    .ruvsense-alert-card {
      pointer-events: auto;
      border: 1px solid rgba(255,255,255,0.16);
      border-radius: 8px;
      background: rgba(8, 16, 28, 0.94);
      color: #eef7f1;
      padding: 10px;
      box-shadow: 0 18px 46px rgba(0,0,0,0.32);
    }
    .ruvsense-alert-card[data-severity="critical"] {
      border-color: rgba(255, 48, 64, 0.62);
      background: rgba(55, 8, 13, 0.96);
    }
    .ruvsense-alert-card[data-severity="warning"] {
      border-color: rgba(255, 176, 32, 0.58);
    }
    .ruvsense-alert-card strong,
    .ruvsense-alert-card span {
      display: block;
    }
    .ruvsense-alert-card strong {
      font-size: 12px;
      letter-spacing: 0;
      color: #fff;
    }
    .ruvsense-alert-card span {
      margin-top: 4px;
      color: rgba(238, 247, 241, 0.78);
      font-size: 12px;
      line-height: 1.35;
    }
    .ruvsense-alert-card button {
      margin-top: 9px;
      min-height: 28px;
      border-radius: 6px;
      border: 1px solid rgba(255,255,255,0.18);
      background: rgba(255,255,255,0.08);
      color: #fff;
      padding: 5px 8px;
      font: 800 11px Inter, system-ui, sans-serif;
      cursor: pointer;
    }
    @keyframes ruvsense-health-blink {
      0%, 100% { opacity: 1; }
      50% { opacity: 0.52; }
    }
  `;
  document.head.append(style);
}

function ensureGlobalBadge() {
  let badge = document.getElementById('global-health-badge');
  if (badge) return badge;
  badge = document.createElement('div');
  badge.id = 'global-health-badge';
  badge.className = 'global-health-badge ruvsense-floating-health global-health-badge--normal';
  badge.textContent = 'NORMAL';
  document.body.append(badge);
  return badge;
}

function ensureAlertTray() {
  let tray = document.getElementById('ruvsense-alert-tray');
  if (tray) return tray;
  tray = document.createElement('div');
  tray.id = 'ruvsense-alert-tray';
  tray.className = 'ruvsense-alert-tray';
  tray.setAttribute('aria-live', 'assertive');
  document.body.append(tray);
  tray.addEventListener('click', (event) => {
    const button = event.target instanceof Element ? event.target.closest('[data-alert-ack]') : null;
    if (button) acknowledgeAlert(button.getAttribute('data-alert-ack'));
  });
  return tray;
}

export function updateGlobalHealth(assessment) {
  const badge = ensureGlobalBadge();
  const level = assessment?.level || 'normal';
  badge.classList.remove('global-health-badge--normal', 'global-health-badge--warning', 'global-health-badge--critical');
  badge.classList.add(`global-health-badge--${level}`);
  badge.textContent = assessment?.label || 'NORMAL';
  badge.title = assessment?.reason || '';
  badge.dataset.healthLevel = level;
}

function renderAlerts() {
  const tray = ensureAlertTray();
  const active = [...alerts.values()]
    .filter((alert) => !alert.acknowledged_at)
    .sort((a, b) => Number(b.created_at || 0) - Number(a.created_at || 0));
  tray.replaceChildren();
  for (const alert of active) {
    const card = document.createElement('div');
    card.className = 'ruvsense-alert-card';
    card.dataset.severity = alert.severity || 'warning';

    const title = document.createElement('strong');
    title.textContent = alert.title || 'ALERTE';

    const body = document.createElement('span');
    body.textContent = alert.message || 'Alerte active.';

    const button = document.createElement('button');
    button.type = 'button';
    button.textContent = 'Acquitter';
    button.setAttribute('data-alert-ack', alert.id);

    card.append(title, body, button);
    tray.append(card);
  }
}

function setupChannel() {
  if (channel || typeof BroadcastChannel === 'undefined') return;
  channel = new BroadcastChannel(CHANNEL_NAME);
  channel.addEventListener('message', (event) => {
    const message = event.data || {};
    if (!message || message.originId === INSTANCE_ID) return;
    if (message.type === 'alert:upsert' && message.alert) {
      upsertAlert(message.alert, { remote: true });
    } else if (message.type === 'alert:ack' && message.id) {
      acknowledgeAlert(message.id, { remote: true });
    }
  });
}

function broadcast(message) {
  try {
    channel?.postMessage({ ...message, originId: INSTANCE_ID, source: sourceName });
  } catch {}
}

async function ensureNotificationPermission() {
  if (!('Notification' in window)) return 'unsupported';
  if (Notification.permission !== 'default') return Notification.permission;
  if (!notificationRequest) {
    notificationRequest = Notification.requestPermission().finally(() => {
      notificationRequest = null;
    });
  }
  return notificationRequest;
}

async function notify(alert) {
  try {
    const permission = await ensureNotificationPermission();
    if (permission === 'granted') {
      new Notification(alert.title || 'RuvSense Alert', {
        body: alert.message || 'Alerte active.',
        tag: alert.id,
        renotify: true,
      });
    }
  } catch {}
}

function ensureAudioContext() {
  const AudioCtor = window.AudioContext || window.webkitAudioContext;
  if (!AudioCtor) return null;
  if (!audioContext) audioContext = new AudioCtor();
  return audioContext;
}

function playAlertSound(severity = 'warning') {
  try {
    const ctx = ensureAudioContext();
    if (!ctx) return;
    if (ctx.state === 'suspended') void ctx.resume();
    const frequencies = severity === 'critical' ? [880, 660, 880] : [520, 660];
    frequencies.forEach((frequency, index) => {
      const oscillator = ctx.createOscillator();
      const gain = ctx.createGain();
      oscillator.type = 'sine';
      oscillator.frequency.value = frequency;
      gain.gain.value = 0;
      oscillator.connect(gain);
      gain.connect(ctx.destination);
      const start = ctx.currentTime + index * 0.16;
      oscillator.start(start);
      gain.gain.linearRampToValueAtTime(0.11, start + 0.02);
      gain.gain.exponentialRampToValueAtTime(0.001, start + 0.13);
      oscillator.stop(start + 0.15);
    });
  } catch {}
}

function upsertAlert(nextAlert, options = {}) {
  if (!nextAlert?.id) return null;
  const existing = alerts.get(nextAlert.id);
  const timestamp = now();
  if (existing) {
    Object.assign(existing, nextAlert, { last_seen_at: timestamp });
    alerts.set(existing.id, existing);
    persistAlerts();
    renderAlerts();
    return existing;
  }

  const alert = {
    ...nextAlert,
    created_at: timestamp,
    last_seen_at: timestamp,
    acknowledged_at: null,
    source: nextAlert.source || sourceName,
  };
  alerts.set(alert.id, alert);
  persistAlerts();
  renderAlerts();

  playAlertSound(alert.severity);
  if (!options.remote) {
    void notify(alert);
    broadcast({ type: 'alert:upsert', alert });
  }
  return alert;
}

export function triggerAlert(alert) {
  return upsertAlert(alert);
}

export function acknowledgeAlert(id, options = {}) {
  if (!id || !alerts.has(id)) return;
  const alert = alerts.get(id);
  alert.acknowledged_at = now();
  alerts.set(id, alert);
  persistAlerts();
  renderAlerts();
  if (!options.remote) broadcast({ type: 'alert:ack', id });
}

export function processAlertState(input) {
  const current = derivedAlerts(input);
  const seen = new Set(current.map((alert) => alert.id));
  for (const alert of current) upsertAlert(alert);

  for (const [id, alert] of [...alerts.entries()]) {
    if (alert.acknowledged_at && !seen.has(id)) alerts.delete(id);
  }

  persistAlerts();
  renderAlerts();
  const assessment = assessGlobalHealth(input);
  updateGlobalHealth(assessment);
  return assessment;
}

export function initAlerts(options = {}) {
  sourceName = options.source || sourceName;
  if (initialized) return { processAlertState, triggerAlert, acknowledgeAlert };
  initialized = true;
  injectStyles();
  ensureGlobalBadge();
  ensureAlertTray();
  loadAlerts();
  setupChannel();
  renderAlerts();
  updateGlobalHealth({ level: 'normal', label: 'NORMAL', reason: '' });

  window.addEventListener('pointerdown', () => {
    try {
      const ctx = ensureAudioContext();
      if (ctx?.state === 'suspended') void ctx.resume();
    } catch {}
  }, { passive: true });

  return { processAlertState, triggerAlert, acknowledgeAlert };
}

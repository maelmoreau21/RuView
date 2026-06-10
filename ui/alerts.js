(function () {
  const MAX_ALERTS = 20;
  const DEDUPE_MS = 30000;
  let audioContext = null;
  const recentKeys = new Map();

  function rs() {
    return window.RS || {};
  }

  function canAlert(causeData) {
    return Boolean(rs().connected) && causeData !== undefined && causeData !== null;
  }

  function dispatchUpdate() {
    if (typeof window.dispatchEvent === 'function' && typeof window.CustomEvent === 'function') {
      window.dispatchEvent(new window.CustomEvent('rs:update'));
    }
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

  function addAlert(type, message, severity = 'info', causeData = message) {
    if (!canAlert(causeData)) return null;

    const normalizedSeverity = ['info', 'warning', 'critical'].includes(severity) ? severity : 'info';
    const key = `${type || 'info'}:${normalizedSeverity}:${message || '--'}`;
    const now = Date.now();
    const last = recentKeys.get(key) || 0;
    if (now - last < DEDUPE_MS) return null;
    recentKeys.set(key, now);

    const alert = {
      time: now,
      type: type || 'info',
      message: message || '--',
      severity: normalizedSeverity,
    };

    window.RS.alerts.unshift(alert);
    window.RS.alerts = window.RS.alerts.slice(0, MAX_ALERTS);

    if (normalizedSeverity === 'critical') {
      playCriticalBeep();
      notifyCritical(type, message);
    }

    dispatchUpdate();
    return alert;
  }

  function clearAlerts() {
    if (!window.RS) return;
    window.RS.alerts = [];
    recentKeys.clear();
    dispatchUpdate();
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
  };
  window.addAlert = addAlert;
})();

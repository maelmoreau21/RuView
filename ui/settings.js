(function () {
  'use strict';

  const NODE_COUNT = 6;
  const DEFAULT_WIDTH = 5;
  const DEFAULT_HEIGHT = 4;
  const DEFAULT_PORT = '3000';
  const FPS_VALUES = [5, 10, 20, 30];
  const DEFAULT_DISPLAY = {
    canvas_fps: 10,
    show_grid: true,
    show_node_ranges: true,
    show_position_trail: true,
  };
  const DEFAULT_ALERTS = {
    sound_enabled: true,
    apnea_seconds: 15,
    no_motion_seconds: 120,
    breathing_confidence: 0.3,
  };
  const IDS = [
    'settings-connection-dot',
    'settings-host-summary',
    'settings-clock',
    'room-width-input',
    'room-height-input',
    'room-reset-button',
    'room-save-button',
    'room-status',
    'room-preview-canvas',
    'node-settings-grid',
    'connection-ip-input',
    'connection-port-input',
    'connection-save-button',
    'connection-test-button',
    'connection-status',
    'connection-state',
    'connection-version',
    'connection-url',
    'alerts-sound-input',
    'alerts-apnea-input',
    'alerts-apnea-value',
    'alerts-no-motion-input',
    'alerts-no-motion-value',
    'alerts-confidence-input',
    'alerts-confidence-value',
    'alerts-save-button',
    'alerts-status',
    'display-fps-input',
    'display-fps-value',
    'display-grid-input',
    'display-node-ranges-input',
    'display-trail-input',
    'display-save-button',
    'display-status',
  ];

  const els = {};
  const nodeCanvases = [];
  let room = defaultRoom(DEFAULT_WIDTH, DEFAULT_HEIGHT);
  let display = normalizeDisplay(loadJsonStorage('ruvsense_display', DEFAULT_DISPLAY));
  let animationFrame = 0;

  function $(id) {
    return document.getElementById(id);
  }

  function getElements() {
    IDS.forEach((id) => {
      els[id] = $(id);
    });
  }

  function safeNumber(value, fallback) {
    const n = Number(value);
    return Number.isFinite(n) ? n : fallback;
  }

  function clamp(value, min, max) {
    return Math.max(min, Math.min(max, value));
  }

  function roundMeters(value) {
    return Math.round(value * 100) / 100;
  }

  function defaultNodes(width, height) {
    const w = clamp(safeNumber(width, DEFAULT_WIDTH), 1, 30);
    const h = clamp(safeNumber(height, DEFAULT_HEIGHT), 1, 30);
    return [
      { id: 1, x: 0, y: 0, active: true, label: 'Node 1' },
      { id: 2, x: w, y: 0, active: true, label: 'Node 2' },
      { id: 3, x: w, y: h, active: true, label: 'Node 3' },
      { id: 4, x: 0, y: h, active: true, label: 'Node 4' },
      { id: 5, x: roundMeters(w / 2), y: 0, active: false, label: 'Node 5' },
      { id: 6, x: roundMeters(w / 2), y: h, active: false, label: 'Node 6' },
    ];
  }

  function defaultRoom(width, height) {
    const roomWidth = clamp(safeNumber(width, DEFAULT_WIDTH), 1, 30);
    const roomHeight = clamp(safeNumber(height, DEFAULT_HEIGHT), 1, 30);
    return {
      room_width_meters: roomWidth,
      room_height_meters: roomHeight,
      nodes: defaultNodes(roomWidth, roomHeight),
    };
  }

  function readNodeCoordinate(raw, key, width, height, fallback) {
    const value = safeNumber(
      raw?.[key] ?? raw?.position?.[key] ?? (key === 'x' ? raw?.position_m?.[0] : raw?.position_m?.[1]),
      fallback,
    );
    return roundMeters(clamp(value, 0, key === 'x' ? width : height));
  }

  function normalizeRoomConfig(config) {
    const width = clamp(
      safeNumber(
        config?.room_width_meters ?? config?.width_meters ?? config?.width_m ?? config?.dimensions?.width,
        DEFAULT_WIDTH,
      ),
      1,
      30,
    );
    const height = clamp(
      safeNumber(
        config?.room_height_meters ?? config?.height_meters ?? config?.height_m ?? config?.dimensions?.height,
        DEFAULT_HEIGHT,
      ),
      1,
      30,
    );
    const defaults = defaultNodes(width, height);
    const rawNodes = Array.isArray(config?.nodes) ? config.nodes : [];
    const nodes = defaults.map((fallback, index) => {
      const raw = rawNodes.find((node) => String(node?.id ?? node?.node_id) === String(index + 1)) || rawNodes[index];
      return {
        id: index + 1,
        x: readNodeCoordinate(raw, 'x', width, height, fallback.x),
        y: readNodeCoordinate(raw, 'y', width, height, fallback.y),
        active: raw ? raw.active !== false : fallback.active,
        label: String(raw?.label || raw?.name || fallback.label),
      };
    });
    return { room_width_meters: width, room_height_meters: height, nodes };
  }

  function normalizeDisplay(value) {
    const fps = FPS_VALUES.includes(Number(value?.canvas_fps)) ? Number(value.canvas_fps) : DEFAULT_DISPLAY.canvas_fps;
    return {
      canvas_fps: fps,
      show_grid: value?.show_grid !== false,
      show_node_ranges: value?.show_node_ranges !== false,
      show_position_trail: value?.show_position_trail !== false,
    };
  }

  function loadJsonStorage(key, fallback) {
    try {
      const raw = localStorage.getItem(key);
      if (!raw) return { ...fallback };
      return { ...fallback, ...JSON.parse(raw) };
    } catch {
      return { ...fallback };
    }
  }

  function localStorageValue(key, fallback) {
    try {
      return localStorage.getItem(key) || fallback;
    } catch {
      return fallback;
    }
  }

  function setStatus(el, message, state) {
    if (!el) return;
    el.textContent = message;
    el.classList.toggle('is-ok', state === 'ok');
    el.classList.toggle('is-error', state === 'error');
  }

  function setConnectionOnline(online) {
    els['settings-connection-dot'].classList.toggle('is-online', online);
    els['settings-connection-dot'].classList.toggle('is-offline', !online);
  }

  function updateClock() {
    const now = new Date();
    els['settings-clock'].textContent = now.toLocaleTimeString('fr-FR', {
      hour: '2-digit',
      minute: '2-digit',
      second: '2-digit',
      hour12: false,
    });
    els['settings-clock'].dateTime = now.toISOString();
  }

  function updateHostSummary() {
    const host = els['connection-ip-input'].value.trim() || 'localhost';
    const port = els['connection-port-input'].value.trim() || DEFAULT_PORT;
    els['settings-host-summary'].textContent = `${host}:${port}`;
    els['connection-url'].textContent = `${hostForBaseUrl()}/api/v1/version`;
  }

  function hostForBaseUrl() {
    let host = els['connection-ip-input'].value.trim() || 'localhost';
    if (/^https?:\/\//i.test(host)) {
      try {
        const url = new URL(host);
        const port = els['connection-port-input'].value.trim();
        if (port) url.port = port;
        url.pathname = '';
        url.search = '';
        url.hash = '';
        return url.toString().replace(/\/$/, '');
      } catch {
        host = 'localhost';
      }
    }
    host = host.replace(/^\/+|\/+$/g, '').replace(/\/.*$/, '');
    const parts = host.split(':');
    if (parts.length === 2 && /^\d+$/.test(parts[1])) host = parts[0];
    if (host.includes(':') && !host.startsWith('[')) host = `[${host}]`;
    return `http://${host}:${els['connection-port-input'].value.trim() || DEFAULT_PORT}`;
  }

  async function readJson(response) {
    const text = await response.text();
    if (!text) return null;
    try {
      return JSON.parse(text);
    } catch {
      return text;
    }
  }

  async function fetchApi(path, options = {}) {
    const controller = new AbortController();
    const timeout = window.setTimeout(() => controller.abort(), 4000);
    try {
      const response = await fetch(`${hostForBaseUrl()}/api/v1/${path}`, {
        cache: 'no-store',
        ...options,
        headers: {
          ...(options.headers || {}),
        },
        signal: controller.signal,
      });
      return { ok: response.ok, status: response.status, data: await readJson(response) };
    } catch (error) {
      return { ok: false, status: 0, error };
    } finally {
      window.clearTimeout(timeout);
    }
  }

  async function loadRoomConfig() {
    try {
      const response = await fetch('room-config.json', { cache: 'no-store' });
      if (!response.ok) throw new Error('room-config');
      room = normalizeRoomConfig(await response.json());
    } catch {
      room = defaultRoom(DEFAULT_WIDTH, DEFAULT_HEIGHT);
    }
    syncRoomInputs();
    renderNodeCards();
  }

  function syncRoomInputs() {
    els['room-width-input'].value = room.room_width_meters;
    els['room-height-input'].value = room.room_height_meters;
    room.nodes.forEach((node) => {
      const active = $(`node-${node.id}-active`);
      const x = $(`node-${node.id}-x`);
      const y = $(`node-${node.id}-y`);
      if (active) active.checked = Boolean(node.active);
      if (x) {
        x.max = room.room_width_meters;
        x.value = node.x;
      }
      if (y) {
        y.max = room.room_height_meters;
        y.value = node.y;
      }
    });
  }

  function updateRoomFromInputs(syncNodes) {
    room.room_width_meters = roundMeters(clamp(safeNumber(els['room-width-input'].value, DEFAULT_WIDTH), 1, 30));
    room.room_height_meters = roundMeters(clamp(safeNumber(els['room-height-input'].value, DEFAULT_HEIGHT), 1, 30));
    room.nodes.forEach((node) => {
      const active = $(`node-${node.id}-active`);
      const x = $(`node-${node.id}-x`);
      const y = $(`node-${node.id}-y`);
      node.active = active ? active.checked : node.active;
      node.x = roundMeters(clamp(safeNumber(x?.value, node.x), 0, room.room_width_meters));
      node.y = roundMeters(clamp(safeNumber(y?.value, node.y), 0, room.room_height_meters));
    });
    if (syncNodes) syncRoomInputs();
  }

  function renderNodeCards() {
    const container = els['node-settings-grid'];
    container.replaceChildren();
    nodeCanvases.length = 0;
    room.nodes.forEach((node) => {
      const card = document.createElement('article');
      card.className = 'card node-settings-card';
      card.innerHTML = `
        <div class="node-settings-head">
          <h3>${node.label || `Node ${node.id}`}</h3>
          <label class="settings-check node-active-check">
            <input id="node-${node.id}-active" type="checkbox">
            <span>Actif</span>
          </label>
        </div>
        <canvas id="node-${node.id}-canvas" class="node-live-canvas" width="200" height="200" aria-label="Aperçu Node ${node.id}"></canvas>
        <div class="node-coordinate-row">
          <div class="settings-field">
            <label for="node-${node.id}-x">Position X (mètres)</label>
            <input id="node-${node.id}-x" type="number" min="0" max="${room.room_width_meters}" step="0.1" inputmode="decimal">
          </div>
          <div class="settings-field">
            <label for="node-${node.id}-y">Position Y (mètres)</label>
            <input id="node-${node.id}-y" type="number" min="0" max="${room.room_height_meters}" step="0.1" inputmode="decimal">
          </div>
        </div>
      `;
      container.append(card);
      nodeCanvases.push($(`node-${node.id}-canvas`));
      [`node-${node.id}-active`, `node-${node.id}-x`, `node-${node.id}-y`].forEach((id) => {
        $(id).addEventListener('input', () => updateRoomFromInputs(false));
      });
    });
    syncRoomInputs();
  }

  function resetRoomToCorners() {
    const width = clamp(safeNumber(els['room-width-input'].value, DEFAULT_WIDTH), 1, 30);
    const height = clamp(safeNumber(els['room-height-input'].value, DEFAULT_HEIGHT), 1, 30);
    room = defaultRoom(width, height);
    renderNodeCards();
    setStatus(els['room-status'], '✅ Valeurs réinitialisées', 'ok');
  }

  function roomPayload() {
    updateRoomFromInputs(true);
    return {
      room_width_meters: room.room_width_meters,
      room_height_meters: room.room_height_meters,
      nodes: room.nodes.map((node) => ({
        id: node.id,
        x: node.x,
        y: node.y,
        label: node.label || `Node ${node.id}`,
      })),
    };
  }

  function broadcastConfigUpdated() {
    if (typeof BroadcastChannel !== 'function') return;
    try {
      const channel = new BroadcastChannel('ruvsense-config');
      channel.postMessage({ type: 'config_updated' });
      channel.close();
    } catch {
      // Best effort only.
    }
  }

  async function saveRoom() {
    const button = els['room-save-button'];
    button.disabled = true;
    setStatus(els['room-status'], 'Sauvegarde...', null);
    const result = await fetchApi('config/room', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(roomPayload()),
    });
    if (result.ok) {
      setStatus(els['room-status'], '✅ Pièce sauvegardée', 'ok');
      broadcastConfigUpdated();
    } else {
      const message = result.data?.message ? ` - ${result.data.message}` : '';
      setStatus(els['room-status'], `❌ Erreur ${result.status || ''}${message}`.trim(), 'error');
    }
    button.disabled = false;
  }

  function loadConnection() {
    els['connection-ip-input'].value = localStorageValue('ruvsense_ip', 'localhost');
    els['connection-port-input'].value = localStorageValue('ruvsense_port', DEFAULT_PORT);
    updateHostSummary();
  }

  function saveConnection() {
    localStorage.setItem('ruvsense_ip', els['connection-ip-input'].value.trim() || 'localhost');
    localStorage.setItem('ruvsense_port', els['connection-port-input'].value.trim() || DEFAULT_PORT);
    updateHostSummary();
    setStatus(els['connection-status'], '✅ Connexion sauvegardée', 'ok');
  }

  async function testConnection() {
    const button = els['connection-test-button'];
    button.disabled = true;
    saveConnection();
    setStatus(els['connection-status'], 'Test...', null);
    els['connection-state'].textContent = 'Test';
    els['connection-version'].textContent = '--';
    const result = await fetchApi('version');
    if (result.ok) {
      const data = result.data || {};
      const version = typeof data === 'string' ? data : data.version || data.server_version || data.git_version || 'OK';
      els['connection-state'].textContent = 'Connecté';
      els['connection-version'].textContent = version;
      setConnectionOnline(true);
      setStatus(els['connection-status'], `✅ Connecté - version ${version}`, 'ok');
    } else {
      els['connection-state'].textContent = 'Hors ligne';
      setConnectionOnline(false);
      setStatus(els['connection-status'], '❌ Impossible de joindre le serveur', 'error');
    }
    button.disabled = false;
  }

  function loadAlerts() {
    els['alerts-sound-input'].checked = localStorageValue('ruvsense_alert_sound', String(DEFAULT_ALERTS.sound_enabled)) !== 'false';
    els['alerts-apnea-input'].value = DEFAULT_ALERTS.apnea_seconds;
    els['alerts-no-motion-input'].value = DEFAULT_ALERTS.no_motion_seconds;
    els['alerts-confidence-input'].value = DEFAULT_ALERTS.breathing_confidence;
    updateAlertLabels();
  }

  function updateAlertLabels() {
    els['alerts-apnea-value'].textContent = `${els['alerts-apnea-input'].value}s`;
    els['alerts-no-motion-value'].textContent = `${els['alerts-no-motion-input'].value}s`;
    els['alerts-confidence-value'].textContent = Number(els['alerts-confidence-input'].value).toFixed(1);
  }

  function alertsPayload() {
    return {
      sound_enabled: els['alerts-sound-input'].checked,
      apnea_seconds: Math.round(clamp(safeNumber(els['alerts-apnea-input'].value, DEFAULT_ALERTS.apnea_seconds), 10, 60)),
      no_motion_seconds: Math.round(clamp(safeNumber(els['alerts-no-motion-input'].value, DEFAULT_ALERTS.no_motion_seconds), 60, 300)),
      breathing_confidence: clamp(safeNumber(els['alerts-confidence-input'].value, DEFAULT_ALERTS.breathing_confidence), 0.1, 0.9),
    };
  }

  async function saveAlerts() {
    const button = els['alerts-save-button'];
    button.disabled = true;
    setStatus(els['alerts-status'], 'Sauvegarde...', null);
    localStorage.setItem('ruvsense_alert_sound', String(els['alerts-sound-input'].checked));
    const result = await fetchApi('config/alerts', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(alertsPayload()),
    });
    setStatus(
      els['alerts-status'],
      result.ok ? '✅ Alertes sauvegardées' : `❌ Erreur ${result.status || ''}`.trim(),
      result.ok ? 'ok' : 'error',
    );
    button.disabled = false;
  }

  function syncDisplayInputs() {
    els['display-fps-input'].value = String(FPS_VALUES.indexOf(display.canvas_fps));
    els['display-grid-input'].checked = Boolean(display.show_grid);
    els['display-node-ranges-input'].checked = Boolean(display.show_node_ranges);
    els['display-trail-input'].checked = Boolean(display.show_position_trail);
    updateDisplayLabel();
  }

  function updateDisplayLabel() {
    const fps = FPS_VALUES[Number(els['display-fps-input'].value)] || DEFAULT_DISPLAY.canvas_fps;
    els['display-fps-value'].textContent = `${fps} FPS`;
  }

  function displayPayload() {
    return {
      canvas_fps: FPS_VALUES[Number(els['display-fps-input'].value)] || DEFAULT_DISPLAY.canvas_fps,
      show_grid: els['display-grid-input'].checked,
      show_node_ranges: els['display-node-ranges-input'].checked,
      show_position_trail: els['display-trail-input'].checked,
    };
  }

  function saveDisplay(showStatus) {
    display = displayPayload();
    localStorage.setItem('ruvsense_display', JSON.stringify(display));
    if (showStatus) setStatus(els['display-status'], '✅ Affichage sauvegardé', 'ok');
  }

  function roomMetrics(canvas, padding) {
    const width = canvas.width;
    const height = canvas.height;
    const roomW = clamp(safeNumber(room.room_width_meters, DEFAULT_WIDTH), 1, 30);
    const roomH = clamp(safeNumber(room.room_height_meters, DEFAULT_HEIGHT), 1, 30);
    const availableW = width - padding.left - padding.right;
    const availableH = height - padding.top - padding.bottom;
    const scale = Math.min(availableW / roomW, availableH / roomH);
    return {
      width,
      height,
      roomW,
      roomH,
      scale,
      x: padding.left + (availableW - roomW * scale) / 2,
      y: padding.top + (availableH - roomH * scale) / 2,
      w: roomW * scale,
      h: roomH * scale,
    };
  }

  function drawGrid(ctx, metrics) {
    if (!display.show_grid) return;
    ctx.strokeStyle = '#1f2937';
    ctx.lineWidth = 1;
    ctx.beginPath();
    for (let x = 0; x <= metrics.roomW + 0.001; x += 1) {
      const sx = metrics.x + x * metrics.scale;
      ctx.moveTo(sx, metrics.y);
      ctx.lineTo(sx, metrics.y + metrics.h);
    }
    for (let y = 0; y <= metrics.roomH + 0.001; y += 1) {
      const sy = metrics.y + y * metrics.scale;
      ctx.moveTo(metrics.x, sy);
      ctx.lineTo(metrics.x + metrics.w, sy);
    }
    ctx.stroke();
  }

  function drawCanvas(canvas, focusIndex, now) {
    const ctx = canvas.getContext('2d');
    const padding = canvas.width === 200
      ? { left: 20, right: 20, top: 20, bottom: 28 }
      : { left: 42, right: 32, top: 28, bottom: 42 };
    const metrics = roomMetrics(canvas, padding);
    ctx.clearRect(0, 0, metrics.width, metrics.height);
    ctx.fillStyle = '#0d1117';
    ctx.fillRect(0, 0, metrics.width, metrics.height);
    drawGrid(ctx, metrics);
    ctx.strokeStyle = '#374151';
    ctx.lineWidth = 2;
    ctx.strokeRect(metrics.x, metrics.y, metrics.w, metrics.h);
    room.nodes.forEach((node, index) => {
      const sx = metrics.x + (node.x / metrics.roomW) * metrics.w;
      const sy = metrics.y + (node.y / metrics.roomH) * metrics.h;
      const focused = index === focusIndex;
      if (display.show_node_ranges && node.active) {
        ctx.beginPath();
        ctx.fillStyle = focused ? 'rgba(59, 130, 246, 0.14)' : 'rgba(59, 130, 246, 0.06)';
        ctx.arc(sx, sy, focused ? 30 : 20, 0, Math.PI * 2);
        ctx.fill();
      }
      if (focused) {
        const pulse = 0.5 + Math.sin(now / 280) * 0.5;
        ctx.beginPath();
        ctx.fillStyle = `rgba(59, 130, 246, ${0.08 + pulse * 0.08})`;
        ctx.arc(sx, sy, 22 + pulse * 10, 0, Math.PI * 2);
        ctx.fill();
      }
      ctx.beginPath();
      ctx.fillStyle = node.active ? (focused ? '#3b82f6' : '#64748b') : '#374151';
      ctx.arc(sx, sy, focused ? 8 : 5, 0, Math.PI * 2);
      ctx.fill();
      ctx.strokeStyle = node.active ? 'rgba(249, 250, 251, 0.68)' : 'rgba(107, 114, 128, 0.42)';
      ctx.lineWidth = focused ? 2 : 1;
      ctx.stroke();
      ctx.fillStyle = focused ? '#f9fafb' : '#6b7280';
      ctx.font = `${focused ? 700 : 600} ${canvas.width === 200 ? 10 : 12}px system-ui, sans-serif`;
      ctx.textAlign = 'center';
      ctx.textBaseline = 'top';
      ctx.fillText(`N${index + 1}`, sx, sy + 11);
    });
    ctx.fillStyle = '#6b7280';
    ctx.font = `${canvas.width === 200 ? 10 : 12}px system-ui, sans-serif`;
    ctx.textAlign = 'left';
    ctx.textBaseline = 'bottom';
    ctx.fillText(`${metrics.roomW.toFixed(1)}m x ${metrics.roomH.toFixed(1)}m`, 10, metrics.height - 8);
  }

  function drawAllCanvases(now) {
    nodeCanvases.forEach((canvas, index) => drawCanvas(canvas, index, now));
    drawCanvas(els['room-preview-canvas'], -1, now);
    animationFrame = requestAnimationFrame(drawAllCanvases);
  }

  function activateTab(name) {
    document.querySelectorAll('.settings-tab').forEach((button) => {
      const active = button.dataset.tab === name;
      button.classList.toggle('is-active', active);
      button.setAttribute('aria-selected', String(active));
    });
    document.querySelectorAll('.settings-panel').forEach((panel) => {
      const active = panel.id === `tab-${name}`;
      panel.classList.toggle('is-active', active);
      panel.hidden = !active;
    });
  }

  function bindEvents() {
    document.querySelectorAll('.settings-tab').forEach((button) => {
      button.addEventListener('click', () => activateTab(button.dataset.tab));
    });
    els['room-width-input'].addEventListener('input', () => updateRoomFromInputs(true));
    els['room-height-input'].addEventListener('input', () => updateRoomFromInputs(true));
    els['room-reset-button'].addEventListener('click', resetRoomToCorners);
    els['room-save-button'].addEventListener('click', saveRoom);
    els['connection-ip-input'].addEventListener('input', updateHostSummary);
    els['connection-port-input'].addEventListener('input', updateHostSummary);
    els['connection-save-button'].addEventListener('click', saveConnection);
    els['connection-test-button'].addEventListener('click', testConnection);
    ['alerts-apnea-input', 'alerts-no-motion-input', 'alerts-confidence-input'].forEach((id) => {
      els[id].addEventListener('input', updateAlertLabels);
    });
    els['alerts-save-button'].addEventListener('click', saveAlerts);
    ['display-fps-input', 'display-grid-input', 'display-node-ranges-input', 'display-trail-input'].forEach((id) => {
      els[id].addEventListener('input', () => {
        updateDisplayLabel();
        saveDisplay(false);
      });
    });
    els['display-save-button'].addEventListener('click', () => saveDisplay(true));
  }

  async function init() {
    getElements();
    updateClock();
    window.setInterval(updateClock, 1000);
    loadConnection();
    loadAlerts();
    syncDisplayInputs();
    bindEvents();
    await loadRoomConfig();
    setStatus(els['room-status'], '--', null);
    setStatus(els['connection-status'], '--', null);
    setStatus(els['alerts-status'], '--', null);
    setStatus(els['display-status'], '--', null);
    animationFrame = requestAnimationFrame(drawAllCanvases);
  }

  window.addEventListener('beforeunload', () => {
    if (animationFrame) cancelAnimationFrame(animationFrame);
  });

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', init, { once: true });
  } else {
    void init();
  }
})();

(function () {
  'use strict';

  const MAX_NODES = 6;
  const DEFAULT_PORT = '3000';
  const GRID_PX = 50;
  const DEFAULT_GRID_METERS = 0.5;
  const FPS_VALUES = [5, 10, 20, 30];
  const DEFAULT_ROOM = {
    version: 2,
    room: {
      shape: 'polygon',
      boundary: [
        { x: 0, y: 0 },
        { x: 5, y: 0 },
        { x: 5, y: 4 },
        { x: 0, y: 4 },
      ],
    },
    nodes: [
      { id: 1, x: 0, y: 0, active: true },
      { id: 2, x: 5, y: 0, active: true },
      { id: 3, x: 2.5, y: 4, active: true },
    ],
  };
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
    'room-load-button',
    'room-reset-button',
    'room-save-button',
    'room-status',
    'room-editor-canvas',
    'room-mode-draw-button',
    'room-mode-nodes-button',
    'room-close-polygon-button',
    'room-suggest-button',
    'room-grid-step-input',
    'room-add-node-button',
    'room-remove-node-button',
    'room-live-node-count',
    'room-node-list',
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
  let room = cloneRoom(DEFAULT_ROOM);
  let polygonClosed = true;
  let editorMode = 'draw';
  let dragNodeId = null;
  let animationFrame = 0;
  let display = normalizeDisplay(loadJsonStorage('ruvsense_display', DEFAULT_DISPLAY));

  function $(id) {
    return document.getElementById(id);
  }

  function getElements() {
    IDS.forEach((id) => {
      els[id] = $(id);
    });
  }

  function safeNumber(value, fallback = null) {
    const n = Number(value);
    return Number.isFinite(n) ? n : fallback;
  }

  function clamp(value, min, max) {
    return Math.max(min, Math.min(max, value));
  }

  function roundMeters(value) {
    return Math.round(value * 100) / 100;
  }

  function cloneRoom(value) {
    return {
      version: 2,
      room: {
        shape: 'polygon',
        boundary: (value?.room?.boundary || []).map((point) => ({
          x: roundMeters(safeNumber(point?.x, 0)),
          y: roundMeters(safeNumber(point?.y, 0)),
        })),
      },
      nodes: (value?.nodes || []).slice(0, MAX_NODES).map((node, index) => ({
        id: safeNumber(node?.id, index + 1),
        x: roundMeters(safeNumber(node?.x, 0)),
        y: roundMeters(safeNumber(node?.y, 0)),
        active: node?.active !== false,
      })),
    };
  }

  function normalizeRoomConfig(value) {
    if (!value || value.version !== 2 || value.room?.shape !== 'polygon') return null;
    const boundary = Array.isArray(value.room.boundary)
      ? value.room.boundary
        .map((point) => {
          const x = safeNumber(point?.x);
          const y = safeNumber(point?.y);
          return x == null || y == null ? null : { x: roundMeters(x), y: roundMeters(y) };
        })
        .filter(Boolean)
      : [];
    if (boundary.length < 3) return null;

    const nodes = Array.isArray(value.nodes)
      ? value.nodes
        .slice(0, MAX_NODES)
        .map((node, index) => {
          const x = safeNumber(node?.x);
          const y = safeNumber(node?.y);
          if (x == null || y == null) return null;
          return {
            id: clamp(Math.round(safeNumber(node?.id, index + 1)), 1, MAX_NODES),
            x: roundMeters(x),
            y: roundMeters(y),
            active: node?.active !== false,
          };
        })
        .filter(Boolean)
      : [];

    if (!nodes.length) return null;
    const ids = new Set();
    nodes.forEach((node, index) => {
      while (ids.has(node.id)) node.id = clamp(index + 1, 1, MAX_NODES);
      ids.add(node.id);
    });

    return { version: 2, room: { shape: 'polygon', boundary }, nodes };
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

  function defaultConnectionHost() {
    return window.location.protocol === 'file:' ? 'localhost' : (window.location.hostname || 'localhost');
  }

  function defaultConnectionPort() {
    return window.location.protocol === 'file:' ? DEFAULT_PORT : (window.location.port || DEFAULT_PORT);
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
    const timeout = window.setTimeout(() => controller.abort(), 5000);
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

  function serverMessage(result) {
    if (typeof result?.data === 'string') return result.data;
    if (result?.data?.message) return String(result.data.message);
    if (result?.data?.error && result?.data?.reason) {
      const field = result.data.field ? `${result.data.field}: ` : '';
      return `${field}${result.data.reason}`;
    }
    if (result?.data?.error) return String(result.data.error);
    if (result?.error?.message) return String(result.error.message);
    return `HTTP ${result?.status || 0}`;
  }

  function gridMeters() {
    return clamp(safeNumber(els['room-grid-step-input']?.value, DEFAULT_GRID_METERS), 0.1, 5);
  }

  function roomBounds() {
    const boundary = room.room.boundary;
    if (!boundary.length) return { minX: 0, minY: 0, maxX: 5, maxY: 4, width: 5, height: 4 };
    const xs = boundary.map((point) => point.x);
    const ys = boundary.map((point) => point.y);
    const minX = Math.min(...xs);
    const minY = Math.min(...ys);
    const maxX = Math.max(...xs);
    const maxY = Math.max(...ys);
    return {
      minX,
      minY,
      maxX,
      maxY,
      width: Math.max(0.001, maxX - minX),
      height: Math.max(0.001, maxY - minY),
    };
  }

  function fitGridToRoom() {
    const canvas = els['room-editor-canvas'];
    const bounds = roomBounds();
    const maxCells = Math.max(1, Math.floor(canvas.width / GRID_PX) - 1);
    const needed = Math.max(bounds.maxX, bounds.maxY) / maxCells;
    if (needed > gridMeters()) {
      els['room-grid-step-input'].value = roundMeters(Math.min(5, Math.max(DEFAULT_GRID_METERS, needed))).toFixed(1);
    }
  }

  function meterToCanvas(point) {
    const pixelsPerMeter = GRID_PX / gridMeters();
    return {
      x: point.x * pixelsPerMeter,
      y: point.y * pixelsPerMeter,
    };
  }

  function canvasToMeter(event) {
    const canvas = els['room-editor-canvas'];
    const rect = canvas.getBoundingClientRect();
    const px = clamp(((event.clientX - rect.left) / rect.width) * canvas.width, 0, canvas.width);
    const py = clamp(((event.clientY - rect.top) / rect.height) * canvas.height, 0, canvas.height);
    const metersPerPixel = gridMeters() / GRID_PX;
    return {
      x: roundMeters(px * metersPerPixel),
      y: roundMeters(py * metersPerPixel),
    };
  }

  function drawGrid(ctx, canvas) {
    ctx.strokeStyle = '#1f2937';
    ctx.lineWidth = 1;
    ctx.beginPath();
    for (let x = 0; x <= canvas.width; x += GRID_PX) {
      ctx.moveTo(x, 0);
      ctx.lineTo(x, canvas.height);
    }
    for (let y = 0; y <= canvas.height; y += GRID_PX) {
      ctx.moveTo(0, y);
      ctx.lineTo(canvas.width, y);
    }
    ctx.stroke();

    ctx.fillStyle = '#6b7280';
    ctx.font = '10px system-ui, sans-serif';
    ctx.textAlign = 'left';
    ctx.textBaseline = 'top';
    for (let x = GRID_PX; x < canvas.width; x += GRID_PX * 2) {
      ctx.fillText(`${roundMeters((x / GRID_PX) * gridMeters())}m`, x + 3, 3);
    }
  }

  function drawPolygon(ctx) {
    const boundary = room.room.boundary;
    if (!boundary.length) return;
    ctx.beginPath();
    boundary.forEach((point, index) => {
      const p = meterToCanvas(point);
      if (index === 0) ctx.moveTo(p.x, p.y);
      else ctx.lineTo(p.x, p.y);
    });
    if (polygonClosed && boundary.length >= 3) ctx.closePath();
    if (polygonClosed && boundary.length >= 3) {
      ctx.fillStyle = 'rgba(59, 130, 246, 0.08)';
      ctx.fill();
    }
    ctx.strokeStyle = '#3b82f6';
    ctx.lineWidth = 2;
    ctx.stroke();

    boundary.forEach((point, index) => {
      const p = meterToCanvas(point);
      ctx.beginPath();
      ctx.fillStyle = '#0d1117';
      ctx.arc(p.x, p.y, 5, 0, Math.PI * 2);
      ctx.fill();
      ctx.strokeStyle = '#bfdbfe';
      ctx.stroke();
      ctx.fillStyle = '#dbeafe';
      ctx.font = '700 10px system-ui, sans-serif';
      ctx.textAlign = 'center';
      ctx.textBaseline = 'middle';
      ctx.fillText(String(index + 1), p.x, p.y - 14);
    });
  }

  function drawNodes(ctx) {
    room.nodes.forEach((node) => {
      const p = meterToCanvas(node);
      if (node.active) {
        ctx.beginPath();
        ctx.fillStyle = 'rgba(16, 185, 129, 0.10)';
        ctx.arc(p.x, p.y, 22, 0, Math.PI * 2);
        ctx.fill();
      }
      ctx.beginPath();
      ctx.fillStyle = node.active ? '#10b981' : '#374151';
      ctx.arc(p.x, p.y, 13, 0, Math.PI * 2);
      ctx.fill();
      ctx.strokeStyle = node.active ? '#d1fae5' : '#9ca3af';
      ctx.lineWidth = dragNodeId === node.id ? 3 : 1.5;
      ctx.stroke();
      ctx.fillStyle = '#ffffff';
      ctx.font = '800 11px system-ui, sans-serif';
      ctx.textAlign = 'center';
      ctx.textBaseline = 'middle';
      ctx.fillText(`N${node.id}`, p.x, p.y);
    });
  }

  function drawEditor() {
    const canvas = els['room-editor-canvas'];
    const ctx = canvas.getContext('2d');
    ctx.clearRect(0, 0, canvas.width, canvas.height);
    ctx.fillStyle = '#0d1117';
    ctx.fillRect(0, 0, canvas.width, canvas.height);
    drawGrid(ctx, canvas);
    drawPolygon(ctx);
    drawNodes(ctx);
    animationFrame = requestAnimationFrame(drawEditor);
  }

  function renderNodeList() {
    const container = els['room-node-list'];
    container.replaceChildren();
    room.nodes.forEach((node) => {
      const row = document.createElement('label');
      row.className = 'room-node-row';
      row.classList.toggle('is-inactive', !node.active);

      const pill = document.createElement('span');
      pill.className = 'room-node-pill';
      pill.textContent = `N${node.id}`;

      const coords = document.createElement('span');
      coords.className = 'room-node-coords';
      const activeText = node.active ? 'actif' : 'inactif';
      coords.innerHTML = `<strong>${node.x.toFixed(2)}m, ${node.y.toFixed(2)}m</strong>${activeText}`;

      const checkbox = document.createElement('input');
      checkbox.type = 'checkbox';
      checkbox.checked = node.active;
      checkbox.setAttribute('aria-label', `N${node.id} actif`);
      checkbox.addEventListener('change', () => {
        node.active = checkbox.checked;
        renderNodeList();
      });

      row.append(pill, coords, checkbox);
      container.append(row);
    });
    els['room-add-node-button'].disabled = room.nodes.length >= MAX_NODES;
    els['room-remove-node-button'].disabled = room.nodes.length <= 1;
  }

  function setEditorMode(mode) {
    editorMode = mode;
    const draw = mode === 'draw';
    els['room-mode-draw-button'].classList.toggle('is-active', draw);
    els['room-mode-nodes-button'].classList.toggle('is-active', !draw);
    els['room-mode-draw-button'].setAttribute('aria-pressed', String(draw));
    els['room-mode-nodes-button'].setAttribute('aria-pressed', String(!draw));
  }

  function hitNode(point) {
    return [...room.nodes].reverse().find((node) => {
      const p = meterToCanvas(node);
      const pointer = meterToCanvas(point);
      return Math.hypot(p.x - pointer.x, p.y - pointer.y) <= 18;
    }) || null;
  }

  function addPolygonPoint(point) {
    if (polygonClosed && room.room.boundary.length >= 3) {
      room.room.boundary = [];
      polygonClosed = false;
    }
    room.room.boundary.push(point);
    setStatus(els['room-status'], `${room.room.boundary.length} point(s)`, null);
  }

  function closestDefaultPoint(index) {
    const boundary = room.room.boundary;
    if (boundary[index]) return boundary[index];
    const bounds = roomBounds();
    const points = [
      { x: bounds.minX, y: bounds.minY },
      { x: bounds.maxX, y: bounds.minY },
      { x: bounds.maxX, y: bounds.maxY },
      { x: bounds.minX, y: bounds.maxY },
      { x: bounds.minX + bounds.width / 2, y: bounds.minY },
      { x: bounds.minX + bounds.width / 2, y: bounds.maxY },
    ];
    return points[index % points.length];
  }

  function addNode() {
    if (room.nodes.length >= MAX_NODES) return;
    const used = new Set(room.nodes.map((node) => node.id));
    let id = 1;
    while (used.has(id) && id <= MAX_NODES) id += 1;
    const point = closestDefaultPoint(room.nodes.length);
    room.nodes.push({ id, x: roundMeters(point.x), y: roundMeters(point.y), active: true });
    room.nodes.sort((a, b) => a.id - b.id);
    renderNodeList();
  }

  function removeNode() {
    if (room.nodes.length <= 1) return;
    room.nodes.pop();
    renderNodeList();
  }

  function closePolygon() {
    if (room.room.boundary.length < 3) {
      setStatus(els['room-status'], 'Ajoutez au moins 3 points avant de fermer la forme', 'error');
      return;
    }
    polygonClosed = true;
    setStatus(els['room-status'], 'Forme fermée', 'ok');
  }

  // Diagnostic 422: the old payload sent nodes[].label and omitted nodes[].active,
  // while the Rust API now accepts only schema v2 { version, room.boundary, nodes[].active }.
  function roomPayload() {
    return {
      version: 2,
      room: {
        shape: 'polygon',
        boundary: room.room.boundary.map((point) => ({
          x: roundMeters(point.x),
          y: roundMeters(point.y),
        })),
      },
      nodes: room.nodes.map((node) => ({
        id: Number(node.id),
        x: roundMeters(node.x),
        y: roundMeters(node.y),
        active: Boolean(node.active),
      })),
    };
  }

  function applyRoomConfig(config, message = null) {
    room = cloneRoom(config);
    polygonClosed = true;
    fitGridToRoom();
    renderNodeList();
    if (message) setStatus(els['room-status'], message, 'ok');
  }

  async function loadRoomConfig() {
    const result = await fetchApi('config/room');
    if (!result.ok) {
      setStatus(els['room-status'], `Chargement impossible - ${serverMessage(result)}`, 'error');
      applyRoomConfig(DEFAULT_ROOM);
      return null;
    }
    const config = normalizeRoomConfig(result.data);
    if (!config) {
      setStatus(els['room-status'], 'Configuration reçue invalide', 'error');
      applyRoomConfig(DEFAULT_ROOM);
      return null;
    }
    applyRoomConfig(config, 'Configuration actuelle chargée');
    return config;
  }

  function resetRoomToDefault() {
    applyRoomConfig(DEFAULT_ROOM, 'Valeurs réinitialisées');
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
    const payload = roomPayload();
    if (payload.room.boundary.length < 3) {
      setStatus(els['room-status'], 'La pièce doit contenir au moins 3 points', 'error');
      return;
    }
    button.disabled = true;
    setStatus(els['room-status'], 'Sauvegarde...', null);
    const result = await fetchApi('config/room', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(payload),
    });
    if (result.ok) {
      setStatus(els['room-status'], 'Pièce sauvegardée', 'ok');
      broadcastConfigUpdated();
    } else {
      setStatus(els['room-status'], serverMessage(result), 'error');
    }
    button.disabled = false;
  }

  function liveNodes() {
    const values = Object.values(window.RS?.nodes || {});
    return values.filter((node) => {
      const status = String(node?.status || node?.health_status || '').toLowerCase();
      if (node?.active === false || ['offline', 'stale', 'sync_only', 'no_nodes'].includes(status)) return false;
      const last = safeNumber(node?.last_seen ?? node?.lastSeen ?? node?.last_seen_ms);
      if (last == null) return node?.active === true || status === 'live';
      if (last > 100000000000) return Date.now() - last < 10000;
      if (last > 1000000000) return Date.now() - last * 1000 < 10000;
      return last < 10000;
    });
  }

  function updateLiveNodeCount() {
    const count = liveNodes().length;
    els['room-live-node-count'].textContent = String(count);
    return count;
  }

  function suggestStarterPositions() {
    const activeCount = updateLiveNodeCount();
    if (activeCount < 3) {
      setStatus(els['room-status'], '3 nœuds actifs requis via WebSocket', 'error');
      return;
    }
    const count = clamp(activeCount, 3, MAX_NODES);
    const base = [
      { x: 0, y: 0 },
      { x: 5, y: 0 },
      { x: 2.5, y: 4 },
      { x: 5, y: 4 },
      { x: 0, y: 4 },
      { x: 2.5, y: 0 },
    ];
    const square = [
      { x: 0, y: 0 },
      { x: 5, y: 0 },
      { x: 5, y: 4 },
      { x: 0, y: 4 },
      { x: 2.5, y: 0 },
      { x: 2.5, y: 4 },
    ];
    const positions = count === 3 ? base : square;
    room = {
      version: 2,
      room: {
        shape: 'polygon',
        boundary: [
          { x: 0, y: 0 },
          { x: 5, y: 0 },
          { x: 5, y: 4 },
          { x: 0, y: 4 },
        ],
      },
      nodes: positions.slice(0, count).map((point, index) => ({
        id: index + 1,
        x: point.x,
        y: point.y,
        active: true,
      })),
    };
    polygonClosed = true;
    els['room-grid-step-input'].value = DEFAULT_GRID_METERS;
    renderNodeList();
    setStatus(els['room-status'], 'Position de départ approximative — ajustez selon votre pièce réelle', 'ok');
  }

  function loadConnection() {
    els['connection-ip-input'].value = localStorageValue('ruvsense_ip', defaultConnectionHost());
    els['connection-port-input'].value = localStorageValue('ruvsense_port', defaultConnectionPort());
    updateHostSummary();
  }

  function saveConnection() {
    localStorage.setItem('ruvsense_ip', els['connection-ip-input'].value.trim() || 'localhost');
    localStorage.setItem('ruvsense_port', els['connection-port-input'].value.trim() || DEFAULT_PORT);
    updateHostSummary();
    window.RuvSenseWS?.connect(hostForBaseUrl());
    setStatus(els['connection-status'], 'Connexion sauvegardée', 'ok');
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
      setStatus(els['connection-status'], `Connecté - version ${version}`, 'ok');
    } else {
      els['connection-state'].textContent = 'Hors ligne';
      setConnectionOnline(false);
      setStatus(els['connection-status'], 'Impossible de joindre le serveur', 'error');
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
      result.ok ? 'Alertes sauvegardées' : serverMessage(result),
      result.ok ? 'ok' : 'error',
    );
    button.disabled = false;
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
    if (showStatus) setStatus(els['display-status'], 'Affichage sauvegardé', 'ok');
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

  function bindRoomEditor() {
    const canvas = els['room-editor-canvas'];
    els['room-mode-draw-button'].addEventListener('click', () => setEditorMode('draw'));
    els['room-mode-nodes-button'].addEventListener('click', () => setEditorMode('nodes'));
    els['room-close-polygon-button'].addEventListener('click', closePolygon);
    els['room-load-button'].addEventListener('click', () => loadRoomConfig());
    els['room-reset-button'].addEventListener('click', resetRoomToDefault);
    els['room-save-button'].addEventListener('click', saveRoom);
    els['room-add-node-button'].addEventListener('click', addNode);
    els['room-remove-node-button'].addEventListener('click', removeNode);
    els['room-suggest-button'].addEventListener('click', suggestStarterPositions);
    els['room-grid-step-input'].addEventListener('input', () => {
      els['room-grid-step-input'].value = gridMeters();
    });

    canvas.addEventListener('pointerdown', (event) => {
      if (event.button !== 0) return;
      const point = canvasToMeter(event);
      if (editorMode === 'draw') {
        addPolygonPoint(point);
        return;
      }
      const node = hitNode(point);
      if (!node) return;
      dragNodeId = node.id;
      canvas.setPointerCapture(event.pointerId);
    });
    canvas.addEventListener('pointermove', (event) => {
      if (dragNodeId == null || editorMode !== 'nodes') return;
      const node = room.nodes.find((candidate) => candidate.id === dragNodeId);
      if (!node) return;
      const point = canvasToMeter(event);
      node.x = point.x;
      node.y = point.y;
      renderNodeList();
    });
    canvas.addEventListener('pointerup', (event) => {
      if (dragNodeId != null) {
        try { canvas.releasePointerCapture(event.pointerId); } catch {}
      }
      dragNodeId = null;
    });
    canvas.addEventListener('pointercancel', () => {
      dragNodeId = null;
    });
  }

  function bindEvents() {
    document.querySelectorAll('.settings-tab').forEach((button) => {
      button.addEventListener('click', () => activateTab(button.dataset.tab));
    });
    bindRoomEditor();
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
    window.RuvSenseWS?.onUpdate(() => updateLiveNodeCount());
  }

  async function init() {
    getElements();
    updateClock();
    window.setInterval(updateClock, 1000);
    loadConnection();
    loadAlerts();
    syncDisplayInputs();
    bindEvents();
    renderNodeList();
    window.RuvSenseWS?.connect(hostForBaseUrl());
    await loadRoomConfig();
    setStatus(els['connection-status'], '--', null);
    setStatus(els['alerts-status'], '--', null);
    setStatus(els['display-status'], '--', null);
    updateLiveNodeCount();
    animationFrame = requestAnimationFrame(drawEditor);
  }

  window.loadRoomConfig = loadRoomConfig;

  window.addEventListener('beforeunload', () => {
    if (animationFrame) cancelAnimationFrame(animationFrame);
  });

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', init, { once: true });
  } else {
    void init();
  }
})();

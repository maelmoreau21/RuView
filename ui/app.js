import { clearAlerts, getAlerts, initAlerts, processAlertState } from './alerts.js?v=module';

const state = {
  topology: null,
  modules: [],
  vitals: null,
  location: null,
  edgeVitals: null,
  calibration: null,
  pose: null,
  fleet: null,
  version: null,
  latest: null,
  ws: null,
  reconnectTimer: null,
  moduleFilter: '',
  moduleCategory: 'All',
  modulePresets: [],
  activePanel: 'fleet',
  logSeen: new Set(),
  placementDraft: new Map(),
  placementOriginal: new Map(),
  placementSelectedId: null,
  placementDirty: false,
  placementError: '',
  placementServerError: '',
  placementZoom: 1.0,
  placementPan: { x: 0, z: 0 },
  draggingNodeId: null,
  placementPanDrag: null,
  calibrationLastStatus: null,
  calibrationEvents: [],
  vitalsOptIn: localStorage.getItem('ruvsense:vitals-opt-in') === 'true',
  autoPlacementLastKey: '',
  autoPlacementStableCount: 0,
  autoPlacementSaving: false,
  placementAutoMode: localStorage.getItem('ruvsense:placement-mode') !== 'manual',
  leftActions: {
    recalibrate: { status: 'idle', message: 'Prêt' },
    reloadModules: { status: 'idle', message: 'Prêt' },
  },
  leftActionCurrent: 'recalibrate',
  roomConfig: null,
  roomConfigSource: 'pending',
  topologyAnimationId: null,
  personTrails: new Map(),
  vitalsTab: 'vitals',
  breathingHistory: new Map(),
  apneaZeroSince: new Map(),
};

const VALID_PANELS = new Set(['fleet', 'modules', 'vitals', 'calibration', 'diagnostics']);
const PLACEMENT_MIN_ZOOM = 0.25;
const PLACEMENT_MAX_ZOOM = 8;
const PLACEMENT_FIT_PADDING = 1.25;
const CALIBRATION_MIN_FRAMES_FALLBACK = 12000;
const AUTO_PLACEMENT_STABLE_REFRESHES = 2;
const ACTION_SPINNER_MS = 3000;
const ROOM_CONFIG_PATH = '/ui/room-config.json';
const DEFAULT_CANVAS_ROOM_WIDTH_M = 5.0;
const DEFAULT_CANVAS_ROOM_DEPTH_M = 4.0;
const DEFAULT_ROOM_CONFIG = {
  room_width_meters: DEFAULT_CANVAS_ROOM_WIDTH_M,
  room_height_meters: DEFAULT_CANVAS_ROOM_DEPTH_M,
  nodes: [
    { id: 1, x: 0, y: 0 },
    { id: 2, x: DEFAULT_CANVAS_ROOM_WIDTH_M, y: 0 },
    { id: 3, x: DEFAULT_CANVAS_ROOM_WIDTH_M, y: DEFAULT_CANVAS_ROOM_DEPTH_M },
    { id: 4, x: 0, y: DEFAULT_CANVAS_ROOM_DEPTH_M },
  ],
};
const BREATHING_TREND_MS = 10 * 60 * 1000;
const APNEA_ZERO_MS = 15 * 1000;
const $ = (selector) => document.querySelector(selector);
const $$ = (selector) => [...document.querySelectorAll(selector)];
initAlerts({ source: 'console' });

function text(value, fallback = '--') {
  if (value === null || value === undefined || value === '') return fallback;
  return String(value);
}

function fmtHz(value) {
  const n = Number(value);
  return Number.isFinite(n) ? `${n.toFixed(1)} Hz` : '0.0 Hz';
}

function fmtDbm(value) {
  const n = Number(value);
  return Number.isFinite(n) ? `${Math.round(n)} dBm` : '--';
}

function fmtAge(ms) {
  const n = Number(ms);
  if (!Number.isFinite(n) || n > 30 * 24 * 60 * 60 * 1000) return 'never';
  if (n < 1000) return `${Math.round(n)} ms`;
  if (n < 60000) return `${(n / 1000).toFixed(1)} s`;
  return `${Math.round(n / 60000)} min`;
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function nodeFrameAgeMs(node) {
  const value = Number(firstDefined(node?.last_csi_ms, node?.last_seen_ms));
  if (!Number.isFinite(value) || value > 30 * 24 * 60 * 60 * 1000) return null;
  return value;
}

function fmtFrameAge(ms) {
  const n = Number(ms);
  if (!Number.isFinite(n)) return '--';
  if (n < 1000) return `${Math.round(n)}ms`;
  if (n < 10000) return `${(n / 1000).toFixed(1)}s`;
  return `${Math.round(n / 1000)}s`;
}

function frameAgeTone(ms) {
  const n = Number(ms);
  if (!Number.isFinite(n)) return 'stale';
  if (n < 5000) return 'fresh';
  if (n <= 30000) return 'aging';
  return 'stale';
}

function rssiSegments(value) {
  const n = Number(value);
  if (!Number.isFinite(n)) return 0;
  if (n >= -50) return 5;
  if (n >= -60) return 4;
  if (n >= -70) return 3;
  if (n >= -80) return 2;
  return 1;
}

function fmtUptime(seconds) {
  const n = Number(seconds);
  if (!Number.isFinite(n) || n < 0) return '--';
  const days = Math.floor(n / 86400);
  const hours = Math.floor((n % 86400) / 3600);
  const minutes = Math.floor((n % 3600) / 60);
  if (days > 0) return `${days}j ${hours}h`;
  if (hours > 0) return `${hours}h ${minutes}m`;
  return `${minutes}m`;
}

function fmtInteger(value) {
  const n = Number(value);
  return Number.isFinite(n) ? Math.round(n).toLocaleString('fr-FR') : '--';
}

function fmtPct(value) {
  const n = Number(value);
  return Number.isFinite(n) ? `${Math.round(n * 100)}%` : '--';
}

function confidencePercentValue(...values) {
  for (const value of values) {
    const n = Number(value);
    if (!Number.isFinite(n)) continue;
    const normalized = n > 1 ? n / 100 : n;
    return Math.round(clamp(normalized, 0, 1) * 100);
  }
  return null;
}

function fmtConfidence(value) {
  return value == null ? '--' : `${value}%`;
}

function confidenceDots(value) {
  if (value == null) return '○○○○○';
  const filled = Math.max(0, Math.min(5, Math.round(value / 20)));
  return `${'●'.repeat(filled)}${'○'.repeat(5 - filled)}`;
}

function approximatePersonLabel(value) {
  const n = Math.max(0, Math.round(Number(value) || 0));
  if (n <= 0) return '0';
  return `~${n} ${n > 1 ? 'personnes' : 'personne'}`;
}

function activityLevelFromMotionEnergy(person) {
  const value = activityValue(person);
  if (value == null) return '--';
  if (value < 10) return 'LOW';
  if (value < 45) return 'MEDIUM';
  return 'HIGH';
}

function fmtMeters(value) {
  const n = Number(value);
  return Number.isFinite(n) ? `${n.toFixed(n >= 10 ? 1 : 2)} m` : '--';
}

function setText(id, value) {
  const node = document.getElementById(id);
  if (node) node.textContent = value;
}

function create(tag, className, value) {
  const el = document.createElement(tag);
  if (className) el.className = className;
  if (value !== undefined) el.textContent = value;
  return el;
}

function clear(node) {
  if (node) node.replaceChildren();
}

function emptyRow(label) {
  return create('div', 'empty-row', label);
}

function statusClass(value) {
  return String(value || 'offline').toLowerCase().replace(/[^a-z0-9_]+/g, '_');
}

function coverageOf(item) {
  const coverage = item?.coverage || {};
  const score = Number(coverage.score);
  return {
    score: Number.isFinite(score) ? clamp(score, 0, 1) : 0,
    quality: text(coverage.quality, score > 0 ? 'usable' : 'offline'),
    radiusM: Number(coverage.radius_m ?? coverage.radiusM ?? coverage.range_m ?? coverage.max_range_m),
    reasons: Array.isArray(coverage.reasons) ? coverage.reasons : [],
  };
}

function coverageColorClass(coverage) {
  return `coverage-${statusClass(coverage.quality)}`;
}

function logEvent(message, level = 'info') {
  const key = `${level}:${message}`;
  if (state.logSeen.has(key)) return;
  state.logSeen.add(key);
  const list = $('#event-log');
  if (!list) return;
  const item = create('li', level === 'info' ? '' : level);
  const stamp = create('time', '', new Date().toLocaleTimeString());
  const body = create('span', '', message);
  item.append(stamp, body);
  list.prepend(item);
  while (list.children.length > 90) list.lastElementChild?.remove();
}

async function fetchJson(path, options) {
  const response = await fetch(path, { cache: 'no-store', ...options });
  if (!response.ok) throw new Error(`${path} HTTP ${response.status}`);
  return response.json();
}

function positiveNumber(value, fallback) {
  const n = Number(value);
  return Number.isFinite(n) && n > 0 ? n : fallback;
}

function normalizeRoomConfig(raw, source) {
  const width = positiveNumber(
    raw?.room_width_meters ?? raw?.width_meters ?? raw?.width_m ?? raw?.width ?? raw?.dimensions_m?.[0],
    DEFAULT_CANVAS_ROOM_WIDTH_M,
  );
  const depth = positiveNumber(
    raw?.room_height_meters ?? raw?.room_depth_meters ?? raw?.depth_meters ?? raw?.height_meters ?? raw?.depth ?? raw?.dimensions_m?.[2] ?? raw?.dimensions_m?.[1],
    DEFAULT_CANVAS_ROOM_DEPTH_M,
  );
  const rawNodes = Array.isArray(raw?.nodes) ? raw.nodes : [];
  const nodes = rawNodes
    .map((node, index) => {
      const position = node?.position_m ?? node?.position ?? [];
      const id = Number(node?.id ?? node?.node_id ?? index + 1);
      const x = Number(node?.x ?? position?.[0] ?? position?.x);
      const y = Number(node?.y ?? node?.z ?? position?.[2] ?? position?.z ?? position?.[1]);
      return {
        id,
        x,
        y,
        label: text(node?.label, `N${id}`),
        rangeM: Number(node?.range_m ?? node?.radius_m ?? node?.coverage?.radius_m),
      };
    })
    .filter((node) => Number.isFinite(node.id) && Number.isFinite(node.x) && Number.isFinite(node.y));

  return { width, depth, nodes, source };
}

async function loadRoomConfig() {
  try {
    const config = await fetchJson(ROOM_CONFIG_PATH);
    state.roomConfig = normalizeRoomConfig(config, 'file');
    state.roomConfigSource = 'file';
  } catch (error) {
    state.roomConfig = normalizeRoomConfig(DEFAULT_ROOM_CONFIG, 'default');
    state.roomConfigSource = 'default';
    logEvent(`room-config.json absent, plan Canvas2D par dÃ©faut ${DEFAULT_CANVAS_ROOM_WIDTH_M}m x ${DEFAULT_CANVAS_ROOM_DEPTH_M}m`, 'warn');
  }
}

async function setModuleEnabled(id, enabled) {
  return fetchJson(`/api/v1/modules/${encodeURIComponent(id)}/enabled`, {
    method: 'PUT',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ enabled }),
  });
}

async function setEnabledModules(enabledModules) {
  return fetchJson('/api/v1/modules/enabled', {
    method: 'PUT',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ enabled_modules: enabledModules }),
  });
}

async function saveNodePositions(nodes) {
  return fetchJson('/api/v1/environment/node-positions', {
    method: 'PUT',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ nodes }),
  });
}

function topologyRoom() {
  return state.topology?.room || { name: 'primary', dimensions_m: [5.2, 2.6, 4.8] };
}

function roomDims() {
  const dims = topologyRoom().dimensions_m || [5.2, 2.6, 4.8];
  return [
    Number(dims[0]) || 5.2,
    Number(dims[1]) || 2.6,
    Number(dims[2]) || 4.8,
  ];
}

function positionOf(item) {
  const pos = item?.position ?? item?.position_m ?? {};
  if (Array.isArray(pos)) return { x: Number(pos[0]) || 0, y: Number(pos[1]) || 0, z: Number(pos[2]) || 0, source: 'configured', confidence: 0.9 };
  return {
    x: Number(pos.x) || 0,
    y: Number(pos.y) || 0,
    z: Number(pos.z) || 0,
    source: text(pos.source, item?.position_source || 'unknown'),
    confidence: Number(pos.confidence ?? item?.position_confidence ?? 0),
  };
}

function clamp(value, min, max) {
  return Math.max(min, Math.min(max, value));
}

function clonePosition(pos) {
  return {
    x: Number(pos?.x) || 0,
    y: Number(pos?.y) || 0,
    z: Number(pos?.z) || 0,
    source: text(pos?.source, 'manual'),
    confidence: Number(pos?.confidence ?? 0.95),
  };
}

function nodeIdOf(node) {
  return Number(node?.node_id);
}

function currentNodePosition(node) {
  const id = nodeIdOf(node);
  return state.placementDraft.get(id) || clonePosition(positionOf(node));
}

function roomBounds() {
  const [width, height, depth] = roomDims();
  return {
    minX: -width / 2,
    maxX: width / 2,
    minY: 0,
    maxY: height,
    minZ: -depth / 2,
    maxZ: depth / 2,
  };
}

function pointToStage(pos) {
  const [width, , depth] = roomDims();
  const zoom = state.placementZoom || 1;
  return {
    left: 50 + (((pos.x - state.placementPan.x) / width) * 100 * zoom),
    top: 50 - (((pos.z - state.placementPan.z) / depth) * 100 * zoom),
  };
}

function stagePointToWorld(stage, clientX, clientY) {
  const rect = stage.getBoundingClientRect();
  const [width, , depth] = roomDims();
  const zoom = Math.max(PLACEMENT_MIN_ZOOM, state.placementZoom || 1);
  const left = (clientX - rect.left) / Math.max(1, rect.width);
  const top = (clientY - rect.top) / Math.max(1, rect.height);
  return {
    x: state.placementPan.x + ((left - 0.5) * width / zoom),
    z: state.placementPan.z + ((0.5 - top) * depth / zoom),
  };
}

function stageToPosition(stage, clientX, clientY, current) {
  const point = stagePointToWorld(stage, clientX, clientY);
  return {
    ...clonePosition(current),
    x: point.x,
    z: point.z,
    source: 'manual',
    confidence: 1,
  };
}

function updatePlacementViewControls() {
  setText('placement-zoom-label', `${Math.round((state.placementZoom || 1) * 100)}%`);
  const zoomIn = $('#placement-zoom-in');
  const zoomOut = $('#placement-zoom-out');
  if (zoomIn) zoomIn.disabled = state.placementZoom >= PLACEMENT_MAX_ZOOM - 0.001;
  if (zoomOut) zoomOut.disabled = state.placementZoom <= PLACEMENT_MIN_ZOOM + 0.001;
}

function updateStageViewportStyle(stage) {
  const rect = stage.getBoundingClientRect();
  const [width, , depth] = roomDims();
  const zoom = Math.max(PLACEMENT_MIN_ZOOM, state.placementZoom || 1);
  const gridX = clamp((rect.width / Math.max(1, width)) * zoom, 8, 180);
  const gridY = clamp((rect.height / Math.max(1, depth)) * zoom, 8, 180);
  const originX = rect.width * (0.5 - ((state.placementPan.x / width) * zoom));
  const originY = rect.height * (0.5 + ((state.placementPan.z / depth) * zoom));
  stage.style.setProperty('--topology-grid-x', `${gridX}px`);
  stage.style.setProperty('--topology-grid-y', `${gridY}px`);
  stage.style.setProperty('--topology-grid-origin-x', `${originX}px`);
  stage.style.setProperty('--topology-grid-origin-y', `${originY}px`);
  updatePlacementViewControls();
}

function renderPlacementRoomFrame(stage) {
  const [width, , depth] = roomDims();
  const topLeft = pointToStage({ x: -width / 2, y: 0, z: depth / 2 });
  const frame = create('div', 'topology-room-frame');
  frame.style.left = `${topLeft.left}%`;
  frame.style.top = `${topLeft.top}%`;
  frame.style.width = `${100 * (state.placementZoom || 1)}%`;
  frame.style.height = `${100 * (state.placementZoom || 1)}%`;
  stage.append(frame);
}

function zoomPlacementAt(stage, clientX, clientY, factor) {
  const before = stagePointToWorld(stage, clientX, clientY);
  const nextZoom = clamp((state.placementZoom || 1) * factor, PLACEMENT_MIN_ZOOM, PLACEMENT_MAX_ZOOM);
  if (Math.abs(nextZoom - state.placementZoom) < 0.0001) return;
  state.placementZoom = nextZoom;
  const after = stagePointToWorld(stage, clientX, clientY);
  state.placementPan.x += before.x - after.x;
  state.placementPan.z += before.z - after.z;
  renderTopology();
}

function zoomPlacementBy(factor) {
  const stage = $('#topology-stage');
  if (!stage) return;
  const rect = stage.getBoundingClientRect();
  zoomPlacementAt(stage, rect.left + rect.width / 2, rect.top + rect.height / 2, factor);
}

function resetPlacementView() {
  state.placementZoom = 1.0;
  state.placementPan = { x: 0, z: 0 };
  renderTopology();
}

function fitPlacementViewToNodes() {
  const nodes = Array.isArray(state.topology?.nodes) ? state.topology.nodes : [];
  if (!nodes.length) {
    resetPlacementView();
    return;
  }

  const positions = nodes.map(currentNodePosition);
  const [roomWidth, , roomDepth] = roomDims();
  const xs = positions.map((pos) => pos.x);
  const zs = positions.map((pos) => pos.z);
  const minX = Math.min(...xs);
  const maxX = Math.max(...xs);
  const minZ = Math.min(...zs);
  const maxZ = Math.max(...zs);
  const spanX = Math.max(maxX - minX, roomWidth * 0.25);
  const spanZ = Math.max(maxZ - minZ, roomDepth * 0.25);

  state.placementPan = {
    x: (minX + maxX) / 2,
    z: (minZ + maxZ) / 2,
  };
  state.placementZoom = clamp(
    Math.min(roomWidth / (spanX * PLACEMENT_FIT_PADDING), roomDepth / (spanZ * PLACEMENT_FIT_PADDING)),
    PLACEMENT_MIN_ZOOM,
    PLACEMENT_MAX_ZOOM,
  );
  renderTopology();
}

function positionsEqual(a, b) {
  return ['x', 'y', 'z'].every((axis) => Math.abs((Number(a?.[axis]) || 0) - (Number(b?.[axis]) || 0)) < 0.005);
}

function changedPlacementNodes() {
  const nodes = Array.isArray(state.topology?.nodes) ? state.topology.nodes : [];
  const changed = [];
  for (const node of nodes) {
    const id = nodeIdOf(node);
    const draft = state.placementDraft.get(id);
    const original = state.placementOriginal.get(id) || clonePosition(positionOf(node));
    if (draft && !positionsEqual(draft, original)) {
      changed.push({ node, id, pos: draft });
    }
  }
  return changed;
}

function validatePosition(pos) {
  if (!pos || !['x', 'y', 'z'].every((axis) => Number.isFinite(Number(pos[axis])))) {
    return 'Coordonnées invalides.';
  }
  const b = roomBounds();
  if (pos.y < b.minY || pos.y > b.maxY) return `Y hauteur doit rester entre ${b.minY.toFixed(2)} m et ${b.maxY.toFixed(2)} m.`;
  return '';
}

function refreshPlacementDirty() {
  state.placementDirty = changedPlacementNodes().length > 0;
}

function setPlacementInputValues(pos) {
  const b = roomBounds();
  const manual = !state.placementAutoMode;
  const fields = [
    ['placement-x', 'x', null, null],
    ['placement-y', 'y', b.minY, b.maxY],
    ['placement-z', 'z', null, null],
  ];
  for (const [id, axis, min, max] of fields) {
    const input = document.getElementById(id);
    if (!(input instanceof HTMLInputElement)) continue;
    input.disabled = !pos || !manual;
    if (min === null || max === null) {
      input.removeAttribute('min');
      input.removeAttribute('max');
    } else {
      input.min = min.toFixed(2);
      input.max = max.toFixed(2);
    }
    input.step = '0.01';
    input.setAttribute('aria-invalid', state.placementError ? 'true' : 'false');
    if (!pos) {
      input.value = '';
    } else if (document.activeElement !== input) {
      input.value = Number(pos[axis]).toFixed(2);
    }
  }
}

function parsePlacementInput(id) {
  const input = document.getElementById(id);
  if (!(input instanceof HTMLInputElement)) return NaN;
  return Number(String(input.value).trim().replace(',', '.'));
}

function syncPlacementState(force = false) {
  const nodes = Array.isArray(state.topology?.nodes) ? state.topology.nodes : [];
  const liveIds = new Set(nodes.map(nodeIdOf));
  for (const node of nodes) {
    const id = nodeIdOf(node);
    const pos = clonePosition(positionOf(node));
    if (force || !state.placementDirty || !state.placementDraft.has(id)) {
      state.placementDraft.set(id, pos);
      state.placementOriginal.set(id, clonePosition(pos));
    }
  }
  for (const id of [...state.placementDraft.keys()]) {
    if (!liveIds.has(id)) {
      state.placementDraft.delete(id);
      state.placementOriginal.delete(id);
      if (state.placementSelectedId === id) state.placementSelectedId = null;
    }
  }
  refreshPlacementDirty();
}

function markPlacementDirty() {
  state.placementServerError = '';
  const nodes = Array.isArray(state.topology?.nodes) ? state.topology.nodes : [];
  const selected = nodes.find((node) => nodeIdOf(node) === state.placementSelectedId);
  state.placementError = selected ? validatePosition(currentNodePosition(selected)) : '';
  refreshPlacementDirty();
  renderPlacementControls();
}

function renderStatus() {
  const topology = state.topology;
  const readiness = topology?.readiness || {};
  const activeNodes = Number(readiness.active_nodes || topology?.nodes?.length || 0);
  const enabledModules = state.modules.filter((mod) => mod.enabled !== false).length;
  const ready = Boolean(readiness.ready);
  const fusion = text(readiness.fusion_mode, 'offline');
  const source = text(topology?.source, 'offline');
  const frameRate = Math.max(0, ...(topology?.nodes || []).map((node) => Number(node.frame_rate_hz) || 0));
  const roomConfig = canvasRoomConfig();

  const master = $('#master-status');
  if (master) {
    master.classList.toggle('is-ready', ready);
    master.classList.toggle('is-degraded', !ready && activeNodes > 0);
    master.querySelector('strong').textContent = ready ? 'prêt' : activeNodes > 0 ? 'limité' : 'hors ligne';
  }
  setText('active-nodes', `${activeNodes}/${Number(readiness.min_nodes || 1)}`);
  setText('enabled-modules', String(enabledModules));
  setText('fusion-mode', fusion);
  setText('source-mode', source);
  setText('scan-interface', text(topology?.wifi_scan?.interface, 'wlan0'));
  setText('room-size', `${roomConfig.width.toFixed(1)} x ${roomConfig.depth.toFixed(1)} m`);
  setText('readiness-label', ready ? 'prêt' : activeNodes > 0 ? 'limité' : 'pas prêt');
  setText('frame-rate', fmtHz(frameRate));
  setText('fleet-summary', `${activeNodes} live`);
  setText('vitals-optin-status', state.vitalsOptIn ? 'on' : 'off');
  const vitalsToggle = $('#vitals-optin');
  if (vitalsToggle instanceof HTMLInputElement) vitalsToggle.checked = state.vitalsOptIn;
}

function marker(kind, item, compact) {
  const pos = kind === 'node' ? currentNodePosition(item) : positionOf(item);
  const p = pointToStage(pos);
  const label = kind === 'ap'
    ? text(item.ssid || item.label || item.bssid, 'Hidden AP')
    : text(item.display_label || item.label || `C6-${item.node_id}`, `C6-${item.node_id}`);
  const detail = kind === 'ap'
    ? `${fmtDbm(item.rssi_dbm)} / ch ${text(item.channel)}`
    : `${pos.x.toFixed(2)} / ${pos.y.toFixed(2)} / z ${pos.z.toFixed(2)}`;
  const selected = kind === 'node' && nodeIdOf(item) === state.placementSelectedId;
  const el = create('div', `topology-marker ${kind} ${statusClass(item.health_status || item.status || pos.source)} ${statusClass(pos.source)}${compact ? ' compact' : ''}${selected ? ' is-selected' : ''}`);
  el.style.left = `${p.left}%`;
  el.style.top = `${p.top}%`;
  el.title = kind === 'node'
    ? `${label} / X ${pos.x.toFixed(2)} / Y ${pos.y.toFixed(2)} / Z ${pos.z.toFixed(2)}`
    : `${label} / ${pos.source} / confidence ${fmtPct(pos.confidence)}`;
  if (kind === 'node') el.dataset.nodeId = String(item.node_id);
  el.append(create('i'), create('strong', '', label), create('small', '', detail));
  return el;
}

function renderLink(stage, link, apsByBssid, nodesById) {
  const ap = apsByBssid.get(String(link.ap_bssid || link.ap_id || '').toLowerCase());
  const node = nodesById.get(Number(link.node_id));
  if (!ap || !node) return;
  const coverage = coverageOf(link);
  const a = pointToStage(positionOf(ap));
  const b = pointToStage(currentNodePosition(node));
  const dx = b.left - a.left;
  const dy = b.top - a.top;
  const line = create('div', `rf-link ${statusClass(link.source)} ${coverageColorClass(coverage)}`);
  line.style.left = `${a.left}%`;
  line.style.top = `${a.top}%`;
  line.style.width = `${Math.hypot(dx, dy)}%`;
  line.style.transform = `rotate(${Math.atan2(dy, dx)}rad)`;
  line.title = `confidence ${fmtPct(link.confidence)} / link ${coverage.quality} ${fmtPct(coverage.score)} / ${fmtMeters(link.distance_m)}`;
  stage.append(line);
}

function distanceBetween(a, b) {
  return Math.hypot(a.x - b.x, a.y - b.y, a.z - b.z);
}

function renderDistance(stage, aNode, bNode) {
  const aPos = currentNodePosition(aNode);
  const bPos = currentNodePosition(bNode);
  const a = pointToStage(aPos);
  const b = pointToStage(bPos);
  const dx = b.left - a.left;
  const dy = b.top - a.top;
  const line = create('div', 'node-distance-line');
  line.style.left = `${a.left}%`;
  line.style.top = `${a.top}%`;
  line.style.width = `${Math.hypot(dx, dy)}%`;
  line.style.transform = `rotate(${Math.atan2(dy, dx)}rad)`;
  const label = create('span', 'node-distance-label', `${distanceBetween(aPos, bPos).toFixed(2)} m`);
  label.style.left = `${(a.left + b.left) / 2}%`;
  label.style.top = `${(a.top + b.top) / 2}%`;
  stage.append(line, label);
}

function renderNodeDistances(stage, nodes) {
  if (nodes.length < 2) return;
  const selectedId = state.placementSelectedId;
  if (selectedId && nodes.length > 8) {
    const selected = nodes.find((node) => nodeIdOf(node) === selectedId);
    if (!selected) return;
    for (const node of nodes) {
      if (nodeIdOf(node) !== selectedId) renderDistance(stage, selected, node);
    }
    return;
  }
  if (nodes.length > 8) return;
  for (let i = 0; i < nodes.length; i += 1) {
    for (let j = i + 1; j < nodes.length; j += 1) {
      renderDistance(stage, nodes[i], nodes[j]);
    }
  }
}

function radiusToStageSize(radiusM) {
  const [width, , depth] = roomDims();
  const zoom = state.placementZoom || 1;
  const r = Number(radiusM);
  if (!Number.isFinite(r) || r <= 0) return { width: 18, height: 18 };
  return {
    width: clamp((r / Math.max(width, 0.1)) * 100 * 2 * zoom, 6, 260),
    height: clamp((r / Math.max(depth, 0.1)) * 100 * 2 * zoom, 6, 260),
  };
}

function renderCoverageLayers(stage, nodes) {
  let usable = 0;
  let weak = 0;
  for (const node of nodes) {
    const coverage = coverageOf(node);
    if (coverage.score >= 0.46) usable += 1;
    if (coverage.score > 0 && coverage.score < 0.46) weak += 1;
    const pos = currentNodePosition(node);
    const p = pointToStage(pos);
    const size = radiusToStageSize(coverage.radiusM || Math.max(roomDims()[0], roomDims()[2]) * (0.25 + coverage.score));
    const zone = create('div', `coverage-zone ${coverageColorClass(coverage)}`);
    zone.style.left = `${p.left}%`;
    zone.style.top = `${p.top}%`;
    zone.style.width = `${size.width}%`;
    zone.style.height = `${size.height}%`;
    zone.title = `${text(node.display_label || node.label, `node ${node.node_id}`)} coverage ${fmtPct(coverage.score)} ${coverage.reasons.join(', ')}`;
    stage.append(zone);
  }
  setText('coverage-summary', nodes.length ? `${usable}/${nodes.length} coverage usable` : 'coverage --');
  setText('dead-zone-summary', weak ? `${weak} weak link(s)` : nodes.length ? 'dead zones none obvious' : 'dead zones --');
}

function vectorPosition(value) {
  if (Array.isArray(value) && value.length >= 3) {
    return { x: Number(value[0]) || 0, y: Number(value[1]) || 0, z: Number(value[2]) || 0 };
  }
  if (value && typeof value === 'object') {
    return { x: Number(value.x) || 0, y: Number(value.y) || 0, z: Number(value.z) || 0 };
  }
  return null;
}

function locationPerson() {
  const persons = Array.isArray(state.location?.persons) ? state.location.persons : [];
  const source = persons.length ? persons[0] : state.vitals?.location;
  if (!source || typeof source !== 'object') return null;
  const x = Number(source.x);
  const y = Number(source.y);
  if (!Number.isFinite(x) || !Number.isFinite(y)) return null;
  return {
    x,
    y,
    confidence: Number(source.confidence ?? 0),
    timestamp_ms: Number(source.timestamp_ms ?? state.location?.timestamp_ms ?? 0),
  };
}

function locationNodePositions() {
  const locationNodes = Array.isArray(state.location?.nodes) ? state.location.nodes : [];
  if (locationNodes.length) {
    return locationNodes
      .map((node) => ({
        node_id: Number(node.node_id),
        x: Number(node.x),
        y: Number(node.y),
      }))
      .filter((node) => Number.isFinite(node.node_id) && Number.isFinite(node.x) && Number.isFinite(node.y));
  }

  const topologyNodes = Array.isArray(state.topology?.nodes) ? state.topology.nodes : [];
  return topologyNodes
    .map((node) => {
      const pos = currentNodePosition(node);
      return {
        node_id: nodeIdOf(node),
        x: Number(pos.x),
        y: Number(pos.z),
      };
    })
    .filter((node) => Number.isFinite(node.node_id) && Number.isFinite(node.x) && Number.isFinite(node.y));
}

function locationBounds(points) {
  const [width, , depth] = roomDims();
  const hasNegative = points.some((point) => point.x < -0.001 || point.y < -0.001);
  return hasNegative
    ? { minX: -width / 2, maxX: width / 2, minY: -depth / 2, maxY: depth / 2 }
    : { minX: 0, maxX: width, minY: 0, maxY: depth };
}

function projectLocationPoint(point, bounds, rect) {
  const spanX = Math.max(0.001, bounds.maxX - bounds.minX);
  const spanY = Math.max(0.001, bounds.maxY - bounds.minY);
  return {
    x: rect.x + clamp((point.x - bounds.minX) / spanX, 0, 1) * rect.width,
    y: rect.y + rect.height - clamp((point.y - bounds.minY) / spanY, 0, 1) * rect.height,
  };
}

function svgEl(tag, attrs = {}) {
  const el = document.createElementNS('http://www.w3.org/2000/svg', tag);
  for (const [key, value] of Object.entries(attrs)) {
    if (value !== undefined && value !== null) el.setAttribute(key, String(value));
  }
  return el;
}

function renderLocationPlan() {
  const svg = $('#location-plan');
  if (!svg) return;
  svg.replaceChildren();
  svg.setAttribute('viewBox', '0 0 360 220');
  const rect = { x: 22, y: 20, width: 316, height: 170 };
  const nodes = locationNodePositions();
  const person = locationPerson();
  const points = [...nodes, ...(person ? [person] : [])];

  svg.append(svgEl('rect', {
    class: 'location-room',
    x: rect.x,
    y: rect.y,
    width: rect.width,
    height: rect.height,
    rx: 6,
  }));

  if (!points.length) {
    const empty = svgEl('text', {
      x: rect.x + rect.width / 2,
      y: rect.y + rect.height / 2,
      fill: 'currentColor',
      'text-anchor': 'middle',
      class: 'location-node-label',
    });
    empty.textContent = 'Aucune position RSSI';
    svg.append(empty);
    setText('location-plan-summary', 'position --');
    return;
  }

  const bounds = locationBounds(points);
  for (const node of nodes) {
    const point = projectLocationPoint(node, bounds, rect);
    const label = svgEl('text', {
      class: 'location-node-label',
      x: point.x + 10,
      y: point.y - 8,
    });
    label.textContent = `N${node.node_id}`;
    svg.append(
      svgEl('circle', {
        class: 'location-node',
        cx: point.x,
        cy: point.y,
        r: 6,
      }),
      label,
    );
  }

  if (person) {
    const point = projectLocationPoint(person, bounds, rect);
    const radius = clamp((1 - clamp(person.confidence, 0, 1)) * 34 + 14, 14, 48);
    const label = svgEl('text', {
      class: 'location-person-label',
      x: point.x + 12,
      y: point.y + 18,
    });
    label.textContent = `P ${fmtPct(person.confidence)}`;
    svg.append(
      svgEl('circle', {
        class: 'location-person-ring',
        cx: point.x,
        cy: point.y,
        r: radius,
      }),
      svgEl('line', {
        class: 'location-person-cross',
        x1: point.x - 8,
        y1: point.y - 8,
        x2: point.x + 8,
        y2: point.y + 8,
      }),
      svgEl('line', {
        class: 'location-person-cross',
        x1: point.x + 8,
        y1: point.y - 8,
        x2: point.x - 8,
        y2: point.y + 8,
      }),
      label,
    );
    setText('location-plan-summary', `x ${person.x.toFixed(2)} / y ${person.y.toFixed(2)} / ${fmtPct(person.confidence)}`);
  } else {
    setText('location-plan-summary', `${nodes.length} noeud(s), position --`);
  }
}

function primaryPerson() {
  const persons = Array.isArray(state.latest?.persons) ? state.latest.persons : [];
  if (persons.length) return persons[0];
  const posePersons = Array.isArray(state.pose?.persons) ? state.pose.persons : [];
  if (posePersons.length) return posePersons[0];
  if (state.pose?.person) return state.pose.person;
  const loc = locationPerson();
  if (loc) {
    return {
      id: 'rssi-location',
      confidence: loc.confidence,
      position_m: [loc.x, 0, loc.y],
      position_source: 'rssi_localization',
    };
  }
  const count = Number(state.latest?.count_evidence?.rendered_persons ?? state.latest?.estimated_persons ?? 0);
  if (count > 0) {
    return {
      id: 'inferred',
      confidence: state.latest?.classification?.confidence ?? 0.35,
      position_source: 'count_evidence',
    };
  }
  return null;
}

function renderPersonEstimate(stage) {
  const person = primaryPerson();
  if (!person) {
    setText('person-position-summary', 'position none');
    return;
  }
  const rawPos = vectorPosition(person.position_m) || vectorPosition(person.position) || { x: 0, y: 0, z: 0 };
  const p = pointToStage(rawPos);
  const confidence = Number(person.confidence ?? state.latest?.classification?.confidence ?? 0.35);
  const source = text(person.position_source, 'estimated');
  const marker = create('div', `person-estimate ${source === 'count_evidence' || source === 'observatory_layout' ? 'is-uncertain' : ''}`);
  marker.style.left = `${p.left}%`;
  marker.style.top = `${p.top}%`;
  marker.title = `person ${source} confidence ${fmtPct(confidence)}`;
  marker.append(create('i'), create('span', '', 'Zone'));
  const uncertainty = create('div', 'person-uncertainty');
  const radius = clamp((1 - (Number.isFinite(confidence) ? confidence : 0.35)) * 36 + 16, 18, 56);
  uncertainty.style.left = `${p.left}%`;
  uncertainty.style.top = `${p.top}%`;
  uncertainty.style.width = `${radius}px`;
  uncertainty.style.height = `${radius}px`;
  stage.append(uncertainty, marker);
  setText('person-position-summary', `${source} ${fmtPct(confidence)}`);
}

function canvasRoomConfig() {
  return state.roomConfig || normalizeRoomConfig(DEFAULT_ROOM_CONFIG, 'default');
}

function liveNodeMap() {
  const nodes = Array.isArray(state.topology?.nodes) ? state.topology.nodes : [];
  return new Map(nodes.map((node) => [nodeIdOf(node), node]));
}

function isNodeActive(node) {
  if (!node || node.active === false) return false;
  const status = statusClass(node.health_status || node.status || node.sync_status || 'live');
  return !['offline', 'stale', 'disabled', 'unavailable', 'error'].includes(status);
}

function canvasNodes() {
  const config = canvasRoomConfig();
  const live = liveNodeMap();
  const fallbackRange = Math.max(config.width, config.depth) * 0.22;
  return config.nodes.map((node) => {
    const liveNode = live.get(Number(node.id));
    const coverage = coverageOf(liveNode || node);
    const coverageRange = Number(coverage.radiusM);
    const configRange = Number(node.rangeM);
    return {
      ...node,
      liveNode,
      active: isNodeActive(liveNode),
      rangeM: Number.isFinite(coverageRange) && coverageRange > 0
        ? coverageRange
        : Number.isFinite(configRange) && configRange > 0
          ? configRange
          : fallbackRange,
    };
  });
}

function vectorPlanPoint(value) {
  if (Array.isArray(value) && value.length >= 3) {
    return { x: Number(value[0]), y: Number(value[2]) };
  }
  if (Array.isArray(value) && value.length >= 2) {
    return { x: Number(value[0]), y: Number(value[1]) };
  }
  if (value && typeof value === 'object') {
    return { x: Number(value.x), y: Number(value.z ?? value.y) };
  }
  return null;
}

function normalizePlanPoint(point, config) {
  if (!point || !Number.isFinite(point.x) || !Number.isFinite(point.y)) return null;
  let x = point.x;
  let y = point.y;
  const centeredX = x >= -config.width / 2 - 0.5 && x <= config.width / 2 + 0.5;
  const centeredY = y >= -config.depth / 2 - 0.5 && y <= config.depth / 2 + 0.5;
  if ((x < -0.001 || y < -0.001) && centeredX && centeredY) {
    x += config.width / 2;
    y += config.depth / 2;
  }
  return { x, y };
}

function pushCanvasPersons(target, source, sourceName, config) {
  if (!Array.isArray(source)) return;
  for (const [index, person] of source.entries()) {
    const rawPoint =
      vectorPlanPoint(person?.position_m) ||
      vectorPlanPoint(person?.position) ||
      vectorPlanPoint(person);
    const point = normalizePlanPoint(rawPoint, config);
    if (!point) continue;
    const id = text(person?.id ?? person?.person_id ?? person?.track_id, `${sourceName}-${index + 1}`);
    target.push({
      id,
      sourceName,
      x: point.x,
      y: point.y,
      confidence: clamp(Number(person?.confidence ?? person?.score ?? person?.probability ?? 0), 0, 1),
    });
  }
}

function collectCanvasPersons() {
  const config = canvasRoomConfig();
  const persons = [];
  if (!String(state.latest?.source || '').toLowerCase().includes('sim')) {
    pushCanvasPersons(persons, state.latest?.persons, 'latest', config);
  }
  pushCanvasPersons(persons, state.pose?.persons, 'pose', config);
  if (state.pose?.person) pushCanvasPersons(persons, [state.pose.person], 'pose', config);
  pushCanvasPersons(persons, state.location?.persons, 'location', config);

  const seen = new Set();
  return persons.filter((person) => {
    const key = person.id;
    if (seen.has(key)) return false;
    seen.add(key);
    return true;
  });
}

function detectedPersonCount(positionedPersons) {
  const liveCount = Number(firstDefined(
    state.latest?.count_evidence?.rendered_persons,
    state.latest?.estimated_persons,
    state.latest?.n_persons,
    Array.isArray(state.location?.persons) ? state.location.persons.length : undefined,
  ));
  return Math.max(0, Math.round(Number.isFinite(liveCount) ? liveCount : positionedPersons.length));
}

function syncCanvasPersonTrails(persons) {
  const liveIds = new Set(persons.map((person) => person.id));
  for (const person of persons) {
    const trail = state.personTrails.get(person.id) || [];
    const last = trail[trail.length - 1];
    if (!last || Math.hypot(last.x - person.x, last.y - person.y) > 0.03) {
      trail.push({ x: person.x, y: person.y });
    }
    state.personTrails.set(person.id, trail.slice(-5));
  }
  for (const id of [...state.personTrails.keys()]) {
    if (!liveIds.has(id)) state.personTrails.delete(id);
  }
}

function canvasGeometry(width, height, config) {
  const padding = { top: 28, right: 28, bottom: 52, left: 38 };
  const availableWidth = Math.max(1, width - padding.left - padding.right);
  const availableHeight = Math.max(1, height - padding.top - padding.bottom);
  const scale = Math.min(availableWidth / config.width, availableHeight / config.depth);
  const roomWidth = config.width * scale;
  const roomHeight = config.depth * scale;
  return {
    scale,
    roomX: padding.left + (availableWidth - roomWidth) / 2,
    roomY: padding.top + (availableHeight - roomHeight) / 2,
    roomWidth,
    roomHeight,
    config,
  };
}

function planToCanvas(point, geometry) {
  return {
    x: geometry.roomX + clamp(point.x, 0, geometry.config.width) * geometry.scale,
    y: geometry.roomY + geometry.roomHeight - clamp(point.y, 0, geometry.config.depth) * geometry.scale,
  };
}

function canvasToPlanPoint(stage, clientX, clientY) {
  const rect = stage.getBoundingClientRect();
  const geometry = canvasGeometry(rect.width, rect.height, canvasRoomConfig());
  return {
    x: (clientX - rect.left - geometry.roomX) / geometry.scale,
    y: (geometry.roomY + geometry.roomHeight - (clientY - rect.top)) / geometry.scale,
  };
}

function pathRoundRect(ctx, x, y, width, height, radius) {
  const r = Math.min(radius, width / 2, height / 2);
  ctx.beginPath();
  ctx.moveTo(x + r, y);
  ctx.lineTo(x + width - r, y);
  ctx.quadraticCurveTo(x + width, y, x + width, y + r);
  ctx.lineTo(x + width, y + height - r);
  ctx.quadraticCurveTo(x + width, y + height, x + width - r, y + height);
  ctx.lineTo(x + r, y + height);
  ctx.quadraticCurveTo(x, y + height, x, y + height - r);
  ctx.lineTo(x, y + r);
  ctx.quadraticCurveTo(x, y, x + r, y);
  ctx.closePath();
}

function confidenceColor(confidence) {
  if (confidence > 0.7) return '#22c55e';
  if (confidence > 0.4) return '#f59e0b';
  return '#ef4444';
}

function hexToRgba(hex, alpha) {
  const value = String(hex).replace('#', '');
  const r = parseInt(value.slice(0, 2), 16);
  const g = parseInt(value.slice(2, 4), 16);
  const b = parseInt(value.slice(4, 6), 16);
  return `rgba(${r}, ${g}, ${b}, ${alpha})`;
}

function drawCanvasGrid(ctx, geometry) {
  const { config } = geometry;
  ctx.save();
  ctx.strokeStyle = '#1f2937';
  ctx.lineWidth = 1;
  for (let x = 0; x <= config.width + 0.0001; x += 1) {
    const p0 = planToCanvas({ x, y: 0 }, geometry);
    const p1 = planToCanvas({ x, y: config.depth }, geometry);
    ctx.beginPath();
    ctx.moveTo(p0.x, p0.y);
    ctx.lineTo(p1.x, p1.y);
    ctx.stroke();
  }
  for (let y = 0; y <= config.depth + 0.0001; y += 1) {
    const p0 = planToCanvas({ x: 0, y }, geometry);
    const p1 = planToCanvas({ x: config.width, y }, geometry);
    ctx.beginPath();
    ctx.moveTo(p0.x, p0.y);
    ctx.lineTo(p1.x, p1.y);
    ctx.stroke();
  }
  ctx.strokeStyle = '#374151';
  ctx.lineWidth = 2;
  ctx.strokeRect(geometry.roomX, geometry.roomY, geometry.roomWidth, geometry.roomHeight);
  ctx.restore();
}

function drawCanvasNodes(ctx, geometry, nodes) {
  ctx.save();
  for (const node of nodes) {
    const p = planToCanvas(node, geometry);
    if (node.active) {
      ctx.beginPath();
      ctx.fillStyle = 'rgba(59, 130, 246, 0.13)';
      ctx.strokeStyle = 'rgba(59, 130, 246, 0.32)';
      ctx.lineWidth = 1;
      ctx.arc(p.x, p.y, node.rangeM * geometry.scale, 0, Math.PI * 2);
      ctx.fill();
      ctx.stroke();
    }
  }
  for (const node of nodes) {
    const p = planToCanvas(node, geometry);
    const size = 26;
    pathRoundRect(ctx, p.x - size / 2, p.y - size / 2, size, size, 5);
    ctx.fillStyle = node.active ? '#3b82f6' : '#374151';
    ctx.fill();
    ctx.strokeStyle = node.active ? 'rgba(255, 255, 255, 0.72)' : 'rgba(148, 163, 184, 0.45)';
    ctx.lineWidth = 1;
    ctx.stroke();
    ctx.fillStyle = '#ffffff';
    ctx.font = '800 11px Inter, system-ui, sans-serif';
    ctx.textAlign = 'center';
    ctx.textBaseline = 'middle';
    ctx.fillText(`N${node.id}`, p.x, p.y + 0.5);
  }
  ctx.restore();
}

function drawCanvasPersons(ctx, geometry, persons, time) {
  ctx.save();
  for (const person of persons) {
    const trail = state.personTrails.get(person.id) || [];
    trail.forEach((point, index) => {
      const p = planToCanvas(point, geometry);
      const alpha = (index + 1) / Math.max(1, trail.length);
      ctx.beginPath();
      ctx.fillStyle = `rgba(148, 163, 184, ${0.08 + alpha * 0.18})`;
      ctx.arc(p.x, p.y, 5 + alpha * 4, 0, Math.PI * 2);
      ctx.fill();
    });
  }
  persons.forEach((person, index) => {
    const p = planToCanvas(person, geometry);
    const color = confidenceColor(person.confidence);
    const pulse = 0.5 + 0.5 * Math.sin(time / 320 + index);
    ctx.beginPath();
    ctx.fillStyle = hexToRgba(color, 0.10 + pulse * 0.12);
    ctx.arc(p.x, p.y, 28 + pulse * 8, 0, Math.PI * 2);
    ctx.fill();
    ctx.beginPath();
    ctx.fillStyle = color;
    ctx.arc(p.x, p.y, 20, 0, Math.PI * 2);
    ctx.fill();
    ctx.strokeStyle = 'rgba(255, 255, 255, 0.82)';
    ctx.lineWidth = 2;
    ctx.stroke();
    ctx.fillStyle = '#ffffff';
    ctx.font = '800 12px Inter, system-ui, sans-serif';
    ctx.textAlign = 'center';
    ctx.textBaseline = 'middle';
    ctx.fillText(`P${index + 1}`, p.x, p.y + 0.5);
  });
  ctx.restore();
}

function drawCanvasFooter(ctx, width, height, geometry, persons) {
  const detected = detectedPersonCount(persons);
  const scaleMeters = geometry.config.width >= 6 ? 2 : 1;
  const scaleWidth = scaleMeters * geometry.scale;
  const scaleX = geometry.roomX;
  const scaleY = height - 25;
  ctx.save();
  ctx.fillStyle = '#e5e7eb';
  ctx.strokeStyle = '#e5e7eb';
  ctx.lineWidth = 2;
  ctx.beginPath();
  ctx.moveTo(scaleX, scaleY);
  ctx.lineTo(scaleX + scaleWidth, scaleY);
  ctx.moveTo(scaleX, scaleY - 5);
  ctx.lineTo(scaleX, scaleY + 5);
  ctx.moveTo(scaleX + scaleWidth, scaleY - 5);
  ctx.lineTo(scaleX + scaleWidth, scaleY + 5);
  ctx.stroke();
  ctx.font = '700 11px Inter, system-ui, sans-serif';
  ctx.textAlign = 'center';
  ctx.textBaseline = 'bottom';
  ctx.fillText(`${scaleMeters} m`, scaleX + scaleWidth / 2, scaleY - 7);
  ctx.textAlign = 'left';
  ctx.textBaseline = 'alphabetic';
  ctx.fillText(`Personnes detectees: ${approximatePersonLabel(detected)}`, 14, height - 12);
  ctx.textAlign = 'right';
  ctx.fillText(`${geometry.config.width.toFixed(1)} m x ${geometry.config.depth.toFixed(1)} m`, width - 14, height - 12);
  ctx.restore();
}

function drawTopologyCanvas(canvas, time = performance.now()) {
  const parent = canvas.parentElement;
  if (!parent) return;
  const rect = parent.getBoundingClientRect();
  const width = Math.max(1, Math.floor(rect.width));
  const height = Math.max(1, Math.floor(rect.height));
  const dpr = Math.max(1, Math.min(window.devicePixelRatio || 1, 2));
  if (canvas.width !== Math.floor(width * dpr) || canvas.height !== Math.floor(height * dpr)) {
    canvas.width = Math.floor(width * dpr);
    canvas.height = Math.floor(height * dpr);
  }
  canvas.style.width = `${width}px`;
  canvas.style.height = `${height}px`;
  const ctx = canvas.getContext('2d');
  if (!ctx) return;
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, width, height);
  ctx.fillStyle = '#0d1117';
  ctx.fillRect(0, 0, width, height);

  const config = canvasRoomConfig();
  const geometry = canvasGeometry(width, height, config);
  const persons = collectCanvasPersons();
  drawCanvasGrid(ctx, geometry);
  drawCanvasPersons(ctx, geometry, persons, time);
  drawCanvasNodes(ctx, geometry, canvasNodes());
  drawCanvasFooter(ctx, width, height, geometry, persons);
}

function ensureTopologyCanvas(stage) {
  let canvas = stage.querySelector('canvas.topology-canvas');
  if (!canvas) {
    canvas = document.createElement('canvas');
    canvas.className = 'topology-canvas';
    canvas.setAttribute('role', 'img');
    canvas.setAttribute('aria-label', 'Plan Canvas2D de la piece, noeuds et personnes detectees');
    stage.replaceChildren(canvas);
  } else {
    for (const child of [...stage.children]) {
      if (child !== canvas) child.remove();
    }
  }
  return canvas;
}

function ensureTopologyAnimation() {
  if (state.topologyAnimationId) return;
  const tick = (time) => {
    const canvas = $('#topology-stage canvas.topology-canvas');
    if (canvas instanceof HTMLCanvasElement) drawTopologyCanvas(canvas, time);
    state.topologyAnimationId = requestAnimationFrame(tick);
  };
  state.topologyAnimationId = requestAnimationFrame(tick);
}

function nodeIdAtCanvasPoint(event) {
  const stage = $('#topology-stage');
  if (!stage) return null;
  const point = canvasToPlanPoint(stage, event.clientX, event.clientY);
  let best = null;
  let bestDistance = Infinity;
  for (const node of canvasNodes()) {
    if (!node.liveNode) continue;
    const distance = Math.hypot(node.x - point.x, node.y - point.y);
    if (distance < bestDistance) {
      best = node;
      bestDistance = distance;
    }
  }
  const hitRadiusM = 18 / Math.max(1, canvasGeometry(stage.clientWidth, stage.clientHeight, canvasRoomConfig()).scale);
  return best && bestDistance <= hitRadiusM ? Number(best.id) : null;
}

function renderTopology() {
  const stage = $('#topology-stage');
  if (!stage) return;
  syncPlacementState();
  stage.classList.toggle('is-auto-placement', state.placementAutoMode);
  updatePlacementViewControls();

  const canvas = ensureTopologyCanvas(stage);
  const nodes = canvasNodes();
  const activeNodes = nodes.filter((node) => node.active).length;
  const persons = collectCanvasPersons();
  syncCanvasPersonTrails(persons);
  drawTopologyCanvas(canvas);
  ensureTopologyAnimation();

  setText('coverage-summary', nodes.length ? `${activeNodes}/${nodes.length} noeud(s) actifs` : '0 noeud room-config');
  setText('dead-zone-summary', state.roomConfigSource === 'file' ? 'room-config ok' : 'room-config defaut');
  setText('person-position-summary', persons.length ? `${persons.length} position(s)` : 'position none');
}

function renderPlacementControls() {
  const nodes = Array.isArray(state.topology?.nodes) ? state.topology.nodes : [];
  const selected = nodes.find((node) => nodeIdOf(node) === state.placementSelectedId);
  const selectedLabel = selected
    ? text(selected.display_label || selected.label, `ESP32-C6 #${selected.node_id}`)
    : 'Aucun nœud sélectionné';
  const pos = selected ? currentNodePosition(selected) : null;
  const activeInputInvalid = ['placement-x', 'placement-y', 'placement-z'].some((id) => {
    const input = document.getElementById(id);
    return document.activeElement === input && !Number.isFinite(parsePlacementInput(id));
  });
  const validationError = pos
    ? activeInputInvalid ? state.placementError || 'Coordonnées invalides.' : validatePosition(pos)
    : '';
  state.placementError = state.placementServerError || validationError;
  refreshPlacementDirty();
  setText('placement-selected', selectedLabel);
  setText('placement-coordinates', pos
    ? `X ${pos.x.toFixed(2)} m / Y hauteur ${pos.y.toFixed(2)} m / Z ${pos.z.toFixed(2)} m`
    : 'X -- / Y -- / Z --');
  setText('placement-state', state.placementError ? 'erreur' : state.placementDirty ? 'à enregistrer' : 'enregistré');
  setText('placement-error', state.placementError || '');
  setText('placement-mode-label', state.placementAutoMode ? 'auto' : 'manuel');
  const modeToggle = $('#placement-auto-mode');
  if (modeToggle instanceof HTMLInputElement) modeToggle.checked = state.placementAutoMode;
  const error = $('#placement-error');
  if (error) error.classList.toggle('is-visible', Boolean(state.placementError));
  setPlacementInputValues(pos);
  const save = $('#save-placement');
  if (save) save.disabled = state.placementAutoMode || !state.placementDirty || Boolean(state.placementError) || !nodes.length;
  const reset = $('#reset-placement');
  if (reset) reset.disabled = state.placementAutoMode || !state.placementDirty;
  const autoNow = $('#auto-place-now');
  if (autoNow) autoNow.disabled = !state.placementAutoMode;
}

function denseRow(title, subtitle, meta, status) {
  const row = create('article', 'dense-row');
  const main = create('div');
  main.append(create('strong', '', title), create('small', '', subtitle));
  const side = create('div', 'dense-meta');
  if (status) side.append(create('span', `state-pill state-${statusClass(status)}`, status));
  for (const item of meta) side.append(create('span', '', item));
  row.append(main, side);
  return row;
}

function createRssiMeter(rssi) {
  const active = rssiSegments(rssi);
  const meter = create('div', 'rssi-meter');
  meter.setAttribute('aria-label', `RSSI ${active}/5`);
  for (let index = 0; index < 5; index += 1) {
    const segment = create('span', index < active ? 'is-active' : '');
    meter.append(segment);
  }
  return meter;
}

function nodeCard(node) {
  const ageMs = nodeFrameAgeMs(node);
  const tone = frameAgeTone(ageMs);
  const card = create('article', `node-card card is-${tone}`);
  const name = text(node.display_label || node.label, `ESP32-C6 #${node.node_id}`);
  const rssi = Number(node.rssi_dbm);
  const header = create('div', 'node-card-header');
  const title = create('div', 'node-card-title');
  title.append(
    create('span', `node-freshness-dot is-${tone}`),
    create('strong', '', name)
  );
  header.append(
    title,
    create('span', 'node-rssi-value', Number.isFinite(rssi) ? `${Math.round(rssi)} dBm` : '--')
  );

  const meta = create('div', 'node-card-meta');
  meta.append(
    create('span', '', `Dernière trame ${fmtFrameAge(ageMs)}`),
    create('span', '', `${rssiSegments(rssi)}/5`)
  );

  card.append(header, meta, createRssiMeter(rssi));
  return card;
}

function versionText() {
  const payload = state.version;
  if (typeof payload === 'string') return text(payload);
  return text(payload?.version);
}

function systemModeInfo() {
  const online = Boolean(state.latest || state.fleet?.active_nodes > 0);
  return online
    ? { label: 'MODE LIVE', className: 'is-live' }
    : { label: 'HORS LIGNE', className: 'is-empty' };
}

function renderSystemPanel() {
  setText('system-version', versionText());
  setText('system-uptime', fmtUptime(state.fleet?.uptime_seconds));
  setText('system-frame-count', fmtInteger(firstDefined(
    state.fleet?.frame_count,
    state.fleet?.frames_processed,
    state.fleet?.tick,
    state.latest?.tick
  )));
  const badge = $('#system-mode-badge');
  if (badge) {
    const mode = systemModeInfo();
    badge.textContent = mode.label;
    badge.className = `system-mode-badge ${mode.className}`;
  }
}

function currentLeftAction() {
  return state.leftActions[state.leftActionCurrent] || state.leftActions.recalibrate;
}

function renderLeftActions() {
  const recalibrate = state.leftActions.recalibrate;
  const reloadModules = state.leftActions.reloadModules;
  const recalibrateButton = $('#left-recalibrate');
  const reloadButton = $('#left-reload-modules');
  if (recalibrateButton) {
    recalibrateButton.disabled = recalibrate.status === 'loading';
    recalibrateButton.classList.toggle('is-loading', recalibrate.status === 'loading');
  }
  if (reloadButton) {
    reloadButton.disabled = reloadModules.status === 'loading';
    reloadButton.classList.toggle('is-loading', reloadModules.status === 'loading');
  }
  const current = currentLeftAction();
  const status = $('#left-action-status');
  if (status) {
    status.textContent = current.message || 'Prêt';
    status.className = `left-action-status is-${statusClass(current.status || 'idle')}`;
  }
}

function renderFleet() {
  const nodes = Array.isArray(state.topology?.nodes) ? state.topology.nodes : [];
  const aps = Array.isArray(state.topology?.access_points) ? state.topology.access_points : [];
  const nodeList = $('#node-list');
  const apList = $('#ap-list');
  const qualitySummary = $('#node-quality-summary');
  clear(nodeList);
  clear(apList);
  clear(qualitySummary);

  const usableNodes = nodes.filter((node) => coverageOf(node).score >= 0.46).length;
  const weakNodes = nodes.filter((node) => {
    const score = coverageOf(node).score;
    return score > 0 && score < 0.46;
  }).length;
  qualitySummary?.append(
    create('span', 'quality-chip strong', `${usableNodes} usable`),
    create('span', weakNodes ? 'quality-chip weak' : 'quality-chip', `${weakNodes} weak`),
    create('span', 'quality-chip', `${nodes.length} total`)
  );

  if (!nodes.length) {
    nodeList?.append(emptyRow('Aucun ESP32-C6 vu par le master'));
  } else {
    for (const node of nodes) {
      nodeList?.append(nodeCard(node));
    }
  }

  if (!aps.length) {
    apList?.append(emptyRow('Scan AP ignore pour ce correctif'));
  } else {
    for (const ap of aps) {
      const pos = positionOf(ap);
      apList?.append(denseRow(
        text(ap.ssid || ap.label || ap.bssid, 'Hidden AP'),
        `${text(ap.bssid)} / ${text(ap.band)} / pos ${pos.source}`,
        [fmtDbm(ap.rssi_dbm), `ch ${text(ap.channel)}`, fmtAge(ap.last_seen_ms)],
        ap.status || 'visible'
      ));
    }
  }
  renderSystemPanel();
  renderLeftActions();
}

function renderModuleControls() {
  const categories = ['All', ...new Set(state.modules.map((mod) => text(mod.category, 'General')))];
  const select = $('#module-category');
  if (!select) return;
  const currentOptions = [...select.options].map((option) => option.value).join('|');
  if (currentOptions === categories.join('|')) return;
  select.replaceChildren(...categories.map((category) => {
    const option = create('option', '', category);
    option.value = category;
    option.selected = category === state.moduleCategory;
    return option;
  }));
}

function renderModulePresets() {
  const container = $('#module-presets');
  if (!container) return;
  clear(container);
  const enabled = new Set(state.modules.filter((mod) => mod.enabled !== false).map((mod) => mod.id));
  for (const preset of state.modulePresets) {
    const moduleIds = preset.module_ids || preset.modules || [];
    const active = moduleIds.length === enabled.size && moduleIds.every((id) => enabled.has(id));
    const button = create('button', `preset-button${active ? ' is-active' : ''}`);
    button.type = 'button';
    button.dataset.presetId = preset.id;
    button.append(
      create('strong', '', text(preset.label, preset.id)),
      create('small', '', `${moduleIds.length} modules`)
    );
    container.append(button);
  }
}

function renderModules() {
  renderModuleControls();
  renderModulePresets();
  const body = $('#module-table');
  clear(body);
  const q = state.moduleFilter.trim().toLowerCase();
  const modules = state.modules
    .filter((mod) => state.moduleCategory === 'All' || mod.category === state.moduleCategory)
    .filter((mod) => !q || `${mod.name} ${mod.id} ${mod.category}`.toLowerCase().includes(q))
    .sort((a, b) => {
      const rank = { active: 0, live: 0, available: 1, disabled: 2, offline: 3 };
      return (rank[a.status] ?? 3) - (rank[b.status] ?? 3) || Number(a.required_nodes || 0) - Number(b.required_nodes || 0);
    });
  const active = state.modules.filter((mod) => mod.status === 'active').length;
  const enabled = state.modules.filter((mod) => mod.enabled !== false).length;
  setText('module-summary', `${enabled} actifs / ${active} live`);

  if (!modules.length) {
    const row = create('tr');
    const cell = create('td', '', 'Aucun module');
    cell.colSpan = 5;
    row.append(cell);
    body?.append(row);
    return;
  }

  for (const mod of modules) {
    const row = create('tr');
    const toggle = create('label', 'module-toggle');
    const checkbox = document.createElement('input');
    checkbox.type = 'checkbox';
    checkbox.checked = mod.enabled !== false;
    checkbox.dataset.moduleId = mod.id;
    checkbox.setAttribute('aria-label', `${text(mod.name, mod.id)} actif`);
    toggle.append(checkbox);
    row.append(
      tableCell(`${text(mod.name, mod.id)}`, text(mod.category, 'General')),
      create('td'),
      create('td', '', String(mod.required_nodes || 1)),
      pillCell(mod.effective_status || mod.status || 'offline'),
      create('td', '', fmtPct(mod.confidence))
    );
    row.children[1].append(toggle);
    body?.append(row);
  }
}

function tableCell(title, subtitle) {
  const cell = create('td');
  cell.append(create('strong', '', title), create('small', '', subtitle));
  return cell;
}

function pillCell(value) {
  const cell = create('td');
  cell.append(create('span', `state-pill state-${statusClass(value)}`, value));
  return cell;
}

function firstDefined(...values) {
  return values.find((value) => value !== undefined && value !== null && value !== '');
}

function finiteNumber(...values) {
  for (const value of values) {
    const n = Number(value);
    if (Number.isFinite(n)) return n;
  }
  return null;
}

function personKey(person, index) {
  return String(firstDefined(person?.id, person?.track_id, person?.person_id, person?.label, `person_${index + 1}`));
}

function globalVitalsSources() {
  const edge = String(state.latest?.type || '').includes('vitals') ? state.latest : {};
  return [
    state.latest?.vital_signs || {},
    edge,
    state.edgeVitals?.edge_vitals || {},
    state.vitals?.vital_signs || state.vitals || {},
  ];
}

function firstMetric(fieldNames, sources) {
  for (const source of sources) {
    if (!source || typeof source !== 'object') continue;
    for (const field of fieldNames) {
      const n = finiteNumber(source[field]);
      if (n != null) return n;
    }
  }
  return null;
}

function detectedPersons() {
  const latestPersons = Array.isArray(state.latest?.persons) ? state.latest.persons : [];
  const posePersons = Array.isArray(state.pose?.persons) ? state.pose.persons : [];
  const base = latestPersons.length
    ? latestPersons
    : posePersons.length
      ? posePersons
      : state.pose?.person
        ? [state.pose.person]
        : primaryPerson()
          ? [primaryPerson()]
          : [];
  const reportedCount = Math.max(
    0,
    Number(firstDefined(
      state.latest?.count_evidence?.rendered_persons,
      state.latest?.count_evidence?.stable_persons,
      state.latest?.estimated_persons,
      state.edgeVitals?.edge_vitals?.n_persons,
      state.edgeVitals?.n_persons,
      base.length,
    )) || 0,
  );
  const count = Math.max(base.length, reportedCount);
  return Array.from({ length: count }, (_, index) => base[index] || {
    id: `detected_${index + 1}`,
    confidence: state.latest?.classification?.confidence,
    position_source: 'count_evidence',
  });
}

function personMetricSources(person, index) {
  const own = [person?.vital_signs, person?.vitals, person, person?.features].filter(Boolean);
  return index === 0 ? [...own, ...globalVitalsSources()] : own;
}

function breathingStatus(personId, breathing, timestamp) {
  if (breathing == null || !state.vitalsOptIn) {
    state.apneaZeroSince.delete(personId);
    return { label: '--', className: 'no-data' };
  }
  if (breathing === 0) {
    const started = state.apneaZeroSince.get(personId) || timestamp;
    state.apneaZeroSince.set(personId, started);
    if (timestamp - started > APNEA_ZERO_MS) return { label: 'APNEE', className: 'apnea' };
    return { label: 'LENT', className: 'slow' };
  }
  state.apneaZeroSince.delete(personId);
  if (breathing < 4) return { label: 'APNEE', className: 'apnea' };
  if (breathing < 8) return { label: 'LENT', className: 'slow' };
  if (breathing <= 25) return { label: 'NORMAL', className: 'normal' };
  return { label: 'RAPIDE', className: 'slow' };
}

function activityLabel(value) {
  if (value < 8) return 'IMMOBILE';
  if (value < 35) return 'PEU ACTIF';
  if (value < 70) return 'ACTIF';
  return 'TR\u00c8S ACTIF';
}

function activityValue(person) {
  const raw = finiteNumber(
    person?.motion_energy,
    person?.motionEnergy,
    person?.activity,
    person?.features?.motion_energy,
    person?.features?.motionEnergy,
    state.latest?.features?.motion_energy,
    state.latest?.features?.motion_band_power,
    state.latest?.classification?.motion_energy,
  );
  if (raw == null) return null;
  return clamp(raw <= 1 ? raw * 100 : raw, 0, 100);
}

function rememberBreathing(personId, breathing, timestamp) {
  if (breathing == null || !state.vitalsOptIn) return;
  const history = state.breathingHistory.get(personId) || [];
  history.push({ time: timestamp, value: breathing });
  const cutoff = timestamp - BREATHING_TREND_MS;
  while (history.length && history[0].time < cutoff) history.shift();
  state.breathingHistory.set(personId, history);
}

function drawMiniSparkline(canvas, history) {
  if (!(canvas instanceof HTMLCanvasElement)) return;
  const ctx = canvas.getContext('2d');
  if (!ctx) return;
  const w = canvas.width;
  const h = canvas.height;
  ctx.clearRect(0, 0, w, h);
  ctx.fillStyle = '#0d1117';
  ctx.fillRect(0, 0, w, h);
  ctx.strokeStyle = 'rgba(255,255,255,0.12)';
  ctx.beginPath();
  ctx.moveTo(0, h - 1);
  ctx.lineTo(w, h - 1);
  ctx.stroke();
  if (!history || history.length < 2) return;
  const start = history[0].time;
  const span = Math.max(1, history[history.length - 1].time - start);
  ctx.strokeStyle = '#39d18f';
  ctx.lineWidth = 1.5;
  ctx.beginPath();
  history.forEach((point, index) => {
    const x = ((point.time - start) / span) * (w - 2) + 1;
    const y = h - 2 - (clamp(point.value, 0, 30) / 30) * (h - 4);
    if (index === 0) ctx.moveTo(x, y);
    else ctx.lineTo(x, y);
  });
  ctx.stroke();
}

function drawBreathingTrend(people) {
  const canvas = $('#breathing-trend-canvas');
  if (!(canvas instanceof HTMLCanvasElement)) return;
  const ctx = canvas.getContext('2d');
  if (!ctx) return;
  const w = canvas.width;
  const h = canvas.height;
  const left = 28;
  const right = w - 8;
  const top = 8;
  const bottom = h - 16;
  const nowMs = Date.now();
  const primary = people[0];
  const id = primary ? personKey(primary, 0) : null;
  const history = id ? (state.breathingHistory.get(id) || []).filter((point) => point.time >= nowMs - BREATHING_TREND_MS) : [];

  ctx.clearRect(0, 0, w, h);
  ctx.fillStyle = '#0d1117';
  ctx.fillRect(0, 0, w, h);
  ctx.strokeStyle = 'rgba(255,255,255,0.18)';
  ctx.lineWidth = 1;
  ctx.beginPath();
  ctx.moveTo(left, top);
  ctx.lineTo(left, bottom);
  ctx.lineTo(right, bottom);
  ctx.stroke();

  const thresholdY = bottom - (4 / 30) * (bottom - top);
  ctx.save();
  ctx.setLineDash([4, 3]);
  ctx.strokeStyle = '#ff3040';
  ctx.beginPath();
  ctx.moveTo(left, thresholdY);
  ctx.lineTo(right, thresholdY);
  ctx.stroke();
  ctx.restore();

  ctx.fillStyle = 'rgba(232,236,224,0.58)';
  ctx.font = '10px JetBrains Mono, Consolas, monospace';
  ctx.fillText('30', 4, top + 4);
  ctx.fillText('0', 10, bottom + 2);
  ctx.fillText('4', 12, thresholdY + 3);

  if (history.length < 2) {
    ctx.fillStyle = 'rgba(232,236,224,0.36)';
    ctx.fillText('no data', left + 56, top + 34);
    setText('trend-person-label', primary ? text(primary.label || primary.id || primary.track_id, `person ${1}`) : '--');
    return;
  }

  ctx.strokeStyle = '#39d18f';
  ctx.lineWidth = 2;
  ctx.beginPath();
  history.forEach((point, index) => {
    const x = left + ((point.time - (nowMs - BREATHING_TREND_MS)) / BREATHING_TREND_MS) * (right - left);
    const y = bottom - (clamp(point.value, 0, 30) / 30) * (bottom - top);
    if (index === 0) ctx.moveTo(x, y);
    else ctx.lineTo(x, y);
  });
  ctx.stroke();
  setText('trend-person-label', text(primary?.label || primary?.id || primary?.track_id, 'person 1'));
}

function renderVitalsTabs() {
  for (const tab of $$('[data-vitals-tab]')) {
    const active = tab.dataset.vitalsTab === state.vitalsTab;
    tab.classList.toggle('is-active', active);
    tab.setAttribute('aria-selected', active ? 'true' : 'false');
  }
  for (const panel of $$('[data-vitals-panel]')) {
    panel.classList.toggle('is-active', panel.dataset.vitalsPanel === state.vitalsTab);
  }
}

function alertIcon(type) {
  const value = String(type || '').toLowerCase();
  if (value.includes('apnea')) return 'BR';
  if (value.includes('cardiac') || value.includes('heart')) return 'HR';
  if (value.includes('fall')) return 'CH';
  if (value.includes('critical')) return '!';
  return 'i';
}

function renderRightAlerts() {
  const listed = getAlerts({ limit: 20 });
  setText('alerts-count', String(listed.length));
  setText('alerts-title-count', String(listed.length));
  const list = $('#right-alert-list');
  clear(list);
  if (!listed.length) {
    list?.append(emptyRow('Aucune alerte active'));
    return;
  }
  for (const alert of listed) {
    const item = create('li', `right-alert-item severity-${statusClass(alert.severity || 'warning')}`);
    const stamp = new Date(Number(alert.created_at || alert.time || Date.now())).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
    item.append(
      create('span', 'right-alert-icon', alertIcon(alert.type)),
      create('time', '', stamp),
      create('strong', '', alert.title || String(alert.type || 'ALERTE').toUpperCase()),
      create('p', '', alert.message || 'Alerte active.'),
    );
    list?.append(item);
  }
}

function renderVitalsDashboard() {
  const vitals = state.vitals?.vital_signs || state.vitals || {};
  const edgeVitals = state.edgeVitals?.edge_vitals || {};
  const latestVitals = state.latest?.vital_signs || {};
  const edge = String(state.latest?.type || '').includes('vitals') ? state.latest : {};
  const presence = firstDefined(state.latest?.classification?.presence, edge.presence, false);
  const motion = firstDefined(state.latest?.classification?.motion_level, edge.motion ? 'motion' : undefined, 'unknown');
  const persons = firstDefined(state.latest?.count_evidence?.rendered_persons, state.latest?.estimated_persons, edge.n_persons, edgeVitals.n_persons, 0);
  const loc = locationPerson();
  const person = primaryPerson();
  const breathing = firstDefined(latestVitals.breathing_rate_bpm, edge.breathing_rate_bpm, edgeVitals.breathing_rate_bpm, vitals.breathing_rate_bpm);
  const heart = firstDefined(latestVitals.heart_rate_bpm, edge.heartrate_bpm, edgeVitals.heartrate_bpm, vitals.heart_rate_bpm);
  const quality = firstDefined(latestVitals.signal_quality, edge.presence_score, edgeVitals.presence_score, vitals.signal_quality);
  const people = detectedPersons();
  const cards = $('#vitals-person-cards');
  const timestamp = Date.now();

  setText('presence-state', presence ? 'presence' : 'absent');
  setText('person-count', approximatePersonLabel(Math.max(Number(persons || 0), people.length)));
  setText('motion-level', text(motion, 'unknown'));
  setText('location-vitals', loc ? 'Zone approximative' : '--');
  setText('posture-state', activityLevelFromMotionEnergy(person || people[0]));
  setText('breathing-rate', breathing ? `${Number(breathing).toFixed(1)} rpm` : '--');
  setText('heart-rate', state.vitalsOptIn && heart ? `${Number(heart).toFixed(1)} bpm` : state.vitalsOptIn ? '--' : 'masquee');
  setText('signal-quality', fmtPct(quality));
  clear(cards);

  if (!people.length) {
    cards?.append(emptyRow('Aucune personne detectee'));
  }

  people.forEach((item, index) => {
    const id = personKey(item, index);
    const label = text(item.display_label || item.label || item.name || item.id || item.track_id, `Personne ${index + 1}`);
    const sources = personMetricSources(item, index);
    const breathingValue = firstMetric(['breathing_rate_bpm', 'breathing_bpm', 'breathing_rate', 'respiration_bpm', 'br'], sources);
    const heartValue = state.vitalsOptIn
      ? firstMetric(['heart_rate_bpm', 'heartrate_bpm', 'hr_proxy_bpm', 'heart_rate', 'hr'], sources)
      : null;
    const activity = activityValue(item);
    const activityPct = activity == null ? 0 : activity;
    const status = breathingStatus(id, breathingValue, timestamp);
    const presenceConfidence = confidencePercentValue(
      item?.confidence,
      item?.tracking_confidence,
      item?.presence_confidence,
      state.latest?.classification?.confidence,
    );
    const breathingConfidence = confidencePercentValue(
      firstMetric(['breathing_confidence', 'respiration_confidence', 'signal_quality', 'confidence'], sources),
      state.latest?.classification?.confidence,
    );

    rememberBreathing(id, breathingValue, timestamp);

    const card = create('article', `vitals-person-card breathing-${status.className}`);
    const head = create('div', 'vitals-person-head');
    head.append(
      create('strong', '', label),
      create('span', `breathing-status ${status.className}`, status.label),
    );

    const confidenceRows = create('div', 'person-confidence-lines');
    const presenceRow = create('div', 'person-confidence-line');
    presenceRow.append(
      create('span', '', 'Présence'),
      create('strong', '', `${fmtConfidence(presenceConfidence)} ${confidenceDots(presenceConfidence)}`),
    );
    const zoneRow = create('div', 'person-confidence-line');
    zoneRow.append(create('span', '', 'Zone'), create('strong', '', 'approximative'));
    const respirationRow = create('div', 'person-confidence-line');
    respirationRow.append(
      create('span', '', 'Respiration'),
      create('strong', '', `${breathingValue == null ? '--' : Math.round(breathingValue)} rpm (confiance ${fmtConfidence(breathingConfidence)})`),
    );
    confidenceRows.append(presenceRow, zoneRow, respirationRow);
    const breathRow = create('div', 'breathing-readout');
    const breathValue = create('span', 'breathing-value', breathingValue == null ? '--' : breathingValue.toFixed(1));
    const breathUnit = create('span', 'breathing-unit', 'rpm');
    const sparkline = create('canvas', 'breathing-sparkline');
    sparkline.width = 100;
    sparkline.height = 30;
    breathRow.append(breathValue, breathUnit, sparkline);

    const heartRow = create('div', 'heart-beta-row');
    heartRow.title = 'Rythme cardiaque beta: estimation CSI non certifiee.';
    heartRow.append(
      create('span', 'heart-beta-label', 'Rythme cardiaque'),
      create('span', 'heart-beta-pill', 'BETA'),
      create('strong', '', heartValue == null ? '--' : `${Math.round(heartValue)} bpm`),
    );

    const activityRow = create('div', 'activity-row');
    const progress = create('div', 'activity-progress');
    const fill = create('span');
    fill.style.width = `${activityPct}%`;
    progress.append(fill);
    activityRow.append(
      create('span', 'activity-label', activity == null ? '--' : activityLabel(activityPct)),
      progress,
      create('span', 'activity-percent', activity == null ? '--' : `${Math.round(activityPct)}`),
    );

    card.append(head, confidenceRows, breathRow, heartRow, activityRow);
    cards?.append(card);
    drawMiniSparkline(sparkline, state.breathingHistory.get(id) || []);
  });

  setText('vitals-status', state.vitalsOptIn ? breathing || heart || presence || people.length ? 'live' : 'attente' : 'opt-in off');
  renderRightAlerts();
  drawBreathingTrend(people);
  renderVitalsTabs();
}

function appStateSnapshot(reason = 'update') {
  return {
    reason,
    updatedAt: Date.now(),
    source: 'console',
    topology: state.topology,
    modules: state.modules,
    vitals: state.vitals,
    location: state.location,
    edgeVitals: state.edgeVitals,
    calibration: state.calibration,
    pose: state.pose,
    latest: state.latest,
    vitalsOptIn: state.vitalsOptIn,
  };
}

function publishSharedState(reason = 'update') {
  const snapshot = appStateSnapshot(reason);
  processAlertState(snapshot);
}

function renderVitals() {
  renderVitalsDashboard();
}

function recordCalibrationEvent(message, level = 'info') {
  state.calibrationEvents.unshift({
    message,
    level,
    time: new Date().toLocaleTimeString(),
  });
  state.calibrationEvents = state.calibrationEvents.slice(0, 6);
}

function renderCalibrationLog() {
  const log = $('#calibration-log');
  clear(log);
  for (const event of state.calibrationEvents) {
    const item = create('li', event.level === 'info' ? '' : event.level);
    item.append(create('time', '', event.time), create('span', '', event.message));
    log?.append(item);
  }
}

function noteCalibrationStatus(status, data) {
  if (state.calibrationLastStatus === null) {
    state.calibrationLastStatus = status;
    return;
  }
  if (state.calibrationLastStatus === status) return;
  const frames = Number(data.frame_count || 0);
  const suffix = frames > 0 ? ` (${frames} frame(s))` : '';
  recordCalibrationEvent(`Calibration ${state.calibrationLastStatus} -> ${status}${suffix}`);
  state.calibrationLastStatus = status;
}

function renderCalibration() {
  const data = state.calibration || {};
  const status = text(data.status, 'unknown');
  const auto = data.auto_mode || {};
  const frameCount = Math.max(0, Number(data.frame_count || 0));
  const minFrames = Math.max(1, Number(data.min_frames || CALIBRATION_MIN_FRAMES_FALLBACK));
  const progress = status === 'Fresh'
    ? 100
    : status === 'Collecting'
      ? Math.min(100, Math.round((frameCount / minFrames) * 100))
      : 0;
  const table = $('#calibration-table');
  clear(table);
  noteCalibrationStatus(status, { ...data, frame_count: frameCount });
  setText('calibration-status', status);
  setText('calibration-progress-label', `${frameCount} / ${minFrames} frames`);
  const progressFill = $('#calibration-progress-fill');
  if (progressFill) progressFill.style.width = `${progress}%`;
  const rows = [
    ['actif', data.enabled],
    ['frame_count', frameCount],
    ['min_frames', minFrames],
    ['active_nodes', data.active_nodes],
    ['min_nodes', data.min_nodes],
    ['dedup_factor', data.dedup_factor],
    ['auto', auto.enabled ? `${auto.guard_state || 'unknown'} / ${auto.recommended_action || '--'}` : 'off'],
    ['blockers', Array.isArray(auto.blockers) && auto.blockers.length ? auto.blockers.join(', ') : 'none'],
  ];
  for (const [key, value] of rows) {
    const row = create('tr');
    row.append(create('td', '', key), create('td', '', text(value)));
    table?.append(row);
  }

  const ready = Boolean(state.topology?.readiness?.ready);
  const start = $('#start-calibration');
  if (start) {
    start.disabled = !ready || status === 'Collecting';
    start.title = ready ? 'Démarrer la calibration pièce vide' : 'Attendre au moins un nœud ESP32-C6 live';
  }
  const stop = $('#stop-calibration');
  if (stop) stop.disabled = !data.enabled;
  const autoButton = $('#auto-calibration');
  if (autoButton) {
    autoButton.classList.toggle('is-active', Boolean(auto.enabled));
    autoButton.textContent = auto.enabled ? 'Auto on' : 'Auto safe';
    autoButton.title = auto.enabled ? 'Desactiver auto-calibration safe' : 'Activer auto-calibration safe';
  }
  const abort = $('#abort-calibration');
  if (abort) abort.disabled = !data.enabled && !auto.enabled;
  setText('calibration-guard', auto.enabled
    ? `${text(auto.guard_state, 'unknown')} / quiet ${Number(auto.quiet_elapsed_sec || 0)}s/${Number(auto.quiet_window_sec || 30)}s / ${text(auto.recommended_action)}`
    : 'auto safe inactif');
  renderCalibrationLog();
}

function renderAll() {
  renderStatus();
  renderTopology();
  renderLocationPlan();
  renderPlacementControls();
  renderFleet();
  renderModules();
  renderVitals();
  renderCalibration();
}

function panelFromHash() {
  const panel = window.location.hash.replace(/^#/, '');
  return VALID_PANELS.has(panel) ? panel : 'fleet';
}

function activatePanel(panel, updateHash = false) {
  if (!VALID_PANELS.has(panel)) panel = 'fleet';
  state.activePanel = panel;
  for (const button of $$('[data-panel]')) button.classList.toggle('is-active', button.dataset.panel === panel);
  for (const view of $$('.panel-view')) view.classList.toggle('is-active', view.dataset.view === panel);
  if (updateHash && window.location.hash !== `#${panel}`) {
    history.pushState(null, '', `#${panel}`);
  }
}

async function refreshRest() {
  const [topologyResult, modulesResult, vitalsResult, locationResult, edgeVitalsResult, calibrationResult, poseResult, fleetResult, versionResult] = await Promise.allSettled([
    fetchJson('/api/v1/topology'),
    fetchJson('/api/v1/modules'),
    fetchJson('/api/v1/vital-signs').catch(() => null),
    fetchJson('/api/v1/location').catch(() => null),
    fetchJson('/api/v1/edge-vitals').catch(() => null),
    fetchJson('/api/v1/calibration').catch(() => null),
    fetchJson('/api/v1/pose/current').catch(() => null),
    fetchJson('/api/v1/fleet').catch(() => null),
    fetchJson('/api/v1/version').catch(() => null),
  ]);

  if (topologyResult.status === 'fulfilled') {
    state.topology = topologyResult.value;
  } else {
    state.topology = null;
    logEvent(topologyResult.reason.message, 'warn');
  }
  if (modulesResult.status === 'fulfilled') {
    state.modules = modulesResult.value.modules || [];
    state.modulePresets = modulesResult.value.presets || [];
  }
  if (vitalsResult.status === 'fulfilled') state.vitals = vitalsResult.value;
  if (locationResult.status === 'fulfilled') state.location = locationResult.value;
  if (edgeVitalsResult.status === 'fulfilled') state.edgeVitals = edgeVitalsResult.value;
  if (calibrationResult.status === 'fulfilled') state.calibration = calibrationResult.value;
  if (poseResult.status === 'fulfilled') state.pose = poseResult.value;
  if (fleetResult.status === 'fulfilled') state.fleet = fleetResult.value;
  if (versionResult.status === 'fulfilled') state.version = versionResult.value;
  renderAll();
  publishSharedState('rest');
  await maybeAutoPersistPlacement();
}

function connectWs() {
  if (state.ws && [WebSocket.OPEN, WebSocket.CONNECTING].includes(state.ws.readyState)) return;
  const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
  const ws = new WebSocket(`${protocol}//${window.location.host}/ws/sensing`);
  state.ws = ws;

  ws.onopen = () => {
    logEvent('WebSocket sensing connecté');
    if (state.reconnectTimer) clearTimeout(state.reconnectTimer);
  };
  ws.onmessage = (event) => {
    try {
      state.latest = JSON.parse(event.data);
      renderTopology();
      renderLocationPlan();
      renderVitals();
      publishSharedState('ws');
    } catch (error) {
      logEvent(`Frame sensing invalide: ${error.message}`, 'warn');
    }
  };
  ws.onclose = () => {
    state.ws = null;
    logEvent('WebSocket sensing déconnecté', 'warn');
    state.reconnectTimer = setTimeout(connectWs, 3000);
  };
  ws.onerror = () => ws.close();
}

function selectNode(nodeId) {
  state.placementSelectedId = nodeId;
  state.placementError = '';
  state.placementServerError = '';
  renderTopology();
  renderPlacementControls();
}

function updatePositionFromInputs() {
  if (state.placementAutoMode) return;
  const nodes = Array.isArray(state.topology?.nodes) ? state.topology.nodes : [];
  const selected = nodes.find((node) => nodeIdOf(node) === state.placementSelectedId);
  if (!selected) return;
  const x = parsePlacementInput('placement-x');
  const y = parsePlacementInput('placement-y');
  const z = parsePlacementInput('placement-z');
  state.placementServerError = '';
  if (![x, y, z].every(Number.isFinite)) {
    state.placementError = 'Coordonnées invalides.';
    refreshPlacementDirty();
    renderPlacementControls();
    return;
  }
  const id = nodeIdOf(selected);
  const next = {
    ...clonePosition(currentNodePosition(selected)),
    x,
    y,
    z,
    source: 'manual',
    confidence: 1,
  };
  state.placementDraft.set(id, next);
  state.placementError = validatePosition(next);
  refreshPlacementDirty();
  renderTopology();
  renderFleet();
  renderPlacementControls();
}

function beginNodeDrag(event) {
  if (state.placementAutoMode) return;
  if (event.button !== undefined && event.button !== 0) return;
  const target = event.target instanceof Element ? event.target : null;
  const markerEl = target?.closest('.topology-marker.node');
  const nodeId = markerEl
    ? Number(markerEl.dataset.nodeId)
    : nodeIdAtCanvasPoint(event);
  if (!Number.isFinite(nodeId)) return;
  event.preventDefault();
  state.draggingNodeId = nodeId;
  selectNode(nodeId);
}

function beginPlacementPan(event) {
  if (event.button !== undefined && event.button !== 0) return;
  const stage = event.currentTarget instanceof HTMLElement ? event.currentTarget : null;
  const target = event.target instanceof Element ? event.target : null;
  if (!stage || target?.closest('.topology-marker')) return;
  if (!state.placementAutoMode && nodeIdAtCanvasPoint(event) !== null) return;
  event.preventDefault();
  state.placementPanDrag = {
    pointerId: event.pointerId,
    startClientX: event.clientX,
    startClientY: event.clientY,
    startPan: { ...state.placementPan },
    moved: false,
  };
  stage.classList.add('is-panning');
  try {
    stage.setPointerCapture(event.pointerId);
  } catch (_error) {
    // Pointer capture is best-effort; window-level listeners keep panning active.
  }
}

function updateNodeDrag(event) {
  if (!state.draggingNodeId) return;
  const stage = $('#topology-stage');
  const nodes = Array.isArray(state.topology?.nodes) ? state.topology.nodes : [];
  const node = nodes.find((item) => nodeIdOf(item) === state.draggingNodeId);
  if (!stage || !node) return;
  const next = stageToPosition(stage, event.clientX, event.clientY, currentNodePosition(node));
  state.placementDraft.set(state.draggingNodeId, next);
  state.placementError = validatePosition(next);
  markPlacementDirty();
  renderTopology();
  renderFleet();
}

function updatePlacementPan(event) {
  const drag = state.placementPanDrag;
  if (!drag) return;
  const stage = $('#topology-stage');
  if (!stage) return;
  event.preventDefault();
  const rect = stage.getBoundingClientRect();
  const [width, , depth] = roomDims();
  const zoom = Math.max(PLACEMENT_MIN_ZOOM, state.placementZoom || 1);
  const dx = event.clientX - drag.startClientX;
  const dy = event.clientY - drag.startClientY;
  state.placementPan = {
    x: drag.startPan.x - ((dx / Math.max(1, rect.width)) * width / zoom),
    z: drag.startPan.z + ((dy / Math.max(1, rect.height)) * depth / zoom),
  };
  drag.moved = drag.moved || Math.hypot(dx, dy) > 3;
  renderTopology();
}

function endNodeDrag() {
  state.draggingNodeId = null;
}

function endPlacementPan(event) {
  const drag = state.placementPanDrag;
  if (!drag) return;
  const stage = $('#topology-stage');
  if (stage) {
    stage.classList.remove('is-panning');
    try {
      stage.releasePointerCapture(drag.pointerId);
    } catch (_error) {
      // Matching best-effort capture release.
    }
  }
  state.placementPanDrag = null;
  if (drag.moved && event?.type === 'pointerup') event.preventDefault();
}

function handlePlacementWheel(event) {
  const stage = event.currentTarget instanceof HTMLElement ? event.currentTarget : null;
  if (!stage) return;
  event.preventDefault();
  const direction = event.deltaY < 0 ? 1 : -1;
  const factor = direction > 0 ? 1.15 : 1 / 1.15;
  zoomPlacementAt(stage, event.clientX, event.clientY, factor);
}

function handlePlacementDoubleClick(event) {
  event.preventDefault();
  fitPlacementViewToNodes();
}

function resetPlacement() {
  state.placementDirty = false;
  state.placementError = '';
  state.placementServerError = '';
  state.placementDraft.clear();
  state.placementOriginal.clear();
  syncPlacementState(true);
  renderAll();
}

async function persistPlacement() {
  const changed = changedPlacementNodes();
  if (!changed.length) return;
  const invalid = changed.find(({ pos }) => validatePosition(pos));
  if (invalid) {
    state.placementError = validatePosition(invalid.pos);
    renderPlacementControls();
    return;
  }
  const payload = changed.map(({ id, pos }) => {
    return {
      node_id: id,
      position_m: [pos.x, pos.y, pos.z],
    };
  });
  const save = $('#save-placement');
  if (save) save.disabled = true;
  state.placementServerError = '';
  try {
    await saveNodePositions(payload);
    state.placementDirty = false;
    state.placementError = '';
    state.placementServerError = '';
    state.placementOriginal.clear();
    state.placementDraft.clear();
    logEvent(`${payload.length} position(s) ESP32 enregistrée(s)`);
    await refreshRest();
  } catch (error) {
    state.placementServerError = error.message;
    state.placementError = error.message;
    logEvent(error.message, 'warn');
    renderPlacementControls();
  }
}

function nodeNeedsAutoPlacement(node) {
  const pos = positionOf(node);
  const source = statusClass(pos.source);
  return !['configured', 'manual'].includes(source) && Number(pos.confidence || 0) < 0.75;
}

function autoPositionFor(index, total) {
  const [width, height, depth] = roomDims();
  const n = Math.max(1, total);
  const angle = -0.35 + (index / n) * Math.PI * 2;
  return {
    x: Math.cos(angle) * width * 0.36,
    y: clamp(height * 0.42, 0.8, Math.max(0.8, height - 0.2)),
    z: Math.sin(angle) * depth * 0.36,
    source: 'auto',
    confidence: 0.58,
  };
}

function autoPlacementPayload() {
  if (!state.placementAutoMode) return [];
  const nodes = Array.isArray(state.topology?.nodes) ? state.topology.nodes : [];
  const candidates = nodes.filter(nodeNeedsAutoPlacement);
  return candidates.map((node, index) => {
    const pos = autoPositionFor(index, candidates.length);
    return {
      node_id: nodeIdOf(node),
      position_m: [pos.x, pos.y, pos.z],
      pos,
    };
  }).filter((item) => item.node_id > 0 && !validatePosition(item.pos));
}

async function autoPlaceUnknownNodes({ persist = false } = {}) {
  const payload = autoPlacementPayload();
  if (!payload.length) {
    setText('auto-placement-status', 'auto placement ok');
    return false;
  }
  for (const item of payload) state.placementDraft.set(item.node_id, item.pos);
  refreshPlacementDirty();
  renderTopology();
  renderPlacementControls();
  setText('auto-placement-status', `${payload.length} auto position(s)`);
  if (!persist) return true;
  try {
    await saveNodePositions(payload.map(({ node_id, position_m }) => ({ node_id, position_m })));
    state.placementDirty = false;
    state.placementDraft.clear();
    state.placementOriginal.clear();
    logEvent(`${payload.length} position(s) auto enregistree(s)`);
    setText('auto-placement-status', `${payload.length} auto saved`);
    return true;
  } catch (error) {
    state.placementServerError = error.message;
    logEvent(`Auto placement: ${error.message}`, 'warn');
    setText('auto-placement-status', 'auto placement blocked');
    return false;
  }
}

async function maybeAutoPersistPlacement() {
  if (!state.placementAutoMode) {
    setText('auto-placement-status', 'mode manuel');
    return;
  }
  if (state.autoPlacementSaving || state.placementDirty) return;
  const payload = autoPlacementPayload();
  if (!payload.length) {
    state.autoPlacementLastKey = '';
    state.autoPlacementStableCount = 0;
    setText('auto-placement-status', 'auto placement ok');
    return;
  }
  const [w, h, d] = roomDims();
  const key = `${w.toFixed(2)}:${h.toFixed(2)}:${d.toFixed(2)}:${payload.map((item) => item.node_id).join(',')}`;
  state.autoPlacementStableCount = key === state.autoPlacementLastKey ? state.autoPlacementStableCount + 1 : 1;
  state.autoPlacementLastKey = key;
  setText('auto-placement-status', `${payload.length} pending / stable ${state.autoPlacementStableCount}`);
  if (state.autoPlacementStableCount < AUTO_PLACEMENT_STABLE_REFRESHES) return;
  state.autoPlacementSaving = true;
  try {
    await autoPlaceUnknownNodes({ persist: true });
  } finally {
    state.autoPlacementSaving = false;
  }
}

function setPlacementAutoMode(enabled) {
  state.placementAutoMode = Boolean(enabled);
  localStorage.setItem('ruvsense:placement-mode', state.placementAutoMode ? 'auto' : 'manual');
  state.placementError = '';
  state.placementServerError = '';
  if (state.placementAutoMode) {
    state.placementDirty = false;
    state.placementDraft.clear();
    state.placementOriginal.clear();
    syncPlacementState(true);
    void maybeAutoPersistPlacement();
  }
  renderTopology();
  renderFleet();
  renderPlacementControls();
}

function setLeftAction(action, status, message) {
  state.leftActionCurrent = action;
  state.leftActions[action] = { status, message };
  renderLeftActions();
}

async function finishLeftActionAfter(startedAt) {
  const elapsed = Date.now() - startedAt;
  if (elapsed < ACTION_SPINNER_MS) await sleep(ACTION_SPINNER_MS - elapsed);
}

async function recalibrateFromLeftColumn() {
  const startedAt = Date.now();
  setLeftAction('recalibrate', 'loading', 'Recalibration...');
  try {
    const result = await fetchJson('/api/v1/config', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ recalibrate: true }),
    });
    if (result?.success === false) throw new Error(result.error || result.message || 'Recalibration rejetee');
    await finishLeftActionAfter(startedAt);
    setLeftAction('recalibrate', 'ok', 'OK');
    logEvent('Recalibration demandee');
    await refreshRest();
  } catch (error) {
    await finishLeftActionAfter(startedAt);
    setLeftAction('recalibrate', 'error', 'Erreur');
    logEvent(`Recalibrer: ${error.message}`, 'warn');
  }
}

async function reloadModulesFromLeftColumn() {
  const startedAt = Date.now();
  setLeftAction('reloadModules', 'loading', 'Rechargement modules...');
  try {
    const result = await fetchJson('/api/v1/modules');
    state.modules = result.modules || [];
    state.modulePresets = result.presets || [];
    renderModules();
    await finishLeftActionAfter(startedAt);
    setLeftAction('reloadModules', 'ok', 'OK');
    logEvent('Modules recharges');
  } catch (error) {
    await finishLeftActionAfter(startedAt);
    setLeftAction('reloadModules', 'error', 'Erreur');
    logEvent(`Recharger modules: ${error.message}`, 'warn');
  }
}

function bindActions() {
  for (const button of $$('[data-panel]')) {
    button.addEventListener('click', () => activatePanel(button.dataset.panel, true));
  }
  window.addEventListener('hashchange', () => activatePanel(panelFromHash()));
  $('#clear-log')?.addEventListener('click', () => {
    state.logSeen.clear();
    $('#event-log')?.replaceChildren();
  });
  $('#left-recalibrate')?.addEventListener('click', recalibrateFromLeftColumn);
  $('#left-reload-modules')?.addEventListener('click', reloadModulesFromLeftColumn);
  $('#module-filter')?.addEventListener('input', (event) => {
    state.moduleFilter = event.target.value;
    renderModules();
  });
  $('#module-category')?.addEventListener('change', (event) => {
    state.moduleCategory = event.target.value;
    renderModules();
  });
  $('#module-presets')?.addEventListener('click', async (event) => {
    const target = event.target instanceof Element ? event.target : null;
    const button = target?.closest('button[data-preset-id]');
    if (!button) return;
    const preset = state.modulePresets.find((item) => item.id === button.dataset.presetId);
    if (!preset) return;
    button.disabled = true;
    try {
      await setEnabledModules(preset.module_ids || preset.modules || []);
      logEvent(`Preset ${text(preset.label, preset.id)} appliqué`);
      await refreshRest();
    } catch (error) {
      logEvent(error.message, 'warn');
    } finally {
      button.disabled = false;
    }
  });
  $('#module-table')?.addEventListener('change', async (event) => {
    const input = event.target;
    if (!(input instanceof HTMLInputElement) || !input.dataset.moduleId) return;
    input.disabled = true;
    try {
      await setModuleEnabled(input.dataset.moduleId, input.checked);
      logEvent(`${input.dataset.moduleId} ${input.checked ? 'activé' : 'désactivé'}`);
      await refreshRest();
    } catch (error) {
      input.checked = !input.checked;
      logEvent(error.message, 'warn');
    } finally {
      input.disabled = false;
    }
  });
  $('#start-calibration')?.addEventListener('click', async () => {
    try {
      const result = await fetchJson('/api/v1/calibration/start', { method: 'POST' });
      if (result.success === false) throw new Error(result.error || 'Calibration start rejected');
      state.calibration = {
        ...(state.calibration || {}),
        ...result,
        status: 'Collecting',
        enabled: true,
        frame_count: Number(result.frame_count || 0),
      };
      recordCalibrationEvent('Calibration démarrée');
      logEvent('Calibration démarrée');
      renderCalibration();
      await refreshRest();
    } catch (error) {
      recordCalibrationEvent(error.message, 'warn');
      logEvent(error.message, 'warn');
      renderCalibration();
    }
  });
  $('#stop-calibration')?.addEventListener('click', async () => {
    try {
      const result = await fetchJson('/api/v1/calibration/stop', { method: 'POST' });
      if (result.success === false) throw new Error(result.error || 'Calibration stop rejected');
      state.calibration = {
        ...(state.calibration || {}),
        ...result,
        status: 'Fresh',
        enabled: true,
      };
      recordCalibrationEvent(`Calibration arrêtée (${Number(result.frame_count || 0)} frame(s))`);
      logEvent('Calibration arrêtée');
      renderCalibration();
      await refreshRest();
    } catch (error) {
      recordCalibrationEvent(error.message, 'warn');
      logEvent(error.message, 'warn');
      renderCalibration();
    }
  });
  $('#auto-calibration')?.addEventListener('click', async () => {
    const enabled = !Boolean(state.calibration?.auto_mode?.enabled);
    try {
      state.calibration = await fetchJson('/api/v1/calibration/auto', {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ enabled, policy: 'safe' }),
      });
      recordCalibrationEvent(enabled ? 'Auto safe active' : 'Auto safe desactive');
      renderCalibration();
      await refreshRest();
    } catch (error) {
      recordCalibrationEvent(error.message, 'warn');
      logEvent(error.message, 'warn');
    }
  });
  $('#abort-calibration')?.addEventListener('click', async () => {
    try {
      const result = await fetchJson('/api/v1/calibration/abort', { method: 'POST' });
      if (result.success === false) throw new Error(result.error || 'Calibration abort rejected');
      state.calibration = result.calibration || state.calibration;
      recordCalibrationEvent('Calibration abort');
      logEvent('Calibration abort');
      renderCalibration();
      await refreshRest();
    } catch (error) {
      recordCalibrationEvent(error.message, 'warn');
      logEvent(error.message, 'warn');
    }
  });
  $('#vitals-optin')?.addEventListener('change', (event) => {
    state.vitalsOptIn = Boolean(event.target.checked);
    localStorage.setItem('ruvsense:vitals-opt-in', state.vitalsOptIn ? 'true' : 'false');
    renderStatus();
    renderVitals();
    publishSharedState('vitals-opt-in');
  });
  for (const tab of $$('[data-vitals-tab]')) {
    tab.addEventListener('click', () => {
      state.vitalsTab = tab.dataset.vitalsTab || 'vitals';
      renderVitalsTabs();
      if (state.vitalsTab === 'trends') drawBreathingTrend(detectedPersons());
    });
  }
  $('#clear-alerts')?.addEventListener('click', () => {
    clearAlerts();
    renderRightAlerts();
  });
  window.addEventListener('ruvsense:alerts-changed', () => renderRightAlerts());
  const topologyStage = $('#topology-stage');
  topologyStage?.addEventListener('pointerdown', beginNodeDrag);
  topologyStage?.addEventListener('pointerdown', beginPlacementPan);
  topologyStage?.addEventListener('wheel', handlePlacementWheel, { passive: false });
  topologyStage?.addEventListener('dblclick', handlePlacementDoubleClick);
  window.addEventListener('pointermove', updateNodeDrag);
  window.addEventListener('pointermove', updatePlacementPan);
  window.addEventListener('pointerup', endNodeDrag);
  window.addEventListener('pointerup', endPlacementPan);
  window.addEventListener('pointercancel', endNodeDrag);
  window.addEventListener('pointercancel', endPlacementPan);
  window.addEventListener('resize', () => {
    renderTopology();
    renderLocationPlan();
    drawBreathingTrend(detectedPersons());
  });
  $('#placement-zoom-in')?.addEventListener('click', () => zoomPlacementBy(1.2));
  $('#placement-zoom-out')?.addEventListener('click', () => zoomPlacementBy(1 / 1.2));
  $('#placement-zoom-fit')?.addEventListener('click', fitPlacementViewToNodes);
  $('#placement-auto-mode')?.addEventListener('change', (event) => {
    setPlacementAutoMode(Boolean(event.target.checked));
  });
  $('#auto-place-now')?.addEventListener('click', async () => {
    await autoPlaceUnknownNodes({ persist: true });
  });
  $('#reset-placement')?.addEventListener('click', resetPlacement);
  $('#save-placement')?.addEventListener('click', persistPlacement);
  for (const id of ['placement-x', 'placement-y', 'placement-z']) {
    const input = document.getElementById(id);
    input?.addEventListener('input', updatePositionFromInputs);
    input?.addEventListener('change', updatePositionFromInputs);
  }
}

window.__ruvsenseConsoleTestApi = {
  state,
  selectNode,
  changedPlacementNodes: () => changedPlacementNodes().map(({ id, pos }) => ({ id, position_m: [pos.x, pos.y, pos.z] })),
  updatePositionFromInputs,
  fitPlacementViewToNodes,
  resetPlacementView,
  autoPlacementPayload,
  autoPlaceUnknownNodes,
  setPlacementAutoMode,
  placementView: () => ({ zoom: state.placementZoom, pan: { ...state.placementPan } }),
};

async function init() {
  bindActions();
  activatePanel(panelFromHash());
  await loadRoomConfig();
  await refreshRest();
  connectWs();
  setInterval(refreshRest, 3000);
}

init();

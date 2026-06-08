const state = {
  topology: null,
  modules: [],
  vitals: null,
  edgeVitals: null,
  calibration: null,
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
  draggingNodeId: null,
};

const VALID_PANELS = new Set(['fleet', 'modules', 'vitals', 'calibration', 'diagnostics']);
const $ = (selector) => document.querySelector(selector);
const $$ = (selector) => [...document.querySelectorAll(selector)];

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

function fmtPct(value) {
  const n = Number(value);
  return Number.isFinite(n) ? `${Math.round(n * 100)}%` : '--';
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
  return {
    left: Math.max(4, Math.min(96, ((pos.x / width) + 0.5) * 100)),
    top: Math.max(4, Math.min(96, (0.5 - (pos.z / depth)) * 100)),
  };
}

function stageToPosition(stage, clientX, clientY, current) {
  const rect = stage.getBoundingClientRect();
  const [width, , depth] = roomDims();
  const leftPct = clamp((clientX - rect.left) / Math.max(1, rect.width), 0.04, 0.96);
  const topPct = clamp((clientY - rect.top) / Math.max(1, rect.height), 0.04, 0.96);
  return {
    ...clonePosition(current),
    x: (leftPct - 0.5) * width,
    z: (0.5 - topPct) * depth,
    source: 'manual',
    confidence: 1,
  };
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
  if (pos.x < b.minX || pos.x > b.maxX) return `X doit rester entre ${b.minX.toFixed(2)} m et ${b.maxX.toFixed(2)} m.`;
  if (pos.y < b.minY || pos.y > b.maxY) return `Y hauteur doit rester entre ${b.minY.toFixed(2)} m et ${b.maxY.toFixed(2)} m.`;
  if (pos.z < b.minZ || pos.z > b.maxZ) return `Z doit rester entre ${b.minZ.toFixed(2)} m et ${b.maxZ.toFixed(2)} m.`;
  return '';
}

function refreshPlacementDirty() {
  state.placementDirty = changedPlacementNodes().length > 0;
}

function setPlacementInputValues(pos) {
  const b = roomBounds();
  const fields = [
    ['placement-x', 'x', b.minX, b.maxX],
    ['placement-y', 'y', b.minY, b.maxY],
    ['placement-z', 'z', b.minZ, b.maxZ],
  ];
  for (const [id, axis, min, max] of fields) {
    const input = document.getElementById(id);
    if (!(input instanceof HTMLInputElement)) continue;
    input.disabled = !pos;
    input.min = min.toFixed(2);
    input.max = max.toFixed(2);
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
  const [w, h, d] = roomDims();

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
  setText('room-size', `${w.toFixed(1)} x ${h.toFixed(1)} x ${d.toFixed(1)} m`);
  setText('readiness-label', ready ? 'prêt' : activeNodes > 0 ? 'limité' : 'pas prêt');
  setText('frame-rate', fmtHz(frameRate));
  setText('fleet-summary', `${activeNodes} live`);
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
  const a = pointToStage(positionOf(ap));
  const b = pointToStage(currentNodePosition(node));
  const dx = b.left - a.left;
  const dy = b.top - a.top;
  const line = create('div', `rf-link ${statusClass(link.source)}`);
  line.style.left = `${a.left}%`;
  line.style.top = `${a.top}%`;
  line.style.width = `${Math.hypot(dx, dy)}%`;
  line.style.transform = `rotate(${Math.atan2(dy, dx)}rad)`;
  line.title = `confidence ${fmtPct(link.confidence)}`;
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

function renderTopology() {
  const stage = $('#topology-stage');
  if (!stage) return;
  stage.replaceChildren();
  syncPlacementState();

  const aps = Array.isArray(state.topology?.access_points) ? state.topology.access_points : [];
  const nodes = Array.isArray(state.topology?.nodes) ? state.topology.nodes : [];
  const links = Array.isArray(state.topology?.links) ? state.topology.links : [];
  const compact = nodes.length > 24 || window.innerWidth < 520;

  if (!aps.length && !nodes.length) {
    stage.append(emptyRow('Aucun ESP32-C6 vu par le master'));
    return;
  }

  const apsByBssid = new Map();
  for (const ap of aps) {
    for (const key of [ap.bssid, ap.ap_id, ap.id].filter(Boolean)) {
      apsByBssid.set(String(key).toLowerCase(), ap);
    }
  }
  const nodesById = new Map(nodes.map((node) => [nodeIdOf(node), node]));
  for (const link of links) renderLink(stage, link, apsByBssid, nodesById);
  renderNodeDistances(stage, nodes);
  for (const ap of aps) stage.append(marker('ap', ap, compact && (aps.length > 12 || window.innerWidth < 520)));
  for (const node of nodes) stage.append(marker('node', node, compact));
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
  const error = $('#placement-error');
  if (error) error.classList.toggle('is-visible', Boolean(state.placementError));
  setPlacementInputValues(pos);
  const save = $('#save-placement');
  if (save) save.disabled = !state.placementDirty || Boolean(state.placementError) || !nodes.length;
  const reset = $('#reset-placement');
  if (reset) reset.disabled = !state.placementDirty;
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

function renderFleet() {
  const nodes = Array.isArray(state.topology?.nodes) ? state.topology.nodes : [];
  const aps = Array.isArray(state.topology?.access_points) ? state.topology.access_points : [];
  const nodeList = $('#node-list');
  const apList = $('#ap-list');
  clear(nodeList);
  clear(apList);

  if (!nodes.length) {
    nodeList?.append(emptyRow('Aucun ESP32-C6 vu par le master'));
  } else {
    for (const node of nodes) {
      const pos = currentNodePosition(node);
      const status = text(node.health_status || node.status, node.active ? 'live' : 'offline');
      nodeList?.append(denseRow(
        text(node.display_label || node.label, `ESP32-C6 #${node.node_id}`),
        `node_id ${node.node_id} / ${text(node.remote_addr, 'no addr')} / X ${pos.x.toFixed(2)} Y ${pos.y.toFixed(2)} Z ${pos.z.toFixed(2)}`,
        [fmtHz(node.frame_rate_hz), `CSI ${fmtAge(node.last_csi_ms ?? node.last_seen_ms)}`, fmtPct(pos.confidence)],
        status
      ));
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

function renderVitals() {
  const vitals = state.vitals?.vital_signs || state.vitals || {};
  const edgeVitals = state.edgeVitals?.edge_vitals || {};
  const latestVitals = state.latest?.vital_signs || {};
  const edge = String(state.latest?.type || '').includes('vitals') ? state.latest : {};
  const presence = firstDefined(state.latest?.classification?.presence, edge.presence, false);
  const motion = firstDefined(state.latest?.classification?.motion_level, edge.motion ? 'motion' : undefined, 'unknown');
  const persons = firstDefined(state.latest?.count_evidence?.rendered_persons, state.latest?.estimated_persons, edge.n_persons, edgeVitals.n_persons, 0);
  const breathing = firstDefined(latestVitals.breathing_rate_bpm, edge.breathing_rate_bpm, edgeVitals.breathing_rate_bpm, vitals.breathing_rate_bpm);
  const heart = firstDefined(latestVitals.heart_rate_bpm, edge.heartrate_bpm, edgeVitals.heartrate_bpm, vitals.heart_rate_bpm);
  const quality = firstDefined(latestVitals.signal_quality, edge.presence_score, edgeVitals.presence_score, vitals.signal_quality);

  setText('presence-state', presence ? 'présence' : 'absent');
  setText('person-count', String(persons || 0));
  setText('motion-level', text(motion, 'unknown'));
  setText('breathing-rate', breathing ? `${Number(breathing).toFixed(1)} bpm` : '--');
  setText('heart-rate', heart ? `${Number(heart).toFixed(1)} bpm` : '--');
  setText('signal-quality', fmtPct(quality));
  setText('vitals-status', breathing || heart || presence ? 'live' : 'attente');
}

function renderCalibration() {
  const data = state.calibration || {};
  const status = text(data.status, 'unknown');
  const table = $('#calibration-table');
  clear(table);
  setText('calibration-status', status);
  const rows = [
    ['actif', data.enabled],
    ['active_nodes', data.active_nodes],
    ['min_nodes', data.min_nodes],
    ['dedup_factor', data.dedup_factor],
  ];
  for (const [key, value] of rows) {
    const row = create('tr');
    row.append(create('td', '', key), create('td', '', text(value)));
    table?.append(row);
  }

  const ready = Boolean(state.topology?.readiness?.ready);
  const start = $('#start-calibration');
  if (start) {
    start.disabled = !ready;
    start.title = ready ? 'Démarrer la calibration pièce vide' : 'Attendre au moins un nœud ESP32-C6 live';
  }
}

function renderAll() {
  renderStatus();
  renderTopology();
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
  const [topologyResult, modulesResult, vitalsResult, edgeVitalsResult, calibrationResult] = await Promise.allSettled([
    fetchJson('/api/v1/topology'),
    fetchJson('/api/v1/modules'),
    fetchJson('/api/v1/vital-signs').catch(() => null),
    fetchJson('/api/v1/edge-vitals').catch(() => null),
    fetchJson('/api/v1/calibration').catch(() => null),
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
  if (edgeVitalsResult.status === 'fulfilled') state.edgeVitals = edgeVitalsResult.value;
  if (calibrationResult.status === 'fulfilled') state.calibration = calibrationResult.value;
  renderAll();
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
      renderVitals();
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
  const target = event.target instanceof Element ? event.target : null;
  const markerEl = target?.closest('.topology-marker.node');
  if (!markerEl) return;
  const nodeId = Number(markerEl.dataset.nodeId);
  if (!Number.isFinite(nodeId)) return;
  event.preventDefault();
  state.draggingNodeId = nodeId;
  selectNode(nodeId);
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

function endNodeDrag() {
  state.draggingNodeId = null;
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

function bindActions() {
  for (const button of $$('[data-panel]')) {
    button.addEventListener('click', () => activatePanel(button.dataset.panel, true));
  }
  window.addEventListener('hashchange', () => activatePanel(panelFromHash()));
  $('#clear-log')?.addEventListener('click', () => {
    state.logSeen.clear();
    $('#event-log')?.replaceChildren();
  });
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
      await fetchJson('/api/v1/calibration/start', { method: 'POST' });
      logEvent('Calibration démarrée');
      await refreshRest();
    } catch (error) {
      logEvent(error.message, 'warn');
    }
  });
  $('#stop-calibration')?.addEventListener('click', async () => {
    try {
      await fetchJson('/api/v1/calibration/stop', { method: 'POST' });
      logEvent('Calibration arrêtée');
      await refreshRest();
    } catch (error) {
      logEvent(error.message, 'warn');
    }
  });
  $('#topology-stage')?.addEventListener('pointerdown', beginNodeDrag);
  window.addEventListener('pointermove', updateNodeDrag);
  window.addEventListener('pointerup', endNodeDrag);
  window.addEventListener('pointercancel', endNodeDrag);
  $('#reset-placement')?.addEventListener('click', resetPlacement);
  $('#save-placement')?.addEventListener('click', persistPlacement);
  for (const id of ['placement-x', 'placement-y', 'placement-z']) {
    const input = document.getElementById(id);
    input?.addEventListener('input', updatePositionFromInputs);
    input?.addEventListener('change', updatePositionFromInputs);
  }
}

function fixtureTopology(count = 3) {
  const nodes = Array.from({ length: count }, (_, index) => {
    const angle = (index / Math.max(1, count)) * Math.PI * 2;
    return {
      node_id: index + 1,
      label: `ESP32-C6 ${index + 1}`,
      rssi_dbm: -42 - (index % 24),
      frame_rate_hz: 9 + (index % 5),
      last_seen_ms: 40 + index,
      sync_status: index % 3 === 0 ? 'valid' : 'no_sync',
      position: {
        x: Math.cos(angle) * 2.0,
        y: 1.1,
        z: Math.sin(angle) * 1.8,
        source: index % 5 === 0 ? 'configured' : 'estimated',
        confidence: index % 5 === 0 ? 0.9 : 0.42,
      },
    };
  });
  const aps = count === 0 ? [] : [
    { bssid: '02:11:22:33:44:01', ssid: 'mesh-east', channel: 6, band: '2.4GHz', rssi_dbm: -39, last_seen_ms: 80, status: 'visible', position: { x: -2.1, y: 2.0, z: -1.7, source: 'estimated', confidence: 0.3 } },
    { bssid: '02:11:22:33:44:02', ssid: 'mesh-west', channel: 11, band: '2.4GHz', rssi_dbm: -48, last_seen_ms: 90, status: 'visible', position: { x: 2.1, y: 2.0, z: 1.7, source: 'estimated', confidence: 0.3 } },
  ];
  return {
    product: 'RuvSense Edge',
    service: 'ruvsense-master',
    source: 'esp32',
    room: { name: 'fixture', dimensions_m: [5.2, 2.6, 4.8] },
    readiness: {
      ready: count >= 1,
      active_nodes: count,
      min_nodes: 1,
      fusion_mode: count === 0 ? 'offline' : count === 1 ? 'single_node' : count === 2 ? 'partial_multistatic' : 'multistatic',
    },
    wifi_scan: { interface: 'fixture', interval_secs: 10, available: true },
    access_points: aps,
    nodes,
    links: nodes.flatMap((node, index) => aps.length ? [{
      link_id: `${aps[index % aps.length].bssid}:node-${node.node_id}`,
      ap_bssid: aps[index % aps.length].bssid,
      node_id: node.node_id,
      source: index % 5 === 0 ? 'configured' : 'estimated',
      confidence: index % 5 === 0 ? 0.86 : 0.35,
    }] : []),
  };
}

window.__ruvsenseRenderFixture = (count = 3) => {
  state.topology = fixtureTopology(Number(count) || 0);
  renderAll();
  return state.topology;
};

window.__ruvsenseConsoleTestApi = {
  state,
  selectNode,
  changedPlacementNodes: () => changedPlacementNodes().map(({ id, pos }) => ({ id, position_m: [pos.x, pos.y, pos.z] })),
  updatePositionFromInputs,
};

async function init() {
  bindActions();
  activatePanel(panelFromHash());
  await refreshRest();
  connectWs();
  setInterval(refreshRest, 3000);
}

init();

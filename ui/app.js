const state = {
  topology: null,
  modules: [],
  vitals: null,
  calibration: null,
  latest: null,
  ws: null,
  reconnectTimer: null,
  moduleFilter: '',
  moduleCategory: 'All',
  activePanel: 'fleet',
  logSeen: new Set(),
};

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
  const pos = item?.position || {};
  if (Array.isArray(pos)) return { x: Number(pos[0]) || 0, y: Number(pos[1]) || 0, z: Number(pos[2]) || 0, source: 'configured', confidence: 0.9 };
  return {
    x: Number(pos.x) || 0,
    y: Number(pos.y) || 0,
    z: Number(pos.z) || 0,
    source: text(pos.source, item?.position_source || 'unknown'),
    confidence: Number(pos.confidence ?? item?.position_confidence ?? 0),
  };
}

function pointToStage(pos) {
  const [width, , depth] = roomDims();
  return {
    left: Math.max(4, Math.min(96, ((pos.x / width) + 0.5) * 100)),
    top: Math.max(4, Math.min(96, ((pos.z / depth) + 0.5) * 100)),
  };
}

function renderStatus() {
  const topology = state.topology;
  const readiness = topology?.readiness || {};
  const activeNodes = Number(readiness.active_nodes || topology?.nodes?.length || 0);
  const aps = Array.isArray(topology?.access_points) ? topology.access_points.length : 0;
  const ready = Boolean(readiness.ready);
  const fusion = text(readiness.fusion_mode, 'offline');
  const source = text(topology?.source, 'offline');
  const frameRate = Math.max(0, ...(topology?.nodes || []).map((node) => Number(node.frame_rate_hz) || 0));
  const [w, h, d] = roomDims();

  const master = $('#master-status');
  if (master) {
    master.classList.toggle('is-ready', ready);
    master.classList.toggle('is-degraded', !ready && activeNodes > 0);
    master.querySelector('strong').textContent = ready ? 'ready' : activeNodes > 0 ? 'limited' : 'offline';
  }
  setText('active-nodes', `${activeNodes}/${Number(readiness.min_nodes || 1)}`);
  setText('ap-count', String(aps));
  setText('fusion-mode', fusion);
  setText('source-mode', source);
  setText('scan-interface', text(topology?.wifi_scan?.interface, 'wlan0'));
  setText('room-size', `${w.toFixed(1)} x ${h.toFixed(1)} x ${d.toFixed(1)} m`);
  setText('readiness-label', ready ? 'ready' : activeNodes > 0 ? 'limited' : 'not ready');
  setText('frame-rate', fmtHz(frameRate));
  setText('fleet-summary', `${activeNodes} live`);
}

function marker(kind, item, compact) {
  const pos = positionOf(item);
  const p = pointToStage(pos);
  const label = kind === 'ap'
    ? text(item.ssid || item.label || item.bssid, 'Hidden AP')
    : text(item.label || `C6-${item.node_id}`, `C6-${item.node_id}`);
  const detail = kind === 'ap'
    ? `${fmtDbm(item.rssi_dbm)} / ch ${text(item.channel)}`
    : `${fmtHz(item.frame_rate_hz)} / ${fmtDbm(item.rssi_dbm)}`;
  const el = create('div', `topology-marker ${kind} ${statusClass(pos.source)}${compact ? ' compact' : ''}`);
  el.style.left = `${p.left}%`;
  el.style.top = `${p.top}%`;
  el.title = `${label} / ${pos.source} / confidence ${fmtPct(pos.confidence)}`;
  el.append(create('i'), create('strong', '', label), create('small', '', detail));
  return el;
}

function renderLink(stage, link, apsByBssid, nodesById) {
  const ap = apsByBssid.get(String(link.ap_bssid || '').toLowerCase());
  const node = nodesById.get(Number(link.node_id));
  if (!ap || !node) return;
  const a = pointToStage(positionOf(ap));
  const b = pointToStage(positionOf(node));
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

function renderTopology() {
  const stage = $('#topology-stage');
  if (!stage) return;
  stage.replaceChildren();

  const aps = Array.isArray(state.topology?.access_points) ? state.topology.access_points : [];
  const nodes = Array.isArray(state.topology?.nodes) ? state.topology.nodes : [];
  const links = Array.isArray(state.topology?.links) ? state.topology.links : [];
  const compact = nodes.length > 24 || window.innerWidth < 520;

  if (!aps.length && !nodes.length) {
    stage.append(emptyRow('Aucun AP ou ESP32-C6 live vu par le master'));
    return;
  }

  const apsByBssid = new Map(aps.map((ap) => [String(ap.bssid || '').toLowerCase(), ap]));
  const nodesById = new Map(nodes.map((node) => [Number(node.node_id), node]));
  for (const link of links) renderLink(stage, link, apsByBssid, nodesById);
  for (const ap of aps) stage.append(marker('ap', ap, compact && (aps.length > 12 || window.innerWidth < 520)));
  for (const node of nodes) stage.append(marker('node', node, compact));
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
    nodeList?.append(emptyRow('Aucun ESP32-C6 live'));
  } else {
    for (const node of nodes) {
      const pos = positionOf(node);
      nodeList?.append(denseRow(
        text(node.label, `ESP32-C6 ${node.node_id}`),
        `node_id ${node.node_id} / ${text(node.sync_status, 'no_sync')} / pos ${pos.source}`,
        [fmtHz(node.frame_rate_hz), fmtAge(node.last_seen_ms), fmtPct(pos.confidence)],
        'active'
      ));
    }
  }

  if (!aps.length) {
    apList?.append(emptyRow('Aucun AP detecte'));
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

function renderModules() {
  renderModuleControls();
  const body = $('#module-table');
  clear(body);
  const q = state.moduleFilter.trim().toLowerCase();
  const modules = state.modules
    .filter((mod) => state.moduleCategory === 'All' || mod.category === state.moduleCategory)
    .filter((mod) => !q || `${mod.name} ${mod.id} ${mod.category}`.toLowerCase().includes(q))
    .sort((a, b) => {
      const rank = { active: 0, available: 1, offline: 2 };
      return (rank[a.status] ?? 3) - (rank[b.status] ?? 3) || Number(a.required_nodes || 0) - Number(b.required_nodes || 0);
    });
  const active = state.modules.filter((mod) => mod.status === 'active').length;
  const available = state.modules.filter((mod) => mod.status === 'available').length;
  setText('module-summary', `${active} active / ${available} limited`);

  if (!modules.length) {
    const row = create('tr');
    const cell = create('td', '', 'No modules');
    cell.colSpan = 4;
    row.append(cell);
    body?.append(row);
    return;
  }

  for (const mod of modules) {
    const row = create('tr');
    row.append(
      tableCell(`${text(mod.name, mod.id)}`, text(mod.category, 'General')),
      create('td', '', String(mod.required_nodes || 1)),
      pillCell(mod.status || 'offline'),
      create('td', '', fmtPct(mod.confidence))
    );
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
  const latestVitals = state.latest?.vital_signs || {};
  const edge = String(state.latest?.type || '').includes('vitals') ? state.latest : {};
  const presence = firstDefined(state.latest?.classification?.presence, edge.presence, false);
  const motion = firstDefined(state.latest?.classification?.motion_level, edge.motion ? 'motion' : undefined, 'unknown');
  const persons = firstDefined(state.latest?.estimated_persons, edge.n_persons, 0);
  const breathing = firstDefined(latestVitals.breathing_rate_bpm, edge.breathing_rate_bpm, vitals.breathing_rate_bpm);
  const heart = firstDefined(latestVitals.heart_rate_bpm, edge.heartrate_bpm, vitals.heart_rate_bpm);
  const quality = firstDefined(latestVitals.signal_quality, edge.presence_score, vitals.signal_quality);

  setText('presence-state', presence ? 'present' : 'absent');
  setText('person-count', String(persons || 0));
  setText('motion-level', text(motion, 'unknown'));
  setText('breathing-rate', breathing ? `${Number(breathing).toFixed(1)} bpm` : '--');
  setText('heart-rate', heart ? `${Number(heart).toFixed(1)} bpm` : '--');
  setText('signal-quality', fmtPct(quality));
  setText('vitals-status', breathing || heart || presence ? 'live' : 'waiting');
}

function renderCalibration() {
  const data = state.calibration || {};
  const status = text(data.status, 'unknown');
  const table = $('#calibration-table');
  clear(table);
  setText('calibration-status', status);
  const rows = [
    ['enabled', data.enabled],
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
    start.title = ready ? 'Start empty-room baseline' : 'Wait for at least one live ESP32-C6 node';
  }
}

function renderAll() {
  renderStatus();
  renderTopology();
  renderFleet();
  renderModules();
  renderVitals();
  renderCalibration();
}

function activatePanel(panel) {
  state.activePanel = panel;
  for (const button of $$('[data-panel]')) button.classList.toggle('is-active', button.dataset.panel === panel);
  for (const view of $$('.panel-view')) view.classList.toggle('is-active', view.dataset.view === panel);
}

async function refreshRest() {
  const [topologyResult, modulesResult, vitalsResult, calibrationResult] = await Promise.allSettled([
    fetchJson('/api/v1/topology'),
    fetchJson('/api/v1/modules'),
    fetchJson('/api/v1/vital-signs').catch(() => null),
    fetchJson('/api/v1/calibration').catch(() => null),
  ]);

  if (topologyResult.status === 'fulfilled') {
    state.topology = topologyResult.value;
  } else {
    state.topology = null;
    logEvent(topologyResult.reason.message, 'warn');
  }
  if (modulesResult.status === 'fulfilled') state.modules = modulesResult.value.modules || [];
  if (vitalsResult.status === 'fulfilled') state.vitals = vitalsResult.value;
  if (calibrationResult.status === 'fulfilled') state.calibration = calibrationResult.value;
  renderAll();
}

function connectWs() {
  if (state.ws && [WebSocket.OPEN, WebSocket.CONNECTING].includes(state.ws.readyState)) return;
  const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
  const ws = new WebSocket(`${protocol}//${window.location.host}/ws/sensing`);
  state.ws = ws;

  ws.onopen = () => {
    logEvent('WebSocket sensing connected');
    if (state.reconnectTimer) clearTimeout(state.reconnectTimer);
  };
  ws.onmessage = (event) => {
    try {
      state.latest = JSON.parse(event.data);
      renderVitals();
    } catch (error) {
      logEvent(`Bad sensing frame: ${error.message}`, 'warn');
    }
  };
  ws.onclose = () => {
    state.ws = null;
    logEvent('WebSocket sensing disconnected', 'warn');
    state.reconnectTimer = setTimeout(connectWs, 3000);
  };
  ws.onerror = () => ws.close();
}

function bindActions() {
  for (const button of $$('[data-panel]')) {
    button.addEventListener('click', () => activatePanel(button.dataset.panel));
  }
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
  $('#start-calibration')?.addEventListener('click', async () => {
    try {
      await fetchJson('/api/v1/calibration/start', { method: 'POST' });
      logEvent('Calibration started');
      await refreshRest();
    } catch (error) {
      logEvent(error.message, 'warn');
    }
  });
  $('#stop-calibration')?.addEventListener('click', async () => {
    try {
      await fetchJson('/api/v1/calibration/stop', { method: 'POST' });
      logEvent('Calibration stopped');
      await refreshRest();
    } catch (error) {
      logEvent(error.message, 'warn');
    }
  });
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

async function init() {
  bindActions();
  activatePanel('fleet');
  await refreshRest();
  connectWs();
  setInterval(refreshRest, 3000);
}

init();

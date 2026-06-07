const state = {
  fleet: null,
  environment: null,
  modules: [],
  latest: null,
  ws: null,
  selectedCategory: 'All',
  reconnectTimer: null,
  environmentError: null,
  calibration: null,
};

const $ = (selector) => document.querySelector(selector);

function logEvent(message, level = 'info') {
  const list = $('#event-log');
  if (!list) return;
  const item = document.createElement('li');
  item.className = `event event-${level}`;
  const stamp = new Date().toLocaleTimeString();
  item.textContent = `${stamp} - ${message}`;
  list.prepend(item);
  while (list.children.length > 80) list.lastElementChild?.remove();
}

async function fetchJson(path, options) {
  const response = await fetch(path, { cache: 'no-store', ...options });
  if (!response.ok) {
    throw new Error(`${path} returned HTTP ${response.status}`);
  }
  return response.json();
}

function fmtHz(value) {
  const n = Number(value);
  return Number.isFinite(n) ? `${n.toFixed(1)} Hz` : '0.0 Hz';
}

function fmtAge(ms) {
  const n = Number(ms);
  if (!Number.isFinite(n)) return 'never';
  if (n < 1000) return `${Math.round(n)} ms`;
  return `${(n / 1000).toFixed(1)} s`;
}

function percent(value) {
  const n = Number(value);
  return Number.isFinite(n) ? `${Math.round(n * 100)}%` : '--';
}

function setText(id, value) {
  const el = document.getElementById(id);
  if (el) el.textContent = value;
}

function setMasterStatus(status, text) {
  const pill = $('#master-status');
  if (!pill) return;
  pill.className = `status-pill status-${status}`;
  pill.querySelector('span:last-child').textContent = text;
}

function operatorFusionStatus(fusion, ready, active) {
  const value = String(fusion || '').toLowerCase();
  if (ready || value === 'active') return 'Fusion active';
  if (active > 0 || value === 'degraded') return 'En attente de quorum';
  return 'Hors ligne';
}

function operatorSourceLabel(fleet) {
  const raw = String(fleet?.source_mode?.raw || fleet?.source || 'offline').toLowerCase();
  if (raw === 'esp32') return 'Live ESP32 CSI';
  if (raw === 'auto') return 'Auto, sans simulation';
  if (raw === 'simulate' || raw === 'simulated') return 'Simulation dev/test';
  if (raw === 'wifi') return 'RSSI hote';
  return 'Hors ligne';
}

function nodeStatusLabel(node) {
  const status = String(node.status || (node.active ? 'active' : 'offline')).toLowerCase();
  if (status === 'active' || node.active) return 'Actif';
  if (status === 'stale') return 'Signal perdu';
  if (status === 'degraded') return 'Degrade';
  return 'Noeud non vu';
}

function nodeNextAction(node) {
  const status = String(node.status || (node.active ? 'active' : 'offline')).toLowerCase();
  if (status === 'active' || node.active) return `Derniere trame ${fmtAge(node.last_seen_ms)}`;
  if (status === 'stale') return 'Verifier alimentation ou WiFi';
  return 'Verifier provisioning et IP du Pi';
}

function updateChecklist() {
  const fleet = state.fleet || {};
  const env = state.environment || {};
  const calibrationStatus = String(state.calibration?.status || state.calibration?.calibration?.status || '').toLowerCase();
  const active = Number(fleet.active_nodes || 0);
  const minNodes = Number(fleet.min_nodes || 3);
  const checks = {
    master: Boolean(fleet.status || fleet.source || fleet.source_mode),
    ap1: (env.access_points || []).length >= 1,
    ap2: (env.access_points || []).length >= 2,
    nodes: active >= minNodes,
    quorum: Boolean(fleet.ready),
    calibration: ['ready', 'complete', 'completed', 'baseline_ready'].includes(calibrationStatus),
  };
  let done = 0;
  for (const [key, value] of Object.entries(checks)) {
    const row = document.querySelector(`[data-check="${key}"]`);
    if (!row) continue;
    row.classList.toggle('check-done', Boolean(value));
    row.classList.toggle('check-pending', !value);
    if (value) done++;
  }
  setText('setup-summary', `${done}/6`);
}

function renderFleet() {
  const hasFleet = Boolean(state.fleet);
  const fleet = state.fleet || {};
  const active = Number(fleet.active_nodes || 0);
  const minNodes = Number(fleet.min_nodes || 3);
  const ready = Boolean(fleet.ready);
  const fusion = fleet.fusion_status || (ready ? 'active' : active > 0 ? 'degraded' : 'offline');

  setMasterStatus(
    hasFleet ? ready ? 'live' : active > 0 ? 'degraded' : 'offline' : 'offline',
    hasFleet ? ready ? 'Master joignable' : active > 0 ? `Besoin de ${minNodes} noeuds` : 'Master en attente' : 'Master non joignable'
  );
  setText('fusion-status', operatorFusionStatus(fusion, ready, active));
  setText('active-nodes', String(active));
  setText('min-nodes', String(minNodes));
  setText('frame-rate', fmtHz(fleet.frame_rate_hz));
  setText('source-mode', operatorSourceLabel(fleet));
  setText('fleet-readiness', ready ? 'pret' : active > 0 ? 'besoin de quorum' : 'en attente');

  const list = $('#node-list');
  if (!list) return;
  const nodes = Array.isArray(fleet.nodes) ? fleet.nodes : Array.isArray(state.environment?.nodes) ? state.environment.nodes : [];
  list.replaceChildren();

  if (!nodes.length) {
    list.append(emptyRow('No configured ESP32-C6 nodes'));
    return;
  }

  for (const node of nodes) {
    const id = node.node_id ?? '?';
    const status = String(node.status || (node.active ? 'active' : 'offline')).toLowerCase();
    const linkedAp = node.linked_ap || node.ap_id || 'AP non assigne';
    const row = document.createElement('article');
    row.className = `device-row device-${status}`;
    row.innerHTML = `
      <div>
        <strong>${node.label || `ESP32-C6 ${id}`}</strong>
        <span>node_id ${id} / slot ${node.tdm_slot ?? '-'} sur ${node.tdm_total ?? '-'} / ${linkedAp}</span>
      </div>
      <div class="device-meta">
        <span>${nodeStatusLabel(node)}</span>
        <span>${fmtHz(node.frame_rate_hz)}</span>
        <span>${nodeNextAction(node)}</span>
      </div>
    `;
    list.append(row);
  }
  updateChecklist();
}

function renderEnvironment() {
  const env = state.environment;
  if (!env) {
    renderEnvironmentUnavailable();
    updateChecklist();
    return;
  }

  setText('room-name', env.room?.name || 'Piece principale');
  const dims = env.room?.dimensions_m || [0, 0, 0];
  setText('room-size', `${dims.map((v) => Number(v).toFixed(1)).join(' x ')} m`);

  const aps = Array.isArray(env.access_points) ? env.access_points : [];
  const apList = $('#ap-list');
  setText('ap-count', `${aps.length} AP`);
  apList?.replaceChildren();
  for (const ap of aps) {
    const row = document.createElement('article');
    row.className = `device-row ${ap.active ? 'device-active' : 'device-offline'}`;
    row.innerHTML = `
      <div>
        <strong>${ap.label || ap.ap_id}</strong>
        <span>${ap.ssid || 'SSID non renseigne'} / ${ap.role || 'ancre mesh'}</span>
      </div>
      <div class="device-meta">
        <span>${ap.band || '2.4GHz'}</span>
        <span>ch ${ap.channel ?? '-'}</span>
      </div>
    `;
    apList?.append(row);
  }

  renderStage(env);
  renderEnvironmentForm(env);
  updateChecklist();
}

function renderEnvironmentUnavailable() {
  setText('room-name', 'Configuration indisponible');
  setText('room-size', '--');
  setText('ap-count', '0 AP');
  $('#ap-list')?.replaceChildren(emptyRow('API environnement indisponible'));
  $('#environment-stage')?.replaceChildren(emptyRow(state.environmentError || 'Configuration indisponible'));
}

function renderStage(env) {
  const stage = $('#environment-stage');
  if (!stage) return;
  stage.replaceChildren();

  const dims = env.room?.dimensions_m || [5.2, 2.6, 4.8];
  const width = Number(dims[0]) || 5.2;
  const depth = Number(dims[2]) || 4.8;
  const toPct = ([x, , z]) => ({
    left: `${((Number(x) / width) + 0.5) * 100}%`,
    top: `${((Number(z) / depth) + 0.5) * 100}%`,
  });

  const aps = env.access_points || [];
  const nodes = mergeConfiguredNodes(env.nodes || [], state.fleet?.nodes || []);
  const byAp = new Map(aps.map((ap) => [ap.ap_id, ap]));
  const byNode = new Map(nodes.map((node) => [Number(node.node_id), node]));

  for (const link of env.links || []) {
    const ap = byAp.get(link.ap_id);
    const node = byNode.get(Number(link.node_id));
    if (!ap || !node) continue;
    const a = toPct(ap.position_m || [0, 0, 0]);
    const b = toPct(node.position_m || node.position || [0, 0, 0]);
    const line = document.createElement('div');
    const status = String(node.status || (node.active ? 'active' : 'offline')).toLowerCase();
    const active = Boolean(node.active) && !['offline', 'stale'].includes(status);
    line.className = `rf-link ${active ? 'rf-link-live' : 'rf-link-offline'}`;
    const ax = parseFloat(a.left);
    const ay = parseFloat(a.top);
    const bx = parseFloat(b.left);
    const by = parseFloat(b.top);
    const dx = bx - ax;
    const dy = by - ay;
    line.style.left = a.left;
    line.style.top = a.top;
    line.style.width = `${Math.hypot(dx, dy)}%`;
    line.style.transform = `rotate(${Math.atan2(dy, dx)}rad)`;
    stage.append(line);
  }

  for (const ap of aps) {
    const marker = markerEl('ap', ap.label || ap.ap_id, ap.position_m);
    const p = toPct(ap.position_m || [0, 0, 0]);
    marker.style.left = p.left;
    marker.style.top = p.top;
    stage.append(marker);
  }

  for (const node of nodes) {
    const status = String(node.status || (node.active ? 'active' : 'offline')).toLowerCase();
    const marker = markerEl(`node node-${status}`, node.label || `C6-${node.node_id}`, node.position_m || node.position);
    const p = toPct(node.position_m || node.position || [0, 0, 0]);
    marker.style.left = p.left;
    marker.style.top = p.top;
    stage.append(marker);
  }
}

function markerEl(kind, label, position) {
  const marker = document.createElement('div');
  marker.className = `map-marker map-marker-${kind}`;
  marker.title = `${label} @ ${(position || [0, 0, 0]).join(', ')}`;
  marker.innerHTML = `<span></span><strong>${label}</strong>`;
  return marker;
}

function mergeConfiguredNodes(configured, live) {
  const liveById = new Map((live || []).map((node) => [Number(node.node_id), node]));
  const merged = configured.map((cfg) => ({ ...cfg, ...(liveById.get(Number(cfg.node_id)) || {}) }));
  for (const node of live || []) {
    if (!merged.some((item) => Number(item.node_id) === Number(node.node_id))) merged.push(node);
  }
  return merged;
}

function renderModules() {
  const modules = state.modules || [];
  const categories = ['All', ...new Set(modules.map((mod) => mod.category || 'General'))];
  const tabs = $('#module-tabs');
  tabs?.replaceChildren();
  for (const category of categories) {
    const button = document.createElement('button');
    button.type = 'button';
    button.className = category === state.selectedCategory ? 'active' : '';
    button.textContent = category;
    button.addEventListener('click', () => {
      state.selectedCategory = category;
      renderModules();
    });
    tabs?.append(button);
  }

  const active = modules.filter((mod) => mod.status === 'active').length;
  const available = modules.filter((mod) => mod.status === 'available').length;
  setText('module-summary', `${active} active / ${available} available`);

  const list = $('#module-list');
  list?.replaceChildren();
  const filtered = modules.filter((mod) => state.selectedCategory === 'All' || mod.category === state.selectedCategory);
  for (const mod of filtered) {
    const row = document.createElement('article');
    const status = String(mod.status || 'offline').toLowerCase();
    row.className = `module-row module-${status}`;
    row.innerHTML = `
      <div>
        <strong>${mod.name || mod.id}</strong>
        <span>${mod.category || 'General'} / ${mod.size_kb || 0} KB / ${mod.required_nodes || 1} nodes</span>
      </div>
      <div class="module-state">
        <span>${status}</span>
        <span>${percent(mod.confidence)}</span>
      </div>
    `;
    list?.append(row);
  }
}

function renderLatest() {
  const latest = state.latest;
  const cls = latest?.classification || {};
  setText('presence-state', cls.presence ? 'present' : 'absent');
  setText('person-count', String(latest?.estimated_persons || 0));
  setText('motion-level', cls.motion_level || 'unknown');
}

function renderCalibration(data) {
  state.calibration = data;
  const status = data?.status || data?.calibration?.status || 'unknown';
  setText('calibration-status', status === 'unknown' ? 'inconnue' : status);
  const start = $('#start-calibration');
  if (start) {
    const ready = Boolean(state.fleet?.ready);
    start.disabled = !ready;
    start.title = ready ? 'Lancer la baseline piece vide' : 'Attendre les 3 noeuds actifs avant la calibration';
  }
  updateChecklist();
}

function emptyRow(text) {
  const row = document.createElement('div');
  row.className = 'empty-row';
  row.textContent = text;
  return row;
}

function renderEnvironmentForm(env) {
  const form = $('#environment-form');
  if (!form || form.dataset.loaded === 'true') return;
  form.dataset.loaded = 'true';

  form.elements['room.name'].value = env.room?.name || 'primary';
  form.elements['room.x'].value = env.room?.dimensions_m?.[0] ?? 5.2;
  form.elements['room.y'].value = env.room?.dimensions_m?.[1] ?? 2.6;
  form.elements['room.z'].value = env.room?.dimensions_m?.[2] ?? 4.8;

  const fields = $('#environment-fields');
  fields?.replaceChildren();
  fields?.append(sectionEditor('AP mesh', env.access_points || [], 'ap'));
  fields?.append(sectionEditor('Noeuds ESP32-C6', env.nodes || [], 'node'));
}

function sectionEditor(title, items, type) {
  const section = document.createElement('section');
  section.className = 'editor-section';
  const heading = document.createElement('h3');
  heading.textContent = title;
  section.append(heading);
  for (const item of items) {
    const id = type === 'ap' ? item.ap_id : item.node_id;
    const row = document.createElement('div');
    row.className = 'position-row';
    row.dataset.type = type;
    row.dataset.id = id;
    row.innerHTML = `
      <strong>${item.label || id}</strong>
      <label>Gauche / droite<input data-axis="0" type="number" step="0.1" value="${item.position_m?.[0] ?? 0}"></label>
      <label>Hauteur<input data-axis="1" type="number" step="0.1" value="${item.position_m?.[1] ?? 0}"></label>
      <label>Avant / arriere<input data-axis="2" type="number" step="0.1" value="${item.position_m?.[2] ?? 0}"></label>
    `;
    section.append(row);
  }
  return section;
}

function readEnvironmentForm() {
  const env = structuredClone(state.environment || {});
  const form = $('#environment-form');
  env.room = env.room || {};
  env.room.name = form.elements['room.name'].value.trim() || 'primary';
  env.room.dimensions_m = [
    Number(form.elements['room.x'].value) || 5.2,
    Number(form.elements['room.y'].value) || 2.6,
    Number(form.elements['room.z'].value) || 4.8,
  ];

  for (const row of document.querySelectorAll('.position-row')) {
    const values = [...row.querySelectorAll('input')].map((input) => Number(input.value) || 0);
    if (row.dataset.type === 'ap') {
      const ap = env.access_points?.find((item) => item.ap_id === row.dataset.id);
      if (ap) ap.position_m = values;
    } else {
      const node = env.nodes?.find((item) => String(item.node_id) === String(row.dataset.id));
      if (node) node.position_m = values;
    }
  }
  return env;
}

async function refreshRest() {
  const [fleetResult, environmentResult, modulesResult, calibrationResult] = await Promise.allSettled([
      fetchJson('/api/v1/fleet'),
      fetchJson('/api/v1/environment'),
      fetchJson('/api/v1/modules'),
      fetchJson('/api/v1/calibration').catch(() => null),
  ]);

  if (fleetResult.status === 'fulfilled') {
    state.fleet = fleetResult.value;
    renderFleet();
  } else {
    state.fleet = null;
    setMasterStatus('offline', 'Master non joignable');
    logEvent(fleetResult.reason.message, 'warn');
    renderFleet();
  }

  if (environmentResult.status === 'fulfilled') {
    state.environment = environmentResult.value;
    state.environmentError = null;
  } else {
    state.environment = null;
    state.environmentError = environmentResult.reason.message;
    logEvent(environmentResult.reason.message, 'warn');
  }

  if (modulesResult.status === 'fulfilled') {
    state.modules = Array.isArray(modulesResult.value.modules) ? modulesResult.value.modules : [];
  }

  renderEnvironment();
  renderModules();
  renderCalibration(calibrationResult.status === 'fulfilled' ? calibrationResult.value : null);
}

function connectWs() {
  if (state.ws && [WebSocket.OPEN, WebSocket.CONNECTING].includes(state.ws.readyState)) return;
  const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
  const url = `${protocol}//${window.location.host}/ws/sensing`;
  const ws = new WebSocket(url);
  state.ws = ws;

  ws.onopen = () => {
    logEvent('Flux CSI connecte');
    if (state.reconnectTimer) clearTimeout(state.reconnectTimer);
  };
  ws.onmessage = (event) => {
    try {
      state.latest = JSON.parse(event.data);
      if (Array.isArray(state.latest.nodes) && state.fleet) {
        state.fleet.nodes = mergeConfiguredNodes(state.environment?.nodes || [], state.latest.nodes);
      }
      renderLatest();
      renderFleet();
      renderEnvironment();
    } catch (error) {
      logEvent(`Bad sensing frame: ${error.message}`, 'warn');
    }
  };
  ws.onclose = () => {
    state.ws = null;
    setMasterStatus('offline', 'Flux CSI hors ligne');
    logEvent('Flux CSI deconnecte', 'warn');
    state.reconnectTimer = setTimeout(connectWs, 3000);
  };
  ws.onerror = () => {
    ws.close();
  };
}

function bindActions() {
  $('#clear-log')?.addEventListener('click', () => $('#event-log')?.replaceChildren());
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
  $('#environment-form')?.addEventListener('submit', async (event) => {
    event.preventDefault();
    setText('save-status', 'saving');
    try {
      const environment = readEnvironmentForm();
      const saved = await fetchJson('/api/v1/environment', {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(environment),
      });
      state.environment = saved.environment;
      setText('save-status', 'saved');
      logEvent('Environment saved');
      renderEnvironment();
    } catch (error) {
      setText('save-status', 'error');
      logEvent(error.message, 'warn');
    }
  });
}

async function init() {
  bindActions();
  await refreshRest();
  connectWs();
  setInterval(refreshRest, 5000);
}

init();

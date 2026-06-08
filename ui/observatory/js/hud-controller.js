/**
 * HudController — Extracted HUD update, settings dialog, and scenario UI
 *
 * Manages all DOM-based HUD elements:
 * - Vital sign display with smooth lerp transitions and color coding
 * - Signal metrics, sparkline, and presence indicator
 * - Scenario description and edge module badges
 * - Mini person-count dot visualization
 * - Settings dialog (tabs, ranges, presets, data source)
 * - Quick-select scenario dropdown
 */

// ---- Constants ----

export const DEFAULTS = {
  bloom: 0.08, bloomRadius: 0.2, bloomThresh: 0.6,
  exposure: 1.3, vignette: 0.25, grain: 0.01, chromatic: 0.0005,
  boneThick: 0.018, jointSize: 0.035, glow: 0.3, trail: 0.35,
  wireColor: '#00d878', jointColor: '#ff4060', aura: 0.02,
  field: 0.45, waves: 0.4, ambient: 0.7, reflect: 0.2,
  fov: 50, orbitSpeed: 0.15, grid: true, room: true,
  dataSource: 'ws', wsUrl: '',
};

export const SETTINGS_VERSION = '7';

export const PRESETS = {
  foundation: {},
  cinematic: {
    bloom: 1.2, bloomRadius: 0.5, bloomThresh: 0.2,
    exposure: 0.8, vignette: 0.7, grain: 0.04, chromatic: 0.002,
    glow: 0.6, trail: 0.8, aura: 0.06, field: 0.4,
    waves: 0.7, ambient: 0.25, reflect: 0.5, fov: 40, orbitSpeed: 0.08,
  },
  minimal: {
    bloom: 0.3, bloomRadius: 0.2, bloomThresh: 0.5,
    exposure: 1.1, vignette: 0.2, grain: 0, chromatic: 0,
    glow: 0.3, trail: 0.2, aura: 0.02, field: 0.7,
    waves: 0.3, ambient: 0.6, reflect: 0.1, wireColor: '#40ff90', jointColor: '#4080ff',
  },
  neon: {
    bloom: 2.5, bloomRadius: 0.8, bloomThresh: 0.1,
    exposure: 0.6, vignette: 0.6, grain: 0.02, chromatic: 0.004,
    glow: 2.0, trail: 1.0, aura: 0.15, field: 0.6,
    waves: 1.0, ambient: 0.15, reflect: 0.7, wireColor: '#00ffaa', jointColor: '#ff00ff',
  },
  tactical: {
    bloom: 0.5, bloomRadius: 0.3, bloomThresh: 0.4,
    exposure: 0.85, vignette: 0.4, grain: 0.04, chromatic: 0.001,
    glow: 0.5, trail: 0.4, aura: 0.03, field: 0.8,
    waves: 0.4, ambient: 0.3, reflect: 0.15, wireColor: '#30ff60', jointColor: '#ff8800',
  },
  medical: {
    bloom: 0.6, bloomRadius: 0.4, bloomThresh: 0.35,
    exposure: 1.0, vignette: 0.3, grain: 0.01, chromatic: 0.0005,
    glow: 0.6, trail: 0.3, aura: 0.04, field: 0.5,
    waves: 0.3, ambient: 0.5, reflect: 0.2, wireColor: '#00ccff', jointColor: '#ff3355',
  },
};

// Vital-sign color-coding thresholds
function vitalColor(type, value) {
  if (value <= 0) return 'var(--text-secondary)';
  if (type === 'hr') {
    if (value < 50 || value > 130) return 'var(--red-alert)';
    if (value < 60 || value > 100) return 'var(--amber)';
    return 'var(--green-glow)';
  }
  if (type === 'br') {
    if (value < 8 || value > 28) return 'var(--red-alert)';
    if (value < 12 || value > 20) return 'var(--amber)';
    return 'var(--green-glow)';
  }
  if (type === 'conf') {
    if (value < 40) return 'var(--red-alert)';
    if (value < 70) return 'var(--amber)';
    return 'var(--green-glow)';
  }
  return 'var(--text-primary)';
}

function lerp(a, b, t) {
  return a + (b - a) * t;
}

// ---- HudController class ----

export class HudController {
  constructor(observatory) {
    this._obs = observatory;
    this._settingsOpen = false;
    this._rssiHistory = [];
    this._sparklineCtx = document.getElementById('rssi-sparkline')?.getContext('2d');

    // Lerp state for smooth vital-sign transitions
    this._lerpHr = 0;
    this._lerpBr = 0;
    this._lerpConf = 0;

    // Operational RuvSense Edge panels
    this._opsFetchAt = 0;
    this._opsFetchPending = false;
    this._fleet = null;
    this._modules = null;

    this._initMobileTabs();
  }

  // ============================================================
  // Settings dialog
  // ============================================================

  initSettings() {
    const overlay = document.getElementById('settings-overlay');
    const btn = document.getElementById('settings-btn');
    const closeBtn = document.getElementById('settings-close');
    btn.addEventListener('click', () => this.toggleSettings());
    closeBtn.addEventListener('click', () => this.toggleSettings());
    overlay.addEventListener('click', (e) => { if (e.target === overlay) this.toggleSettings(); });

    // Tab switching
    document.querySelectorAll('.stab').forEach(tab => {
      tab.addEventListener('click', () => {
        document.querySelectorAll('.stab').forEach(t => t.classList.remove('active'));
        document.querySelectorAll('.stab-content').forEach(c => c.classList.remove('active'));
        tab.classList.add('active');
        document.getElementById(`stab-${tab.dataset.stab}`).classList.add('active');
      });
    });

    const obs = this._obs;
    const s = obs.settings;

    // Bind ranges
    this._bindRange('opt-bloom', 'bloom', v => { obs._postProcessing._bloomPass.strength = v; });
    this._bindRange('opt-bloom-radius', 'bloomRadius', v => { obs._postProcessing._bloomPass.radius = v; });
    this._bindRange('opt-bloom-thresh', 'bloomThresh', v => { obs._postProcessing._bloomPass.threshold = v; });
    this._bindRange('opt-exposure', 'exposure', v => { obs._renderer.toneMappingExposure = v; });
    this._bindRange('opt-vignette', 'vignette', v => { obs._postProcessing._vignettePass.uniforms.uVignetteStrength.value = v; });
    this._bindRange('opt-grain', 'grain', v => { obs._postProcessing._vignettePass.uniforms.uGrainStrength.value = v; });
    this._bindRange('opt-chromatic', 'chromatic', v => { obs._postProcessing._vignettePass.uniforms.uChromaticStrength.value = v; });
    this._bindRange('opt-bone-thick', 'boneThick');
    this._bindRange('opt-joint-size', 'jointSize');
    this._bindRange('opt-glow', 'glow');
    this._bindRange('opt-trail', 'trail');
    this._bindRange('opt-aura', 'aura');
    this._bindRange('opt-field', 'field', v => { obs._fieldMat.opacity = v; });
    this._bindRange('opt-waves', 'waves');
    this._bindRange('opt-ambient', 'ambient', v => { obs._ambient.intensity = v * 5.0; });
    this._bindRange('opt-reflect', 'reflect', v => {
      obs._floorMat.roughness = 1.0 - v * 0.7;
      obs._floorMat.metalness = v * 0.5;
    });
    this._bindRange('opt-fov', 'fov', v => {
      obs._camera.fov = v;
      obs._camera.updateProjectionMatrix();
    });
    this._bindRange('opt-orbit-speed', 'orbitSpeed');

    // Color pickers
    document.getElementById('opt-wire-color').value = s.wireColor;
    document.getElementById('opt-wire-color').addEventListener('input', (e) => {
      s.wireColor = e.target.value; obs._applyColors(); this.saveSettings();
    });
    document.getElementById('opt-joint-color').value = s.jointColor;
    document.getElementById('opt-joint-color').addEventListener('input', (e) => {
      s.jointColor = e.target.value; obs._applyColors(); this.saveSettings();
    });

    // Checkboxes
    document.getElementById('opt-grid').checked = s.grid;
    document.getElementById('opt-grid').addEventListener('change', (e) => {
      s.grid = e.target.checked; obs._grid.visible = e.target.checked; this.saveSettings();
    });
    document.getElementById('opt-room').checked = s.room;
    document.getElementById('opt-room').addEventListener('change', (e) => {
      s.room = e.target.checked; obs._roomWire.visible = e.target.checked; this.saveSettings();
    });

    // Buttons
    document.getElementById('btn-reset-camera').addEventListener('click', () => {
      obs._frameCameraToSensors(true);
    });
    document.getElementById('btn-export-settings').addEventListener('click', () => {
      const blob = new Blob([JSON.stringify(s, null, 2)], { type: 'application/json' });
      const a = document.createElement('a');
      a.href = URL.createObjectURL(blob);
      a.download = 'ruvsense-console-settings.json';
      a.click();
    });
    document.getElementById('btn-reset-settings').addEventListener('click', () => {
      this.applyPreset(DEFAULTS);
    });

    const presetSel = document.getElementById('opt-preset');
    presetSel.addEventListener('change', (e) => {
      const p = PRESETS[e.target.value];
      if (p) this.applyPreset({ ...DEFAULTS, ...p });
    });

    obs._grid.visible = s.grid;
    obs._roomWire.visible = s.room;
  }

  // ============================================================
  // Quick-select (top bar scenario dropdown)
  // ============================================================

  initQuickSelect() {
    // Live-only console: no scenario selector in production.
  }

  _initMobileTabs() {
    const tabs = document.querySelectorAll('.mobile-hud-tab');
    if (!tabs.length) return;
    tabs.forEach((tab) => {
      tab.addEventListener('click', () => this._activateMobilePanel(tab.dataset.panel));
    });
  }

  _activateMobilePanel(panelId) {
    if (!panelId) return;
    document.querySelectorAll('.mobile-hud-tab').forEach((tab) => {
      const active = tab.dataset.panel === panelId;
      tab.classList.toggle('active', active);
      tab.setAttribute('aria-selected', active ? 'true' : 'false');
    });
    document.querySelectorAll('#hud-panel-layout .data-panel').forEach((panel) => {
      panel.classList.toggle('mobile-active', panel.id === panelId);
    });
  }

  // ============================================================
  // Toggle / save / preset
  // ============================================================

  toggleSettings() {
    this._settingsOpen = !this._settingsOpen;
    document.getElementById('settings-overlay').style.display = this._settingsOpen ? 'flex' : 'none';
  }

  get settingsOpen() {
    return this._settingsOpen;
  }

  saveSettings() {
    try {
    localStorage.setItem('ruvsense-console-settings', JSON.stringify(this._obs.settings));
    } catch {}
  }

  applyPreset(preset) {
    const obs = this._obs;
    Object.assign(obs.settings, preset);
    this.saveSettings();
    const rangeMap = {
      'opt-bloom': 'bloom', 'opt-bloom-radius': 'bloomRadius', 'opt-bloom-thresh': 'bloomThresh',
      'opt-exposure': 'exposure', 'opt-vignette': 'vignette', 'opt-grain': 'grain', 'opt-chromatic': 'chromatic',
      'opt-bone-thick': 'boneThick', 'opt-joint-size': 'jointSize', 'opt-glow': 'glow', 'opt-trail': 'trail', 'opt-aura': 'aura',
      'opt-field': 'field', 'opt-waves': 'waves', 'opt-ambient': 'ambient', 'opt-reflect': 'reflect',
      'opt-fov': 'fov', 'opt-orbit-speed': 'orbitSpeed',
    };
    for (const [id, key] of Object.entries(rangeMap)) {
      const el = document.getElementById(id);
      const valEl = document.getElementById(`${id}-val`);
      if (el) el.value = obs.settings[key];
      if (valEl) valEl.textContent = obs.settings[key];
    }
    const gridEl = document.getElementById('opt-grid');
    if (gridEl) { gridEl.checked = obs.settings.grid; obs._grid.visible = obs.settings.grid; }
    const roomEl = document.getElementById('opt-room');
    if (roomEl) { roomEl.checked = obs.settings.room; obs._roomWire.visible = obs.settings.room; }
    document.getElementById('opt-wire-color').value = obs.settings.wireColor;
    document.getElementById('opt-joint-color').value = obs.settings.jointColor;
    obs._applyPostSettings();
    obs._renderer.toneMappingExposure = obs.settings.exposure;
    obs._fieldMat.opacity = obs.settings.field;
    obs._ambient.intensity = obs.settings.ambient * 5.0;
    obs._floorMat.roughness = 1.0 - obs.settings.reflect * 0.7;
    obs._floorMat.metalness = obs.settings.reflect * 0.5;
    obs._camera.fov = obs.settings.fov;
    obs._camera.updateProjectionMatrix();
    obs._applyColors();
  }

  // ============================================================
  // Source badge
  // ============================================================

  updateSourceBadge(status, ws) {
    const dot = document.querySelector('#data-source-badge .dot');
    const label = document.getElementById('data-source-label');
    if (status === 'live') {
      dot.className = 'dot dot--live'; label.textContent = 'LIVE';
    } else if (status === 'degraded') {
      dot.className = 'dot dot--degraded'; label.textContent = 'DEGRADED';
    } else {
      dot.className = 'dot dot--offline'; label.textContent = 'OFFLINE';
    }
  }

  // ============================================================
  // HUD update (called every frame)
  // ============================================================

  updateHUD(data) {
    if (!data) return;
    const vs = data.vital_signs || {};
    const feat = data.features || {};
    const cls = data.classification || {};

    const targetHr = vs.heart_rate_bpm || 0;
    const targetBr = vs.breathing_rate_bpm || 0;
    const targetConf = Math.round((cls.confidence || 0) * 100);

    // Smooth lerp transitions (blend 4% per frame toward target — very stable)
    const lerpFactor = 0.04;
    this._lerpHr = targetHr > 0 ? lerp(this._lerpHr, targetHr, lerpFactor) : 0;
    this._lerpBr = targetBr > 0 ? lerp(this._lerpBr, targetBr, lerpFactor) : 0;
    this._lerpConf = targetConf > 0 ? lerp(this._lerpConf, targetConf, lerpFactor) : 0;

    const dispHr = this._lerpHr > 1 ? Math.round(this._lerpHr) : '--';
    const dispBr = this._lerpBr > 1 ? Math.round(this._lerpBr) : '--';
    const dispConf = this._lerpConf > 1 ? Math.round(this._lerpConf) : '--';

    this._setText('hr-value', dispHr);
    this._setText('br-value', dispBr);
    this._setText('conf-value', dispConf);
    this._setWidth('hr-bar', Math.min(100, this._lerpHr / 120 * 100));
    this._setWidth('br-bar', Math.min(100, this._lerpBr / 30 * 100));
    this._setWidth('conf-bar', this._lerpConf);

    // Color-code vital values
    this._setColor('hr-value', vitalColor('hr', this._lerpHr));
    this._setColor('br-value', vitalColor('br', this._lerpBr));
    this._setColor('conf-value', vitalColor('conf', this._lerpConf));

    // Color-code bar fills to match
    this._setBarColor('hr-bar', vitalColor('hr', this._lerpHr));
    this._setBarColor('br-bar', vitalColor('br', this._lerpBr));
    this._setBarColor('conf-bar', vitalColor('conf', this._lerpConf));

    this._setText('rssi-value', `${Math.round(feat.mean_rssi || 0)} dBm`);
    this._setText('var-value', (feat.variance || 0).toFixed(2));
    this._setText('motion-value', (feat.motion_band_power || 0).toFixed(3));

    // Mini person-count dots. Prefer the conservative rendered count when the
    // backend provides anti-duplicate evidence.
    const evidence = data.count_evidence && typeof data.count_evidence === 'object' ? data.count_evidence : null;
    const renderedCount = Number(evidence?.rendered_persons ?? data.estimated_persons ?? 0);
    const rawCount = Number(evidence?.raw_estimated_persons ?? data.estimated_persons ?? renderedCount);
    const fallbackCount = Array.isArray(data.persons) ? data.persons.length : 0;
    const personCount = Math.max(0, Number.isFinite(renderedCount) ? renderedCount : fallbackCount);
    const ambiguous = Boolean(evidence?.ambiguous || rawCount > personCount);
    this._updatePersonDots(personCount, {
      ambiguous,
      label: ambiguous && rawCount > personCount ? `${personCount} / raw ${rawCount}` : String(personCount),
      title: ambiguous
        ? `Ambiguous multipath: raw ${rawCount}, rendered ${personCount}`
        : `Rendered persons: ${personCount}`,
    });

    const presEl = document.getElementById('presence-indicator');
    const presLabel = document.getElementById('presence-label');
    if (presEl) {
      const ml = cls.motion_level || 'absent';
      presEl.className = 'presence-state';
      if (ml === 'active') { presEl.classList.add('presence--active'); presLabel.textContent = 'ACTIVE'; }
      else if (cls.presence) { presEl.classList.add('presence--present'); presLabel.textContent = 'PRESENT'; }
      else { presEl.classList.add('presence--absent'); presLabel.textContent = 'ABSENT'; }
    }

    const fallEl = document.getElementById('fall-alert');
    if (fallEl) fallEl.style.display = cls.fall_detected ? 'block' : 'none';

    this._updateFleetFromData(data);
    this._refreshOperationalData();
  }

  // ============================================================
  // Sparkline
  // ============================================================

  updateSparkline(data) {
    const rssi = data?.features?.mean_rssi;
    if (rssi == null || !this._sparklineCtx) return;
    this._rssiHistory.push(rssi);
    if (this._rssiHistory.length > 60) this._rssiHistory.shift();

    const ctx = this._sparklineCtx;
    const w = ctx.canvas.width, h = ctx.canvas.height;
    ctx.clearRect(0, 0, w, h);
    if (this._rssiHistory.length < 2) return;

    ctx.beginPath();
    ctx.strokeStyle = '#2090ff';
    ctx.lineWidth = 1.5;
    ctx.shadowColor = '#2090ff';
    ctx.shadowBlur = 4;
    for (let i = 0; i < this._rssiHistory.length; i++) {
      const x = (i / (this._rssiHistory.length - 1)) * w;
      const norm = Math.max(0, Math.min(1, (this._rssiHistory[i] + 80) / 60));
      const y = h - norm * h;
      i === 0 ? ctx.moveTo(x, y) : ctx.lineTo(x, y);
    }
    ctx.stroke();
    ctx.shadowBlur = 0;
    ctx.lineTo(w, h);
    ctx.lineTo(0, h);
    ctx.closePath();
    const grad = ctx.createLinearGradient(0, 0, 0, h);
    grad.addColorStop(0, 'rgba(32,144,255,0.15)');
    grad.addColorStop(1, 'rgba(32,144,255,0)');
    ctx.fillStyle = grad;
    ctx.fill();
  }

  // ============================================================
  // Private helpers
  // ============================================================

  _setText(id, val) {
    const e = document.getElementById(id);
    if (e) e.textContent = val;
  }

  _setWidth(id, pct) {
    const e = document.getElementById(id);
    if (e) e.style.width = `${pct}%`;
  }

  _setColor(id, color) {
    const e = document.getElementById(id);
    if (e) e.style.color = color;
  }

  _setBarColor(id, color) {
    const e = document.getElementById(id);
    if (e) e.style.background = color;
  }

  async _refreshOperationalData() {
    const now = Date.now();
    if (this._opsFetchPending || now - this._opsFetchAt < 3000) return;
    this._opsFetchAt = now;
    this._opsFetchPending = true;

    try {
      const [fleetResp, modulesResp] = await Promise.all([
        fetch('/api/v1/fleet', { cache: 'no-store' }),
        fetch('/api/v1/modules', { cache: 'no-store' }),
      ]);

      if (fleetResp.ok) {
        this._fleet = await fleetResp.json();
        this._renderFleetPanel(this._fleet);
      }
      if (modulesResp.ok) {
        this._modules = await modulesResp.json();
        this._renderModulesPanel(this._modules);
      }
    } catch {
      this.updateSourceBadge('offline', null);
    } finally {
      this._opsFetchPending = false;
    }
  }

  _updateFleetFromData(data) {
    const nodes = Array.isArray(data?.nodes) ? data.nodes : [];
    if (!nodes.length) return;
    if (this._fleet && Date.now() - this._opsFetchAt < 4500) return;

    const activeNodes = nodes.filter(n => {
      const status = String(n.status || '').toLowerCase();
      return n.active !== false && status !== 'stale' && status !== 'offline';
    }).length;

    this._renderFleetPanel({
      source: 'ws',
      active_nodes: activeNodes,
      min_nodes: 3,
      ready: activeNodes >= 3,
      nodes,
    });
  }

  _renderFleetPanel(fleet) {
    const active = Number(fleet?.active_nodes ?? 0);
    const minNodes = Number(fleet?.min_nodes ?? 3);
    const ready = Boolean(fleet?.ready || active >= minNodes);
    this.updateSourceBadge(ready ? 'live' : active > 0 ? 'degraded' : 'offline', this._obs._ws);
    const readyEl = document.getElementById('fleet-ready');
    if (readyEl) {
      readyEl.textContent = ready ? 'Ready for fusion' : 'Waiting for quorum';
      readyEl.className = `fleet-readiness ${ready ? 'fleet-readiness--ready' : 'fleet-readiness--pending'}`;
    }

    this._setText('fleet-active', String(active));
    this._setText('fleet-min', String(minNodes));
    this._setText('fleet-source', String(fleet?.source || 'live'));

    const list = document.getElementById('fleet-list');
    if (!list) return;
    const nodes = Array.isArray(fleet?.nodes) ? [...fleet.nodes] : [];
    nodes.sort((a, b) => Number(a.node_id || 0) - Number(b.node_id || 0));

    list.replaceChildren();
    if (!nodes.length) {
      const empty = document.createElement('div');
      empty.className = 'fleet-empty';
      empty.textContent = 'No ESP32-C6 nodes seen yet';
      list.appendChild(empty);
      return;
    }

    for (const node of nodes.slice(0, 8)) {
      const status = String(node.health_status || node.status || (node.active ? 'live' : 'stale')).toLowerCase();
      const row = document.createElement('div');
      row.className = `fleet-node fleet-node--${status}`;

      const name = document.createElement('span');
      name.className = 'fleet-node-name';
      name.textContent = node.display_label || node.label || `ESP32-C6 #${node.node_id ?? '?'}`;

      const meta = document.createElement('span');
      meta.className = 'fleet-node-meta';
      const lastSeen = Number(node.last_seen_ms);
      const age = Number.isFinite(lastSeen)
        ? (lastSeen < 1000 ? `${Math.round(lastSeen)}ms` : `${(lastSeen / 1000).toFixed(1)}s`)
        : 'never';
      const fps = Number(node.frame_rate_hz || 0).toFixed(1);
      meta.textContent = `${status} / ${fps} Hz / ${age}`;

      row.append(name, meta);
      list.appendChild(row);
    }
  }

  _renderModulesPanel(catalog) {
    const list = document.getElementById('module-list');
    const summary = document.getElementById('module-summary');
    const modules = Array.isArray(catalog?.modules) ? catalog.modules : [];
    if (!list || !summary || !modules.length) return;

    const active = modules.filter(m => {
      const status = String(m.effective_status || m.status || '').toLowerCase();
      return status === 'active' || status === 'live';
    }).length;
    const enabled = modules.filter(m => m.enabled !== false).length;
    const categories = new Set(modules.map(m => m.category)).size;
    summary.textContent = `${modules.length} modules / ${enabled} enabled / ${active} active / ${categories} categories`;

    const rank = { active: 0, live: 0, available: 1, disabled: 2, offline: 3 };
    const ordered = [...modules].sort((a, b) => {
      const byStatus = (rank[a.effective_status || a.status] ?? 9) - (rank[b.effective_status || b.status] ?? 9);
      if (byStatus !== 0) return byStatus;
      return String(a.category).localeCompare(String(b.category));
    });

    list.replaceChildren();
    for (const mod of ordered) {
      const row = document.createElement('div');
      const status = String(mod.effective_status || mod.status || 'offline').toLowerCase();
      row.className = `module-row module-row--${status}`;

      const copy = document.createElement('div');
      copy.className = 'module-copy';
      const name = document.createElement('span');
      name.className = 'module-name';
      name.textContent = mod.name || mod.id || 'Module';
      const category = document.createElement('span');
      category.className = 'module-category';
      category.textContent = `${mod.category || 'General'} / ${mod.size_kb || 0} KB`;
      copy.append(name, category);

      const state = document.createElement('div');
      state.className = 'module-state';
      const statusEl = document.createElement('span');
      statusEl.textContent = status;
      const confEl = document.createElement('span');
      confEl.textContent = this._formatPercent(mod.confidence);
      state.append(statusEl, confEl);

      row.append(copy, state);
      list.appendChild(row);
    }
  }

  _formatPercent(value) {
    const n = Number(value);
    return Number.isFinite(n) ? `${Math.round(n * 100)}%` : '--';
  }

  _bindRange(id, key, applyFn) {
    const el = document.getElementById(id);
    const valEl = document.getElementById(`${id}-val`);
    if (!el) return;
    el.value = this._obs.settings[key];
    if (valEl) valEl.textContent = this._obs.settings[key];
    el.addEventListener('input', (e) => {
      const v = parseFloat(e.target.value);
      this._obs.settings[key] = v;
      if (valEl) valEl.textContent = v;
      if (applyFn) applyFn(v);
      this.saveSettings();
    });
  }

  _updatePersonDots(count, options = {}) {
    const container = document.getElementById('persons-dots');
    if (!container) {
      // Fall back to text-only display
      this._setText('persons-value', options.label ?? count);
      return;
    }
    container.classList.toggle('persons-dots--ambiguous', Boolean(options.ambiguous));
    if (options.title) container.title = options.title;
    // Build dot icons: filled for detected persons, dim for empty slots (max 8)
    const maxDots = 8;
    const clamped = Math.min(count, maxDots);
    let html = '';
    for (let i = 0; i < maxDots; i++) {
      const active = i < clamped;
      html += `<span class="person-dot${active ? ' person-dot--active' : ''}"></span>`;
    }
    container.innerHTML = html;
    const value = document.getElementById('persons-value');
    if (value) {
      value.classList.toggle('ambiguous-count', Boolean(options.ambiguous));
      value.title = options.title || '';
      value.textContent = options.label ?? count;
    }
  }

}

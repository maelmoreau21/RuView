(function () {
  const WS_URL = 'ws://localhost:3000/ws/pose';
  const RECONNECT_MS = 3000;
  let socket = null;
  let reconnectTimer = null;
  let probePending = false;
  let stopped = false;
  let configChannel = null;

  function ensureState() {
    if (!window.RS) {
      window.RS = {
        connected: false,
        nodes: {},
        persons: [],
        alerts: [],
        vitals_history: {},
        breathing_history: [],
        frame_count: 0,
        server_version: '--',
        uptime: '--',
        room_config: null,
      };
    }
    return window.RS;
  }

  function dispatchUpdate() {
    if (typeof window.dispatchEvent === 'function' && typeof window.CustomEvent === 'function') {
      window.dispatchEvent(new window.CustomEvent('rs:update'));
    }
  }

  function isFileMode() {
    return window.location.protocol === 'file:';
  }

  function probeUrl() {
    return WS_URL.replace(/^ws:/, 'http:').replace(/\/ws\/pose$/, '/api/v1/version');
  }

  async function probeBackend() {
    if (typeof fetch !== 'function' || typeof AbortController === 'undefined') return true;
    const controller = new AbortController();
    const timeout = window.setTimeout(() => controller.abort(), 1200);
    try {
      await fetch(probeUrl(), {
        cache: 'no-store',
        mode: 'no-cors',
        signal: controller.signal,
      });
      return true;
    } catch {
      return false;
    } finally {
      window.clearTimeout(timeout);
    }
  }

  function safeNumber(value) {
    const n = Number(value);
    return Number.isFinite(n) ? n : null;
  }

  function normalizeNodeKey(value) {
    return String(value ?? '').toLowerCase().replace(/[^a-z0-9]/g, '');
  }

  function normalizeTimestamp(value) {
    if (value === undefined || value === null || value === '') return Date.now();
    if (typeof value === 'number') {
      if (value > 100000000000) return value;
      if (value > 1000000000) return value * 1000;
      return Date.now() - value;
    }
    const parsed = Date.parse(value);
    return Number.isFinite(parsed) ? parsed : Date.now();
  }

  function normalizeNode(raw, fallbackId) {
    if (!raw || typeof raw !== 'object') return null;
    const id = raw.node_id ?? raw.id ?? raw.name ?? raw.label ?? fallbackId;
    const lastSeen = raw.last_seen ?? raw.lastSeen ?? raw.last_frame_at ?? raw.age_ms;
    return {
      id,
      node_id: raw.node_id ?? id,
      label: raw.label || raw.name || `Node ${id}`,
      rssi: safeNumber(raw.rssi ?? raw.rssi_dbm ?? raw.signal),
      last_seen: normalizeTimestamp(lastSeen),
      active: raw.active !== false && raw.status !== 'inactive' && raw.status !== 'offline',
      x: raw.x ?? raw.position?.x ?? raw.position_m?.[0],
      y: raw.y ?? raw.position?.y ?? raw.position_m?.[1],
    };
  }

  function updateNodes(message) {
    const state = ensureState();
    if (message.system_status === 'no_nodes') {
      state.nodes = {};
      return;
    }

    const rawNodes = message.nodes ?? message.node_status ?? message.nodeStatus;
    if (Array.isArray(rawNodes)) {
      const next = {};
      rawNodes.forEach((raw, index) => {
        const node = normalizeNode(raw, index + 1);
        if (!node) return;
        next[normalizeNodeKey(node.id || node.node_id || index + 1)] = node;
      });
      state.nodes = next;
      return;
    }

    if (rawNodes && typeof rawNodes === 'object') {
      const next = {};
      Object.entries(rawNodes).forEach(([key, raw]) => {
        const node = normalizeNode(raw, key);
        if (!node) return;
        next[normalizeNodeKey(key)] = node;
        next[normalizeNodeKey(node.id)] = node;
      });
      state.nodes = next;
      return;
    }

    if (message.node_id || message.rssi || message.rssi_dbm) {
      const node = normalizeNode(message, message.node_id || message.id || 'node');
      if (node) state.nodes[normalizeNodeKey(node.id || node.node_id)] = node;
    }
  }

  function normalizePerson(raw, index) {
    if (!raw || typeof raw !== 'object') return null;
    const x = safeNumber(raw.x ?? raw.position?.x ?? raw.location?.x ?? raw.centroid?.x);
    const y = safeNumber(raw.y ?? raw.position?.y ?? raw.location?.y ?? raw.centroid?.y);
    const confidence = safeNumber(raw.confidence ?? raw.score ?? raw.presence_confidence);
    const breathing = safeNumber(raw.breathing_bpm ?? raw.breathing_rate_bpm ?? raw.respiration_bpm ?? raw.vitals?.breathing_bpm);
    const motion = safeNumber(raw.motion_energy ?? raw.activity ?? raw.motion);
    return {
      id: raw.id ?? raw.person_id ?? raw.track_id ?? index + 1,
      x,
      y,
      confidence,
      breathing_bpm: breathing,
      motion_energy: motion,
    };
  }

  function updatePersons(message) {
    const state = ensureState();
    const rawPersons = message.persons ?? message.people ?? message.detections ?? message.poses;
    if (!Array.isArray(rawPersons)) return;
    state.persons = rawPersons.map(normalizePerson).filter(Boolean);
    state.persons.forEach((person) => {
      if (person.breathing_bpm === null || person.breathing_bpm === undefined) return;
      const id = String(person.id);
      const history = state.vitals_history[id] || [];
      history.push(person.breathing_bpm);
      state.vitals_history[id] = history.slice(-30);
      state.breathing_history.push({ time: Date.now(), value: person.breathing_bpm });
    });
    const cutoff = Date.now() - 10 * 60 * 1000;
    state.breathing_history = state.breathing_history.filter((point) => point.time >= cutoff).slice(-600);
  }

  function updateSystem(message) {
    const state = ensureState();
    if (message.frame_count !== undefined || message.csi_frame_count !== undefined || message.total_frames !== undefined) {
      state.frame_count = safeNumber(message.frame_count ?? message.csi_frame_count ?? message.total_frames) ?? state.frame_count;
    } else {
      state.frame_count += 1;
    }
    if (message.server_version || message.version) {
      state.server_version = message.server_version || message.version;
    }
    if (message.uptime || message.uptime_seconds !== undefined) {
      if (message.uptime) state.uptime = message.uptime;
      else {
        const seconds = safeNumber(message.uptime_seconds);
        if (seconds != null) state.uptime = formatUptime(seconds);
      }
    }
    if (message.room_config !== undefined) {
      state.room_config = message.room_config;
      state.room_config_source = 'websocket';
    }
  }

  function formatUptime(totalSeconds) {
    const seconds = Math.max(0, Math.floor(totalSeconds));
    const h = Math.floor(seconds / 3600);
    const m = Math.floor((seconds % 3600) / 60);
    if (h > 0) return `${h}h${String(m).padStart(2, '0')}`;
    return `${m}m`;
  }

  function updateAlerts(message) {
    const incoming = Array.isArray(message.alerts) ? message.alerts : [];
    incoming.forEach((alert) => {
      window.RSAlerts?.addAlert(
        alert.type || 'info',
        alert.message || alert.title || '--',
        alert.severity || 'info',
        alert,
      );
    });
  }

  function handleMessage(event) {
    let message = null;
    try {
      message = JSON.parse(event.data);
    } catch {
      return;
    }
    if (!message || typeof message !== 'object') return;

    ensureState().connected = true;
    updateNodes(message);
    updatePersons(message);
    updateSystem(message);
    updateAlerts(message);
    dispatchUpdate();
  }

  function scheduleReconnect() {
    if (stopped || reconnectTimer || isFileMode()) return;
    reconnectTimer = window.setTimeout(() => {
      reconnectTimer = null;
      connect();
    }, RECONNECT_MS);
  }

  function connect() {
    if (stopped || isFileMode() || socket) {
      dispatchUpdate();
      return;
    }
    ensureState();

    if (probePending) return;
    probePending = true;
    void probeBackend().then((reachable) => {
      probePending = false;
      if (stopped || socket) return;
      if (!reachable) {
        ensureState().connected = false;
        dispatchUpdate();
        scheduleReconnect();
        return;
      }
      openSocket();
    });
  }

  function openSocket() {
    try {
      socket = new WebSocket(WS_URL);
    } catch {
      socket = null;
      ensureState().connected = false;
      dispatchUpdate();
      scheduleReconnect();
      return;
    }

    socket.addEventListener('open', () => {
      ensureState().connected = true;
      dispatchUpdate();
    });

    socket.addEventListener('message', handleMessage);

    socket.addEventListener('close', () => {
      socket = null;
      ensureState().connected = false;
      dispatchUpdate();
      scheduleReconnect();
    });

    socket.addEventListener('error', () => {
      ensureState().connected = false;
      dispatchUpdate();
    });
  }

  function disconnect() {
    stopped = true;
    if (reconnectTimer) window.clearTimeout(reconnectTimer);
    reconnectTimer = null;
    if (socket) socket.close();
    socket = null;
    ensureState().connected = false;
    dispatchUpdate();
  }

  async function reloadRoomConfig() {
    if (typeof window.loadRoomConfig === 'function') {
      await window.loadRoomConfig();
      return;
    }
    if (window.RSApi && typeof window.RSApi.loadRoomConfig === 'function') {
      const config = await window.RSApi.loadRoomConfig();
      const state = ensureState();
      state.room_config = config || state.room_config;
      if (config) state.room_config_source = 'file';
      dispatchUpdate();
    }
  }

  function bindConfigChannel() {
    if (typeof window.BroadcastChannel !== 'function' || configChannel) return;
    configChannel = new window.BroadcastChannel('ruvsense-config');
    configChannel.addEventListener('message', (event) => {
      if (event.data?.type !== 'config_updated') return;
      void reloadRoomConfig();
    });
  }

  window.RSWebSocket = {
    connect,
    disconnect,
  };

  bindConfigChannel();
  connect();
})();

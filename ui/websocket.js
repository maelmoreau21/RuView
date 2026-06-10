(function () {
  const RECONNECT_MS = 3000;
  const DEFAULT_API_BASE = 'http://localhost:3000';
  let socket = null;
  let socketUrl = null;
  let reconnectTimer = null;
  let stopped = false;
  let configChannel = null;
  const updateCallbacks = new Set();

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

  function dispatchUpdate(message = null) {
    const state = ensureState();
    updateCallbacks.forEach((callback) => {
      try {
        callback(message, state);
      } catch (error) {
        console.error('[RuvSenseWS] update callback failed', error);
      }
    });
    if (typeof window.dispatchEvent === 'function' && typeof window.CustomEvent === 'function') {
      window.dispatchEvent(new window.CustomEvent('rs:update', { detail: { message, state } }));
    }
  }

  function safeNumber(value) {
    const n = Number(value);
    return Number.isFinite(n) ? n : null;
  }

  function normalizeApiBase(apiBase) {
    const candidate = apiBase || window.RUVSENSE_CONFIG?.api_base || DEFAULT_API_BASE;
    try {
      const url = new URL(candidate, DEFAULT_API_BASE);
      url.pathname = url.pathname.replace(/\/api\/v1\/?$/, '').replace(/\/$/, '');
      return `${url.protocol}//${url.host}${url.pathname === '/' ? '' : url.pathname}`;
    } catch {
      return DEFAULT_API_BASE;
    }
  }

  function wsUrlFromApiBase(apiBase) {
    const base = normalizeApiBase(apiBase);
    if (base.startsWith('ws://') || base.startsWith('wss://')) return `${base.replace(/\/$/, '')}/ws/pose`;
    const url = new URL(base);
    const protocol = url.protocol === 'https:' ? 'wss:' : 'ws:';
    return `${protocol}//${url.host}/ws/pose`;
  }

  function normalizeNodeList(message) {
    if (Array.isArray(message.nodes)) return message.nodes;
    const rawNodes = message.node_status ?? message.nodeStatus;
    if (Array.isArray(rawNodes)) return rawNodes;
    if (rawNodes && typeof rawNodes === 'object') {
      return Object.entries(rawNodes).map(([key, node]) => ({
        ...(node && typeof node === 'object' ? node : {}),
        id: node?.id ?? node?.node_id ?? key,
        node_id: node?.node_id ?? node?.id ?? key,
      }));
    }
    if (message.node_id || message.rssi || message.rssi_dbm) {
      return [message];
    }
    return [];
  }

  function normalizeNodeMessage(message) {
    return {
      ...message,
      nodes: normalizeNodeList(message),
    };
  }

  function normalizePerson(raw, index) {
    if (!raw || typeof raw !== 'object') return null;
    const x = safeNumber(raw.x ?? raw.position?.x ?? raw.location?.x ?? raw.centroid?.x);
    const y = safeNumber(raw.y ?? raw.position?.y ?? raw.location?.y ?? raw.centroid?.y);
    const confidence = safeNumber(raw.confidence ?? raw.score ?? raw.presence_confidence);
    const breathing = safeNumber(raw.breathing_bpm ?? raw.breathing_rate_bpm ?? raw.respiration_bpm ?? raw.vitals?.breathing_bpm);
    const motion = safeNumber(raw.motion_energy ?? raw.activity ?? raw.motion);
    return {
      ...raw,
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
    state.persons = Array.isArray(rawPersons)
      ? rawPersons.map(normalizePerson).filter(Boolean)
      : [];
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

  function markOffline() {
    const state = ensureState();
    state.connected = false;
    state.persons = [];
    Object.keys(state.nodes).forEach((id) => {
      state.nodes[id].active = false;
    });
    dispatchUpdate(null);
  }

  function handleMessage(event) {
    let message = null;
    try {
      message = JSON.parse(event.data);
    } catch {
      return;
    }
    if (!message || typeof message !== 'object') return;

    const normalized = normalizeNodeMessage(message);
    const state = ensureState();
    state.connected = true;
    if (typeof state.updateNodeStatus === 'function') {
      state.updateNodeStatus(normalized);
    }
    updatePersons(normalized);
    updateSystem(normalized);
    updateAlerts(normalized);
    dispatchUpdate(normalized);
  }

  function scheduleReconnect() {
    if (stopped || reconnectTimer || !socketUrl) return;
    reconnectTimer = window.setTimeout(() => {
      reconnectTimer = null;
      connect(window.RUVSENSE_CONFIG?.api_base);
    }, RECONNECT_MS);
  }

  function openSocket(url) {
    try {
      socket = new WebSocket(url);
    } catch {
      socket = null;
      markOffline();
      scheduleReconnect();
      return;
    }

    socket.addEventListener('open', () => {
      ensureState().connected = true;
      dispatchUpdate(null);
    });

    socket.addEventListener('message', handleMessage);

    socket.addEventListener('close', () => {
      socket = null;
      markOffline();
      scheduleReconnect();
    });

    socket.addEventListener('error', () => {
      markOffline();
    });
  }

  function connect(apiBase) {
    stopped = false;
    const url = wsUrlFromApiBase(apiBase);
    window.RUVSENSE_CONFIG = {
      ...(window.RUVSENSE_CONFIG || {}),
      api_base: normalizeApiBase(apiBase),
    };

    if (socket && socketUrl === url) {
      dispatchUpdate(null);
      return;
    }

    if (reconnectTimer) {
      window.clearTimeout(reconnectTimer);
      reconnectTimer = null;
    }
    if (socket) {
      socket.close();
      socket = null;
    }
    socketUrl = url;
    openSocket(url);
  }

  function disconnect() {
    stopped = true;
    if (reconnectTimer) window.clearTimeout(reconnectTimer);
    reconnectTimer = null;
    if (socket) socket.close();
    socket = null;
    markOffline();
  }

  function onUpdate(callback) {
    if (typeof callback !== 'function') return () => {};
    updateCallbacks.add(callback);
    callback(null, ensureState());
    return () => updateCallbacks.delete(callback);
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
      dispatchUpdate(null);
    }
  }

  function bindConfigChannel() {
    if (typeof window.BroadcastChannel !== 'function' || configChannel) return;
    configChannel = new window.BroadcastChannel('ruvsense-config');
    configChannel.addEventListener('message', (event) => {
      if (event.data?.type !== 'config_updated' && event.data?.type !== 'settings_updated') return;
      void reloadRoomConfig();
    });
  }

  window.RuvSenseWS = {
    connect,
    disconnect,
    onUpdate,
  };
  window.RSWebSocket = window.RuvSenseWS;

  bindConfigChannel();
})();

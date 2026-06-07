/**
 * Live-only sensing WebSocket service.
 *
 * This service never generates client-side frames. When the master or hardware
 * is unavailable it reports offline/reconnecting and waits for real data.
 */

const SENSING_WS_PORT_BY_HTTP_PORT = {
  '3000': '3001',
  '8080': '8765',
};

export function buildSensingWsUrl(locationLike = (typeof window !== 'undefined' ? window.location : null)) {
  const protocol = locationLike && locationLike.protocol === 'https:' ? 'wss:' : 'ws:';
  const host = locationLike && locationLike.host ? locationLike.host : 'localhost:3001';
  const hostname = locationLike && locationLike.hostname ? locationLike.hostname : host.split(':')[0];
  const port = locationLike && locationLike.port ? locationLike.port : '';
  const wsPort = SENSING_WS_PORT_BY_HTTP_PORT[port];
  const wsHost = wsPort ? `${hostname}:${wsPort}` : host;

  return `${protocol}//${wsHost}/ws/sensing`;
}

const SENSING_WS_URL = buildSensingWsUrl();
const RECONNECT_DELAYS = [1000, 2000, 4000, 8000, 16000];
const MAX_RECONNECT_ATTEMPTS = 20;

class SensingService {
  constructor() {
    this._ws = null;
    this._listeners = new Set();
    this._stateListeners = new Set();
    this._reconnectAttempt = 0;
    this._reconnectTimer = null;
    this._state = 'disconnected';
    this._dataSource = 'offline';
    this._serverSource = null;
    this._lastMessage = null;
    this._rssiHistory = [];
    this._perNodeRssiHistory = {};
    this._maxHistory = 60;
  }

  start() {
    this._connect();
  }

  stop() {
    this._clearTimers();
    if (this._ws) {
      this._ws.close(1000, 'client stop');
      this._ws = null;
    }
    this._setState('disconnected');
    this._setDataSource('offline');
  }

  onData(callback) {
    this._listeners.add(callback);
    if (this._lastMessage) callback(this._lastMessage);
    return () => this._listeners.delete(callback);
  }

  onStateChange(callback) {
    this._stateListeners.add(callback);
    callback(this._state);
    return () => this._stateListeners.delete(callback);
  }

  getRssiHistory() {
    return [...this._rssiHistory];
  }

  getPerNodeRssiHistory() {
    return { ...this._perNodeRssiHistory };
  }

  get state() {
    return this._state;
  }

  get dataSource() {
    return this._dataSource;
  }

  get serverSource() {
    return this._serverSource;
  }

  _connect() {
    if (this._ws && this._ws.readyState <= WebSocket.OPEN) return;

    this._setState('connecting');
    this._setDataSource('reconnecting');

    try {
      this._ws = new WebSocket(SENSING_WS_URL);
    } catch (err) {
      console.warn('[Sensing] WebSocket constructor failed:', err.message);
      this._scheduleReconnect();
      return;
    }

    this._ws.onopen = () => {
      console.info('[Sensing] Connected to', SENSING_WS_URL);
      this._reconnectAttempt = 0;
      this._setState('connected');
      this._detectServerSource();
    };

    this._ws.onmessage = (evt) => {
      try {
        this._handleData(JSON.parse(evt.data));
      } catch (e) {
        console.warn('[Sensing] Invalid message:', e.message);
      }
    };

    this._ws.onerror = () => {};

    this._ws.onclose = (evt) => {
      console.info('[Sensing] Connection closed (code=%d)', evt.code);
      this._ws = null;
      if (evt.code !== 1000) {
        this._scheduleReconnect();
      } else {
        this._setState('disconnected');
        this._setDataSource('offline');
      }
    };
  }

  _scheduleReconnect() {
    if (this._reconnectAttempt >= MAX_RECONNECT_ATTEMPTS) {
      console.warn('[Sensing] Max reconnect attempts reached; staying offline');
      this._setState('disconnected');
      this._setDataSource('offline');
      return;
    }

    const delay = RECONNECT_DELAYS[Math.min(this._reconnectAttempt, RECONNECT_DELAYS.length - 1)];
    this._reconnectAttempt++;
    this._setState('reconnecting');
    this._setDataSource('reconnecting');
    this._reconnectTimer = setTimeout(() => {
      this._reconnectTimer = null;
      this._connect();
    }, delay);
  }

  async _detectServerSource() {
    try {
      const resp = await fetch('/api/v1/status');
      if (resp.ok) {
        const json = await resp.json();
        this._applyServerSource(json.source);
        return;
      }
    } catch {
      // First frame will update the source if status is unavailable.
    }
    this._setDataSource('live');
  }

  _applyServerSource(rawSource) {
    this._serverSource = rawSource;
    if (rawSource === 'esp32' || rawSource === 'wifi' || rawSource === 'live') {
      this._setDataSource('live');
    } else if (rawSource === 'simulated' || rawSource === 'simulate') {
      this._setDataSource('server-simulated');
    } else {
      this._setDataSource('offline');
    }
  }

  _handleData(data) {
    this._lastMessage = data;
    if (data.source && this._state === 'connected' && data.source !== this._serverSource) {
      this._applyServerSource(data.source);
    }

    if (data.features && data.features.mean_rssi != null) {
      this._rssiHistory.push(data.features.mean_rssi);
      if (this._rssiHistory.length > this._maxHistory) this._rssiHistory.shift();
    }

    if (data.node_features) {
      for (const nf of data.node_features) {
        if (!this._perNodeRssiHistory[nf.node_id]) this._perNodeRssiHistory[nf.node_id] = [];
        this._perNodeRssiHistory[nf.node_id].push(nf.rssi_dbm);
        if (this._perNodeRssiHistory[nf.node_id].length > this._maxHistory) {
          this._perNodeRssiHistory[nf.node_id].shift();
        }
      }
    }

    for (const cb of this._listeners) {
      try {
        cb(data);
      } catch (e) {
        console.error('[Sensing] Listener error:', e);
      }
    }
  }

  _setState(newState) {
    if (newState === this._state) return;
    this._state = newState;
    for (const cb of this._stateListeners) {
      try { cb(newState); } catch { /* ignore listener errors */ }
    }
  }

  _setDataSource(source) {
    if (source === this._dataSource) return;
    this._dataSource = source;
    for (const cb of this._stateListeners) {
      try { cb(this._state); } catch { /* ignore listener errors */ }
    }
  }

  _clearTimers() {
    if (this._reconnectTimer) {
      clearTimeout(this._reconnectTimer);
      this._reconnectTimer = null;
    }
  }
}

export const sensingService = new SensingService();

(function () {
  const API_BASE = 'http://localhost:3000/api/v1/';

  function isFileMode() {
    return window.location.protocol === 'file:';
  }

  function dispatchUpdate() {
    if (typeof window.dispatchEvent === 'function' && typeof window.CustomEvent === 'function') {
      window.dispatchEvent(new window.CustomEvent('rs:update'));
    }
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

  async function safeFetch(url, options) {
    if (isFileMode()) return { ok: false, data: null };
    try {
      const response = await fetch(url, {
        cache: 'no-store',
        ...options,
        headers: {
          ...(options && options.headers ? options.headers : {}),
        },
      });
      return {
        ok: response.ok,
        status: response.status,
        data: await readJson(response),
      };
    } catch {
      return { ok: false, data: null };
    }
  }

  async function loadRoomConfig() {
    const result = await safeFetch('/room-config.json');
    const config = result.ok && result.data && typeof result.data === 'object' ? result.data : null;
    if (config && window.RS) {
      window.RS.room_config = config;
      dispatchUpdate();
    }
    return config;
  }

  async function postConfig(payload) {
    return safeFetch(`${API_BASE}config`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(payload || {}),
    });
  }

  async function recalibrate() {
    return postConfig({ recalibrate: true });
  }

  async function getVersion() {
    const result = await safeFetch(`${API_BASE}version`);
    if (!result.ok || result.data == null) return null;
    if (typeof result.data === 'string') return result.data || null;
    if (window.RS) {
      window.RS.server_version = result.data.version || result.data.server_version || result.data.git_version || window.RS.server_version;
      window.RS.uptime = result.data.uptime || result.data.uptime_seconds || window.RS.uptime;
      dispatchUpdate();
    }
    return result.data.version || result.data.server_version || result.data.git_version || null;
  }

  async function getFeatures() {
    const result = await safeFetch(`${API_BASE}features`);
    return result.ok && result.data && typeof result.data === 'object' ? result.data : null;
  }

  window.RSApi = {
    loadRoomConfig,
    recalibrate,
    getVersion,
    getFeatures,
    postConfig,
  };
})();

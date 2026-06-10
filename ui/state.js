(function () {
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
    room_config_source: 'pending',
  };
})();

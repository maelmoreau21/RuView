(function () {
  function normalizeNodeKey(value) {
    return String(value ?? '').toLowerCase().replace(/[^a-z0-9]/g, '');
  }

  function safeNumber(value) {
    const n = Number(value);
    return Number.isFinite(n) ? n : null;
  }

  function updateNodeStatus(wsMessage) {
    const RS = window.RS;
    const now = Date.now();

    Object.keys(RS.nodes).forEach((id) => {
      RS.nodes[id].active = false;
    });

    if (wsMessage?.nodes && Array.isArray(wsMessage.nodes)) {
      wsMessage.nodes.forEach((node, index) => {
        const id = node?.id ?? node?.node_id ?? index + 1;
        const key = normalizeNodeKey(id);
        if (!key) return;
        RS.nodes[key] = {
          ...node,
          id,
          node_id: node?.node_id ?? id,
          label: node?.label || node?.name || `Node ${id}`,
          active: true,
          last_seen: now,
        };
      });
    }

    if (
      wsMessage?.persons
      && Array.isArray(wsMessage.persons)
      && wsMessage.persons.length > 0
      && safeNumber(wsMessage.node_count) > 0
    ) {
      const count = Math.max(0, Math.floor(safeNumber(wsMessage.node_count)));
      for (let i = 1; i <= count; i += 1) {
        const key = normalizeNodeKey(i);
        RS.nodes[key] = {
          ...(RS.nodes[key] || {}),
          id: RS.nodes[key]?.id ?? i,
          node_id: RS.nodes[key]?.node_id ?? i,
          label: RS.nodes[key]?.label || `Node ${i}`,
          active: true,
          last_seen: now,
        };
      }
    }
  }

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
    updateNodeStatus,
  };
})();

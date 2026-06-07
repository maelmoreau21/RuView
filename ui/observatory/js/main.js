/**
 * RuvSense Console - Main Scene Orchestrator
 *
 * Room-based WiFi sensing visualization with:
 * - Pool of 4 human wireframe figures for live multi-person frames
 * - 7 pose types (standing, walking, lying, sitting, fallen, exercising, gesturing, crouching)
 * - Scenario-specific room props (chair, exercise mat, door, rubble wall, screen, desk)
 * - Dot-matrix mist body mass, particle trails, WiFi waves, signal field
 * - Reflective floor, settings dialog, and practical data HUD
 */
import * as THREE from 'three';
import { OrbitControls } from 'three/addons/controls/OrbitControls.js';

import { NebulaBackground } from './nebula-background.js';
import { PostProcessing } from './post-processing.js';
import { FigurePool, SKELETON_PAIRS } from './figure-pool.js';
import { PoseSystem } from './pose-system.js';
import { HudController, DEFAULTS, SETTINGS_VERSION } from './hud-controller.js';

// ---- Palette ----
const C = {
  greenGlow:  0x00d878,
  greenBright:0x3eff8a,
  greenDim:   0x0a6b3a,
  amber:      0xffb020,
  blueSignal: 0x2090ff,
  redAlert:   0xff3040,
  redHeart:   0xff4060,
  bgDeep:     0x080c14,
};

// SCENARIO_NAMES, DEFAULTS, SETTINGS_VERSION, PRESETS imported from hud-controller.js

// ---- Main Class ----

class Observatory {
  constructor() {
    this._canvas = document.getElementById('observatory-canvas');
    this.settings = { ...DEFAULTS };

    // Load saved settings
    try {
      const ver = localStorage.getItem('ruvsense-console-settings-version');
      if (ver === SETTINGS_VERSION) {
        const saved = localStorage.getItem('ruvsense-console-settings');
        if (saved) Object.assign(this.settings, JSON.parse(saved));
      } else {
        localStorage.removeItem('ruview-observatory-settings');
        localStorage.removeItem('ruvsense-console-settings');
        localStorage.setItem('ruvsense-console-settings-version', SETTINGS_VERSION);
      }
    } catch {}

    // Renderer
    this._renderer = new THREE.WebGLRenderer({
      canvas: this._canvas,
      antialias: true,
      powerPreference: 'high-performance',
    });
    this._renderer.setPixelRatio(Math.min(window.devicePixelRatio, 2));
    this._renderer.setSize(window.innerWidth, window.innerHeight);
    this._renderer.toneMapping = THREE.ACESFilmicToneMapping;
    this._renderer.toneMappingExposure = this.settings.exposure;
    this._renderer.shadowMap.enabled = true;
    this._renderer.shadowMap.type = THREE.PCFSoftShadowMap;

    // Scene
    this._scene = new THREE.Scene();
    this._scene.background = new THREE.Color(C.bgDeep);
    this._scene.fog = new THREE.FogExp2(C.bgDeep, 0.005);

    // Camera
    this._camera = new THREE.PerspectiveCamera(
      this.settings.fov, window.innerWidth / window.innerHeight, 0.1, 300
    );
    this._camera.position.set(6, 5, 8);
    this._camera.lookAt(0, 1.2, 0);

    // Controls
    this._controls = new OrbitControls(this._camera, this._canvas);
    this._controls.enableDamping = true;
    this._controls.dampingFactor = 0.08;
    this._controls.minDistance = 2;
    this._controls.maxDistance = 25;
    this._controls.maxPolarAngle = Math.PI * 0.88;
    this._controls.target.set(0, 1.2, 0);
    this._controls.update();

    this._clock = new THREE.Clock();

    // Live data
    this._currentData = null;
    this._environment = null;
    this._wsReconnectTimer = null;
    this._lastLiveAt = 0;

    // Build scene
    this._setupLighting();
    this._nebula = new NebulaBackground(this._scene);
    this._buildRoom();
    this._buildTopologyDevices();
    this._poseSystem = new PoseSystem();
    this._figurePool = new FigurePool(this._scene, this.settings, this._poseSystem);
    this._buildDotMatrixMist();
    this._buildParticleTrail();
    this._buildWifiWaves();
    this._buildSignalField();

    // Post-processing
    this._postProcessing = new PostProcessing(this._renderer, this._scene, this._camera);
    this._applyPostSettings();

    // HUD controller (settings dialog, sparkline, vital displays)
    this._hud = new HudController(this);

    // State
    this._autopilot = false;
    this._autoAngle = 0;
    this._fpsFrames = 0;
    this._fpsTime = 0;
    this._fpsValue = 60;
    this._showFps = false;
    this._qualityLevel = 2;

    // WebSocket for live data — always try auto-detect on startup
    this._ws = null;
    this._liveData = null;
    this._fetchEnvironment();
    this._autoDetectLive();

    // Input
    this._initKeyboard();
    this._hud.initSettings();
    window.addEventListener('resize', () => this._onResize());

    // Start
    this._animate();
  }

  // ---- Lighting ----

  _setupLighting() {
    this._ambient = new THREE.AmbientLight(0xccccdd, this.settings.ambient * 5.0);
    this._scene.add(this._ambient);

    const hemi = new THREE.HemisphereLight(0x6688bb, 0x203040, 1.2);
    this._scene.add(hemi);

    const key = new THREE.DirectionalLight(0xffeedd, 1.2);
    key.position.set(4, 8, 3);
    key.castShadow = true;
    key.shadow.mapSize.set(1024, 1024);
    key.shadow.camera.near = 0.5;
    key.shadow.camera.far = 20;
    key.shadow.camera.left = -8;
    key.shadow.camera.right = 8;
    key.shadow.camera.top = 8;
    key.shadow.camera.bottom = -8;
    this._scene.add(key);

    // Fill light from opposite side
    const fill = new THREE.DirectionalLight(0x8899bb, 0.7);
    fill.position.set(-4, 5, -2);
    this._scene.add(fill);

    // Rim light from above/behind for edge definition
    const rim = new THREE.DirectionalLight(0x6699cc, 0.5);
    rim.position.set(0, 6, -5);
    this._scene.add(rim);

    // Overhead room light — general illumination
    const overhead = new THREE.PointLight(0x8899aa, 1.0, 20, 1.0);
    overhead.position.set(0, 3.8, 0);
    this._scene.add(overhead);
  }

  // ---- Room ----

  _buildRoom() {
    this._grid = new THREE.GridHelper(12, 24, 0x1a4830, 0x0c2818);
    this._grid.material.opacity = 0.5;
    this._grid.material.transparent = true;
    this._scene.add(this._grid);

    const boxGeo = new THREE.BoxGeometry(12, 4, 10);
    const edges = new THREE.EdgesGeometry(boxGeo);
    this._roomWire = new THREE.LineSegments(edges, new THREE.LineBasicMaterial({
      color: C.greenDim, opacity: 0.3, transparent: true,
    }));
    this._roomWire.position.y = 2;
    this._scene.add(this._roomWire);

    // Reflective floor
    const floorGeo = new THREE.PlaneGeometry(12, 10);
    this._floorMat = new THREE.MeshStandardMaterial({
      color: 0x101810,
      roughness: 1.0 - this.settings.reflect * 0.7,
      metalness: this.settings.reflect * 0.5,
      emissive: 0x020404,
      emissiveIntensity: 0.08,
    });
    const floor = new THREE.Mesh(floorGeo, this._floorMat);
    floor.rotation.x = -Math.PI / 2;
    floor.receiveShadow = true;
    this._scene.add(floor);

  }

  // ---- Topology devices ----

  _buildTopologyDevices() {
    this._topologyGroup = new THREE.Group();
    this._linkGroup = new THREE.Group();
    this._deviceMeshes = new Map();
    this._linkMeshes = new Map();
    this._wifiWaves = [];
    this._scene.add(this._linkGroup);
    this._scene.add(this._topologyGroup);
  }

  // ---- WiFi Waves ----

  _buildWifiWaves() {
    this._wifiWaves = [];
  }

  _ensureWaveSource(id, position, active, scale = 1) {
    let waves = this._wifiWaves.find(w => w.id === id);
    if (!waves) {
      waves = { id, active, shells: [] };
      for (let i = 0; i < 3; i++) {
        const radius = (0.6 + i * 0.65) * scale;
        const geo = new THREE.SphereGeometry(radius, 18, 10, 0, Math.PI * 2, 0, Math.PI * 0.56);
        const mat = new THREE.MeshBasicMaterial({
          color: C.blueSignal,
          transparent: true, opacity: 0,
          side: THREE.DoubleSide,
          blending: THREE.AdditiveBlending,
          depthWrite: false, wireframe: true,
        });
        const shell = new THREE.Mesh(geo, mat);
        this._scene.add(shell);
        waves.shells.push({ mesh: shell, mat, phase: i * 0.75 });
      }
      this._wifiWaves.push(waves);
    }
    waves.active = active;
    for (const shell of waves.shells) {
      shell.mesh.position.set(position[0], position[1] + 0.15, position[2]);
      shell.mesh.visible = true;
    }
  }

  _defaultEnvironment() {
    return {
      room: { dimensions_m: [5.2, 2.6, 4.8] },
      access_points: [
        { ap_id: 'ap-1', label: 'Mesh AP 1', position_m: [-2.4, 1.9, -2.0], active: true },
        { ap_id: 'ap-2', label: 'Mesh AP 2', position_m: [2.4, 1.9, 2.0], active: true },
      ],
      nodes: [
        { node_id: 1, label: 'ESP32-C6 1', position_m: [-2.0, 1.1, -1.6], linked_ap: 'ap-1' },
        { node_id: 2, label: 'ESP32-C6 2', position_m: [2.0, 1.1, -1.6], linked_ap: 'ap-1' },
        { node_id: 3, label: 'ESP32-C6 3', position_m: [0.0, 1.1, 2.0], linked_ap: 'ap-2' },
      ],
      links: [
        { link_id: 'ap-1:c6-1', ap_id: 'ap-1', node_id: 1 },
        { link_id: 'ap-1:c6-2', ap_id: 'ap-1', node_id: 2 },
        { link_id: 'ap-1:c6-3', ap_id: 'ap-1', node_id: 3 },
        { link_id: 'ap-2:c6-1', ap_id: 'ap-2', node_id: 1 },
        { link_id: 'ap-2:c6-2', ap_id: 'ap-2', node_id: 2 },
        { link_id: 'ap-2:c6-3', ap_id: 'ap-2', node_id: 3 },
      ],
    };
  }

  _fetchEnvironment() {
    fetch('/api/v1/environment', { cache: 'no-store' })
      .then(r => r.ok ? r.json() : Promise.reject())
      .then(env => {
        this._environment = env;
        this._syncTopology(this._currentData);
      })
      .catch(() => {
        this._environment = this._defaultEnvironment();
        this._syncTopology(this._currentData);
      });
  }

  _mergeNodes(liveData) {
    const env = this._environment || this._defaultEnvironment();
    const live = new Map((liveData?.nodes || []).map(n => [Number(n.node_id), n]));
    const nodes = (env.nodes || []).map(cfg => ({
      ...cfg,
      ...(live.get(Number(cfg.node_id)) || {}),
    }));
    for (const node of liveData?.nodes || []) {
      if (!nodes.some(n => Number(n.node_id) === Number(node.node_id))) nodes.push(node);
    }
    return nodes;
  }

  _positionOf(entity) {
    return entity?.position_m || entity?.position || [0, 0, 0];
  }

  _upsertDevice(key, kind, label, position, active) {
    let entry = this._deviceMeshes.get(key);
    if (!entry) {
      const group = new THREE.Group();
      const color = kind === 'ap' ? C.blueSignal : C.greenGlow;
      const bodyGeo = kind === 'ap'
        ? new THREE.BoxGeometry(0.54, 0.16, 0.34)
        : new THREE.CylinderGeometry(0.11, 0.11, 0.32, 16);
      const mat = new THREE.MeshBasicMaterial({ color, transparent: true, opacity: 0.9 });
      const body = new THREE.Mesh(bodyGeo, mat);
      body.castShadow = true;
      group.add(body);

      if (kind === 'ap') {
        for (let i = -1; i <= 1; i++) {
          const ant = new THREE.Mesh(
            new THREE.CylinderGeometry(0.012, 0.012, 0.38, 8),
            new THREE.MeshBasicMaterial({ color: 0x9fb6c8, transparent: true, opacity: 0.75 })
          );
          ant.position.set(i * 0.16, 0.28, 0);
          ant.rotation.z = i * 0.18;
          group.add(ant);
        }
      }

      const beacon = new THREE.Mesh(
        new THREE.SphereGeometry(kind === 'ap' ? 0.055 : 0.045, 16, 12),
        new THREE.MeshBasicMaterial({ color, transparent: true, opacity: 1 })
      );
      beacon.position.y = kind === 'ap' ? 0.2 : 0.23;
      group.add(beacon);

      this._topologyGroup.add(group);
      entry = { group, mat, beacon, label };
      this._deviceMeshes.set(key, entry);
    }
    entry.group.position.set(position[0], position[1], position[2]);
    entry.mat.opacity = active ? 0.9 : 0.28;
    entry.beacon.material.opacity = active ? 1 : 0.28;
    entry.group.visible = true;
  }

  _upsertLink(key, from, to, active) {
    let line = this._linkMeshes.get(key);
    if (!line) {
      const geo = new THREE.BufferGeometry();
      const mat = new THREE.LineBasicMaterial({
        color: active ? C.blueSignal : 0x385060,
        transparent: true,
        opacity: active ? 0.55 : 0.16,
      });
      line = new THREE.Line(geo, mat);
      this._linkGroup.add(line);
      this._linkMeshes.set(key, line);
    }
    line.geometry.setFromPoints([
      new THREE.Vector3(from[0], from[1], from[2]),
      new THREE.Vector3(to[0], to[1], to[2]),
    ]);
    line.material.opacity = active ? 0.55 : 0.16;
    line.material.color.set(active ? C.blueSignal : 0x385060);
    line.visible = true;
  }

  _syncTopology(liveData) {
    const env = this._environment || this._defaultEnvironment();
    const nodes = this._mergeNodes(liveData);
    const nodeById = new Map(nodes.map(n => [Number(n.node_id), n]));
    const apById = new Map((env.access_points || []).map(ap => [ap.ap_id, ap]));
    const visibleDevices = new Set();
    const visibleLinks = new Set();

    for (const ap of env.access_points || []) {
      const position = this._positionOf(ap);
      const key = `ap:${ap.ap_id}`;
      visibleDevices.add(key);
      this._upsertDevice(key, 'ap', ap.label || ap.ap_id, position, ap.active !== false);
      this._ensureWaveSource(key, position, ap.active !== false, 1.15);
    }

    for (const node of nodes) {
      const status = String(node.status || (node.active ? 'active' : 'offline')).toLowerCase();
      const active = Boolean(node.active) && !['offline', 'stale'].includes(status);
      const position = this._positionOf(node);
      const key = `node:${node.node_id}`;
      visibleDevices.add(key);
      this._upsertDevice(key, 'node', node.label || `C6-${node.node_id}`, position, active);
      if (active) this._ensureWaveSource(key, position, true, 0.55);
    }

    for (const link of env.links || []) {
      const ap = apById.get(link.ap_id);
      const node = nodeById.get(Number(link.node_id));
      if (!ap || !node) continue;
      const nodeStatus = String(node.status || (node.active ? 'active' : 'offline')).toLowerCase();
      const active = Boolean(node.active) && !['offline', 'stale'].includes(nodeStatus);
      const key = link.link_id || `${link.ap_id}:c6-${link.node_id}`;
      visibleLinks.add(key);
      this._upsertLink(key, this._positionOf(ap), this._positionOf(node), active);
    }

    for (const [key, entry] of this._deviceMeshes) {
      if (!visibleDevices.has(key)) entry.group.visible = false;
    }
    for (const [key, line] of this._linkMeshes) {
      if (!visibleLinks.has(key)) line.visible = false;
    }
  }

  // ========================================
  // DOT MATRIX MIST
  // ========================================

  _buildDotMatrixMist() {
    const COUNT = 800;
    const positions = new Float32Array(COUNT * 3);
    const alphas = new Float32Array(COUNT);
    for (let i = 0; i < COUNT; i++) {
      const angle = Math.random() * Math.PI * 2;
      const r = Math.random() * 0.5;
      positions[i * 3] = Math.cos(angle) * r;
      positions[i * 3 + 1] = Math.random() * 1.8;
      positions[i * 3 + 2] = Math.sin(angle) * r;
      alphas[i] = 0;
    }
    const geo = new THREE.BufferGeometry();
    geo.setAttribute('position', new THREE.BufferAttribute(positions, 3));
    geo.setAttribute('alpha', new THREE.BufferAttribute(alphas, 1));
    const mat = new THREE.ShaderMaterial({
      vertexShader: `
        attribute float alpha;
        varying float vAlpha;
        void main() {
          vAlpha = alpha;
          vec4 mv = modelViewMatrix * vec4(position, 1.0);
          gl_PointSize = 3.0 * (200.0 / -mv.z);
          gl_Position = projectionMatrix * mv;
        }
      `,
      fragmentShader: `
        uniform vec3 uColor;
        varying float vAlpha;
        void main() {
          float d = length(gl_PointCoord - 0.5);
          if (d > 0.5) discard;
          float edge = smoothstep(0.5, 0.2, d);
          gl_FragColor = vec4(uColor, edge * vAlpha);
        }
      `,
      uniforms: { uColor: { value: new THREE.Color(this.settings.wireColor) } },
      transparent: true, blending: THREE.AdditiveBlending, depthWrite: false,
    });
    this._mistPoints = new THREE.Points(geo, mat);
    this._scene.add(this._mistPoints);
    this._mistCount = COUNT;
  }

  // ---- Particle Trail ----

  _buildParticleTrail() {
    const COUNT = 200;
    const positions = new Float32Array(COUNT * 3);
    const ages = new Float32Array(COUNT);
    for (let i = 0; i < COUNT; i++) ages[i] = 1;
    const geo = new THREE.BufferGeometry();
    geo.setAttribute('position', new THREE.BufferAttribute(positions, 3));
    geo.setAttribute('age', new THREE.BufferAttribute(ages, 1));
    const mat = new THREE.ShaderMaterial({
      vertexShader: `
        attribute float age;
        varying float vAge;
        void main() {
          vAge = age;
          vec4 mv = modelViewMatrix * vec4(position, 1.0);
          gl_PointSize = max(1.0, (1.0 - age) * 5.0 * (150.0 / -mv.z));
          gl_Position = projectionMatrix * mv;
        }
      `,
      fragmentShader: `
        uniform vec3 uColor;
        varying float vAge;
        void main() {
          float d = length(gl_PointCoord - 0.5);
          if (d > 0.5) discard;
          float alpha = (1.0 - vAge) * 0.6 * smoothstep(0.5, 0.1, d);
          gl_FragColor = vec4(uColor, alpha);
        }
      `,
      uniforms: { uColor: { value: new THREE.Color(C.greenGlow) } },
      transparent: true, blending: THREE.AdditiveBlending, depthWrite: false,
    });
    this._trail = new THREE.Points(geo, mat);
    this._scene.add(this._trail);
    this._trailHead = 0;
    this._trailCount = COUNT;
    this._trailTimer = 0;
  }

  // ---- Signal Field ----

  _buildSignalField() {
    const gridSize = 20;
    const count = gridSize * gridSize;
    const positions = new Float32Array(count * 3);
    this._fieldColors = new Float32Array(count * 3);
    this._fieldSizes = new Float32Array(count);
    for (let iz = 0; iz < gridSize; iz++) {
      for (let ix = 0; ix < gridSize; ix++) {
        const idx = iz * gridSize + ix;
        positions[idx * 3] = (ix - gridSize / 2) * 0.6;
        positions[idx * 3 + 1] = 0.02;
        positions[idx * 3 + 2] = (iz - gridSize / 2) * 0.5;
        this._fieldSizes[idx] = 8;
      }
    }
    const geo = new THREE.BufferGeometry();
    geo.setAttribute('position', new THREE.BufferAttribute(positions, 3));
    geo.setAttribute('color', new THREE.BufferAttribute(this._fieldColors, 3));
    geo.setAttribute('size', new THREE.BufferAttribute(this._fieldSizes, 1));
    this._fieldMat = new THREE.PointsMaterial({
      size: 0.35, vertexColors: true, transparent: true,
      opacity: this.settings.field, blending: THREE.AdditiveBlending,
      depthWrite: false, sizeAttenuation: true,
    });
    this._fieldPoints = new THREE.Points(geo, this._fieldMat);
    this._scene.add(this._fieldPoints);
  }

  // ---- Keyboard ----

  _initKeyboard() {
    window.addEventListener('keydown', (e) => {
      if (this._hud.settingsOpen) return;
      switch (e.key.toLowerCase()) {
        case 'a':
          this._autopilot = !this._autopilot;
          this._controls.enabled = !this._autopilot;
          break;
        case 'f':
          this._showFps = !this._showFps;
          document.getElementById('fps-counter').style.display = this._showFps ? 'block' : 'none';
          break;
        case 's': this._hud.toggleSettings(); break;
      }
    });
  }

  // ---- Settings / HUD methods delegated to HudController ----

  _applyPostSettings() {
    const pp = this._postProcessing;
    pp._bloomPass.strength = this.settings.bloom;
    pp._bloomPass.radius = this.settings.bloomRadius;
    pp._bloomPass.threshold = this.settings.bloomThresh;
    pp._vignettePass.uniforms.uVignetteStrength.value = this.settings.vignette;
    pp._vignettePass.uniforms.uGrainStrength.value = this.settings.grain;
    pp._vignettePass.uniforms.uChromaticStrength.value = this.settings.chromatic;
  }

  _applyColors() {
    const wc = new THREE.Color(this.settings.wireColor);
    const jc = new THREE.Color(this.settings.jointColor);
    this._figurePool.applyColors(wc, jc);
    this._mistPoints.material.uniforms.uColor.value.copy(wc);
  }

  // ---- WebSocket live data ----

  _autoDetectLive() {
    // Probe sensing server health on same origin, then common ports
    const host = window.location.hostname || 'localhost';
    const candidates = [
      window.location.origin,                   // same origin (e.g. :3000)
      `http://${host}:8765`,                     // default WS port
      `http://${host}:3000`,                     // default HTTP port
    ];
    // Deduplicate
    const unique = [...new Set(candidates)];

    const tryNext = (i) => {
      if (i >= unique.length) {
        console.log('[Observatory] No sensing server detected; staying offline');
        this._hud.updateSourceBadge('offline', null);
        this._scheduleReconnect();
        return;
      }
      const base = unique[i];
      fetch(`${base}/health`, { signal: AbortSignal.timeout(1500) })
        .then(r => r.ok ? r.json() : Promise.reject())
        .then(data => {
          if (data && data.status === 'ok') {
            const wsProto = base.startsWith('https') ? 'wss:' : 'ws:';
            const urlObj = new URL(base);
            const wsUrl = `${wsProto}//${urlObj.host}/ws/sensing`;
            console.log('[Observatory] Sensing server detected at', base, '→', wsUrl);
            this.settings.dataSource = 'ws';
            this.settings.wsUrl = wsUrl;
            this._connectWS(wsUrl);
          } else {
            tryNext(i + 1);
          }
        })
        .catch(() => tryNext(i + 1));
    };
    tryNext(0);
  }

  _connectWS(url) {
    this._disconnectWS();
    try {
      this._ws = new WebSocket(url);
      this._ws.onopen = () => {
        console.log('[Observatory] WebSocket connected');
        this._hud.updateSourceBadge('live', this._ws);
      };
      this._ws.onmessage = (evt) => {
        try {
          this._liveData = JSON.parse(evt.data);
          this._lastLiveAt = performance.now();
          this._syncTopology(this._liveData);
        } catch {}
      };
      this._ws.onclose = () => {
        console.log('[Observatory] WebSocket closed; stream offline');
        this._ws = null;
        this._hud.updateSourceBadge('offline', null);
        this._scheduleReconnect();
      };
      this._ws.onerror = () => {};
    } catch {}
  }

  _scheduleReconnect() {
    if (this._wsReconnectTimer) return;
    this._wsReconnectTimer = window.setTimeout(() => {
      this._wsReconnectTimer = null;
      this._autoDetectLive();
    }, 3000);
  }

  _disconnectWS() {
    if (this._ws) { this._ws.close(); this._ws = null; }
    this._liveData = null;
  }

  _emptyLiveFrame() {
    return {
      msg_type: 'sensing_update',
      source: 'offline',
      nodes: this._mergeNodes(null),
      persons: [],
      estimated_persons: 0,
      features: {
        mean_rssi: 0,
        variance: 0,
        motion_band_power: 0,
      },
      classification: {
        presence: false,
        motion_level: 'absent',
        confidence: 0,
      },
      signal_field: null,
      vital_signs: null,
    };
  }

  // ========================================
  // ANIMATION LOOP
  // ========================================

  _animate() {
    requestAnimationFrame(() => this._animate());
    const dt = Math.min(this._clock.getDelta(), 0.1);
    const elapsed = this._clock.getElapsedTime();

    // Live data only. Without WebSocket frames the scene remains in a real
    // offline state instead of fabricating movement.
    this._currentData = this._liveData || this._emptyLiveFrame();
    const data = this._currentData;

    // Updates
    this._nebula.update(dt, elapsed);
    this._figurePool.update(data, elapsed);
    this._updateDotMatrixMist(data, elapsed);
    this._updateParticleTrail(data, dt, elapsed);
    this._syncTopology(data);
    this._updateWifiWaves(elapsed);
    this._updateSignalField(data);
    this._hud.updateHUD(data);
    this._hud.updateSparkline(data);

    // Autopilot orbit
    if (this._autopilot) {
      this._autoAngle += dt * this.settings.orbitSpeed;
      const r = 10;
      this._camera.position.set(
        Math.sin(this._autoAngle) * r,
        4.5 + Math.sin(this._autoAngle * 0.5),
        Math.cos(this._autoAngle) * r
      );
      this._controls.target.set(0, 1.2, 0);
      this._controls.update();
    }
    this._controls.update();
    this._postProcessing.update(elapsed);
    this._postProcessing.render();
    this._updateFPS(dt);
  }


  // ========================================
  // MIST & TRAIL
  // ========================================

  _updateDotMatrixMist(data, elapsed) {
    const persons = data?.persons || [];
    const isPresent = data?.classification?.presence || false;
    const pos = this._mistPoints.geometry.attributes.position;
    const alpha = this._mistPoints.geometry.attributes.alpha;

    if (!isPresent || persons.length === 0) {
      for (let i = 0; i < this._mistCount; i++) {
        alpha.array[i] = Math.max(0, alpha.array[i] - 0.02);
      }
      alpha.needsUpdate = true;
      return;
    }

    // Follow primary person
    const pp = persons[0].position || [0, 0, 0];
    const px = pp[0] || 0, pz = pp[2] || 0;
    const ms = persons[0].motion_score || 0;
    const pose = persons[0].pose || 'standing';
    const isLying = pose === 'lying' || pose === 'fallen';
    const bodyH = isLying ? 0.4 : 1.7;
    const bodyBaseY = isLying ? (pp[1] || 0) + 0.05 : 0.05;
    const spread = ms > 50 ? 0.6 : 0.4;

    for (let i = 0; i < this._mistCount; i++) {
      const drift = Math.sin(elapsed * 0.5 + i * 0.1) * 0.003;
      const angle = (i / this._mistCount) * Math.PI * 2 + elapsed * 0.1;
      const layerT = (i % 20) / 20;
      const layerY = bodyBaseY + layerT * bodyH;

      let bodyWidth;
      if (isLying) {
        bodyWidth = 0.25;
      } else {
        bodyWidth = layerT > 0.75 ? 0.15 : (layerT > 0.45 ? 0.25 : 0.18);
      }
      const r = bodyWidth * (0.5 + 0.5 * Math.sin(i * 1.7 + elapsed * 0.3)) * spread;

      const tx = px + Math.cos(angle + i * 0.3) * r + drift;
      const tz = pz + Math.sin(angle + i * 0.5) * r * 0.6;

      pos.array[i * 3] += (tx - pos.array[i * 3]) * 0.05;
      pos.array[i * 3 + 1] += (layerY - pos.array[i * 3 + 1]) * 0.05;
      pos.array[i * 3 + 2] += (tz - pos.array[i * 3 + 2]) * 0.05;

      const targetAlpha = 0.15 + Math.sin(elapsed * 2 + i * 0.5) * 0.08;
      alpha.array[i] += (targetAlpha - alpha.array[i]) * 0.08;
    }
    pos.needsUpdate = true;
    alpha.needsUpdate = true;
  }

  _updateParticleTrail(data, dt, elapsed) {
    if (this.settings.trail <= 0) return;
    const persons = data?.persons || [];
    const isPresent = data?.classification?.presence || false;
    const pos = this._trail.geometry.attributes.position;
    const ages = this._trail.geometry.attributes.age;

    for (let i = 0; i < this._trailCount; i++) {
      ages.array[i] = Math.min(1, ages.array[i] + dt * 0.8);
    }

    // Emit from all active persons
    if (isPresent && persons.length > 0) {
      this._trailTimer += dt;
      const ms = persons[0].motion_score || 0;
      const emitRate = ms > 50 ? 0.02 : 0.08;

      if (this._trailTimer >= emitRate) {
        this._trailTimer = 0;
        for (const p of persons) {
          const pp = p.position || [0, 0, 0];
          const idx = this._trailHead;
          pos.array[idx * 3] = (pp[0] || 0) + (Math.random() - 0.5) * 0.15;
          pos.array[idx * 3 + 1] = Math.random() * 1.5 + 0.1;
          pos.array[idx * 3 + 2] = (pp[2] || 0) + (Math.random() - 0.5) * 0.15;
          ages.array[idx] = 0;
          this._trailHead = (this._trailHead + 1) % this._trailCount;
        }
      }
    }
    pos.needsUpdate = true;
    ages.needsUpdate = true;
  }

  // ---- WiFi Waves ----

  _updateWifiWaves(elapsed) {
    for (const source of this._wifiWaves) {
      for (const w of source.shells) {
        const t = (elapsed * 0.8 + w.phase) % 4.5;
        const life = t / 4.5;
        const activity = source.active ? 1 : 0.22;
        w.mat.opacity = Math.max(0, this.settings.waves * 0.22 * activity * (1 - life));
        const scale = 1 + life * 0.7;
        w.mesh.scale.set(scale, scale, scale);
        w.mesh.rotation.y = elapsed * 0.05;
      }
    }
  }

  // ---- Signal Field ----

  _updateSignalField(data) {
    const field = data?.signal_field?.values;
    if (!field) return;
    const count = Math.min(field.length, 400);
    for (let i = 0; i < count; i++) {
      const v = field[i] || 0;
      let r, g, b;
      if (v < 0.3) { r = 0; g = v * 1.5; b = v * 0.3; }
      else if (v < 0.6) {
        const t = (v - 0.3) / 0.3;
        r = t * 0.3; g = 0.45 + t * 0.4; b = 0.09 - t * 0.05;
      } else {
        const t = (v - 0.6) / 0.4;
        r = 0.3 + t * 0.7; g = 0.85 - t * 0.2; b = 0.04;
      }
      this._fieldColors[i * 3] = r;
      this._fieldColors[i * 3 + 1] = g;
      this._fieldColors[i * 3 + 2] = b;
      this._fieldSizes[i] = 5 + v * 15;
    }
    this._fieldPoints.geometry.attributes.color.needsUpdate = true;
    this._fieldPoints.geometry.attributes.size.needsUpdate = true;
  }

  // ---- FPS ----

  _updateFPS(dt) {
    this._fpsFrames++;
    this._fpsTime += dt;
    if (this._fpsTime >= 1) {
      this._fpsValue = Math.round(this._fpsFrames / this._fpsTime);
      this._fpsFrames = 0;
      this._fpsTime = 0;
      if (this._showFps) {
        document.getElementById('fps-counter').textContent = `${this._fpsValue} FPS`;
      }
      this._adaptQuality();
    }
  }

  _adaptQuality() {
    let nl = this._qualityLevel;
    if (this._fpsValue < 25 && nl > 0) nl--;
    else if (this._fpsValue > 55 && nl < 2) nl++;
    if (nl !== this._qualityLevel) {
      this._qualityLevel = nl;
      this._nebula.setQuality(nl);
      this._postProcessing.setQuality(nl);
    }
  }

  _onResize() {
    const w = window.innerWidth, h = window.innerHeight;
    this._camera.aspect = w / h;
    this._camera.updateProjectionMatrix();
    this._renderer.setSize(w, h);
    this._postProcessing.resize(w, h);
  }
}

new Observatory();

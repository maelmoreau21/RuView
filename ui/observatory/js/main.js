/**
 * RuvSense Console - Main Scene Orchestrator
 *
 * Room-based WiFi sensing visualization with:
 * - Anonymous presence markers for live multi-person frames
 * - Scenario-specific room props (chair, exercise mat, door, rubble wall, screen, desk)
 * - Dot-matrix mist body mass, particle trails, WiFi waves, signal field
 * - Reflective floor, settings dialog, and practical data HUD
 */
import * as THREE from 'three';
import { OrbitControls } from 'three/addons/controls/OrbitControls.js';

import { NebulaBackground } from './nebula-background.js';
import { PostProcessing } from './post-processing.js';
import { ScenarioProps } from './scenario-props.js';
import { HudController, DEFAULTS, SETTINGS_VERSION } from './hud-controller.js';
import { initAlerts } from '../../alerts.js?v=module';

// ---- Palette ----
const C = {
  greenGlow:  0x00d878,
  greenBright:0x3eff8a,
  greenDim:   0x0a6b3a,
  amber:      0xffb020,
  blueSignal: 0x2090ff,
  obstacleHot:0xff5a1f,
  redAlert:   0xff3040,
  redHeart:   0xff4060,
  bgDeep:     0x080c14,
};

const MAX_SCENE_PERSONS = 8;
const PERSON_DEDUPE_RADIUS_M = 0.65;
const ROOM_CONFIG_ENDPOINT = '/api/v1/config/room';
const ROOM_CONFIG_STORAGE_KEY = 'ruvsense:room-config';
const NODE_ACTIVE_COLOR = 0x3b82f6;
const NODE_INACTIVE_COLOR = 0x374151;
const NODE_ACTIVE_WINDOW_MS = 5000;
const NODE_CUBE_SIZE_M = 0.15;
const NODE_ELEVATION_M = 0.1;
const PERSON_HEIGHT_M = 1.7;
const ROOM_VISUAL_HEIGHT_M = 2.6;
const DEFAULT_ROOM_CONFIG = {
  version: 2,
  room: {
    shape: 'polygon',
    boundary: [
      { x: 0, y: 0 },
      { x: 5, y: 0 },
      { x: 5, y: 4 },
      { x: 0, y: 4 },
    ],
  },
  nodes: [
    { id: 1, x: 0, y: 0, active: true },
    { id: 2, x: 5, y: 0, active: true },
    { id: 3, x: 2.5, y: 4, active: true },
  ],
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
    this._environmentNotice = null;
    this._sensorOrigin = [0, 0, 0];
    this._sensorBounds = { width: 12, height: 4, depth: 10 };
    this._sensorTargetY = 1.2;
    this._sceneTarget = new THREE.Vector3(0, 1.2, 0);
    this._roomSize = { width: 12, height: 4, depth: 10 };
    this._cameraFramedToSensors = false;
    this._wsReconnectTimer = null;
    this._wsCandidateIndex = 0;
    this._lastLiveAt = 0;
    this._lastEdgeVitals = null;
    this._currentScenario = null;
    this._obstacleSummary = null;
    this._personPositions = new Map();
    this._linkRaycaster = new THREE.Raycaster();
    this._connectionState = 'connecting';
    this._roomConfig = null;
    this._roomConfigSource = 'default';
    this.nodeObjects = new Map();
    this._nodeRangeObjects = new Map();
    this._nodeRuntimeState = new Map();
    this._presenceSilhouettes = new Map();
    this._activeRoomNodeCount = 0;
    this._sceneStatusOverlay = null;
    this._unsubscribeWs = null;
    this._scenarioProps = null;
    this._nebula = null;
    this._mistPoints = null;
    this._trail = null;
    this._fieldPoints = null;

    // Build scene
    this._setupLighting();
    this._buildRoom();
    this._buildTopologyDevices();
    this._buildRoomConfigLayer();
    this._buildPresenceLayer();
    this._buildWifiWaves();

    // Post-processing
    this._postProcessing = new PostProcessing(this._renderer, this._scene, this._camera);
    this._applyPostSettings();

    // HUD controller (settings dialog, sparkline, vital displays)
    this._hud = new HudController(this);
    initAlerts({ source: 'observatory' });
    this._initSceneStatusOverlay();
    this._initRoomConfigPanel();
    this._loadRoomConfig();

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
    this._connectLiveUpdates();

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

  _roomBoundary(config = this._roomConfig || DEFAULT_ROOM_CONFIG) {
    const boundary = config?.room?.boundary;
    return Array.isArray(boundary)
      ? boundary
        .map((point) => {
          const x = Number(point?.x);
          const y = Number(point?.y);
          return Number.isFinite(x) && Number.isFinite(y) ? { x, y } : null;
        })
        .filter(Boolean)
      : [];
  }

  _roomConfigBounds(config = this._roomConfig || DEFAULT_ROOM_CONFIG) {
    const boundary = this._roomBoundary(config);
    if (boundary.length < 3) {
      return { minX: 0, minY: 0, maxX: 5, maxY: 4, width: 5, depth: 4, height: ROOM_VISUAL_HEIGHT_M };
    }
    const xs = boundary.map((point) => point.x);
    const ys = boundary.map((point) => point.y);
    const minX = Math.min(...xs);
    const minY = Math.min(...ys);
    const maxX = Math.max(...xs);
    const maxY = Math.max(...ys);
    return {
      minX,
      minY,
      maxX,
      maxY,
      width: Math.max(0.001, maxX - minX),
      depth: Math.max(0.001, maxY - minY),
      height: ROOM_VISUAL_HEIGHT_M,
    };
  }

  _buildRoom() {
    const bounds = this._roomConfigBounds(DEFAULT_ROOM_CONFIG);

    this._grid = this._createRoomGrid(bounds);
    this._scene.add(this._grid);

    this._roomWire = this._createRoomWire(DEFAULT_ROOM_CONFIG, bounds.height);
    this._scene.add(this._roomWire);

    this._floorMat = new THREE.MeshStandardMaterial({
      color: 0x101810,
      roughness: 1.0 - this.settings.reflect * 0.7,
      metalness: this.settings.reflect * 0.5,
      emissive: 0x020404,
      emissiveIntensity: 0.08,
    });
    this._floor = new THREE.Mesh(this._createRoomFloorGeometry(DEFAULT_ROOM_CONFIG), this._floorMat);
    this._floor.rotation.x = -Math.PI / 2;
    this._floor.receiveShadow = true;
    this._floor.userData.obstacleName = 'Sol de la pièce';
    this._floor.userData.isFloorObstacle = true;
    this._scene.add(this._floor);

  }

  _roomDimensions(env) {
    if (this._roomConfig) {
      const bounds = this._roomConfigBounds(this._roomConfig);
      return { width: bounds.width, height: bounds.height, depth: bounds.depth };
    }
    const dims = env?.room?.dimensions_m;
    const width = Number(dims?.[0]);
    const height = Number(dims?.[1]);
    const depth = Number(dims?.[2]);
    return {
      width: Number.isFinite(width) && width > 0 ? width : this._roomSize.width,
      height: Number.isFinite(height) && height > 0 ? height : this._roomSize.height,
      depth: Number.isFinite(depth) && depth > 0 ? depth : this._roomSize.depth,
    };
  }

  _createRoomGrid(bounds) {
    const divisions = Math.max(4, Math.min(80, Math.ceil(Math.max(bounds.width, bounds.depth) * 2)));
    const vertices = [];
    for (let i = 0; i <= divisions; i++) {
      const x = bounds.minX + (bounds.width * i) / divisions;
      const z = bounds.minY + (bounds.depth * i) / divisions;
      vertices.push(x, 0.01, bounds.minY, x, 0.01, bounds.maxY);
      vertices.push(bounds.minX, 0.01, z, bounds.maxX, 0.01, z);
    }
    const geo = new THREE.BufferGeometry();
    geo.setAttribute('position', new THREE.Float32BufferAttribute(vertices, 3));
    const mat = new THREE.LineBasicMaterial({
      color: 0x0c2818,
      opacity: 0.5,
      transparent: true,
      depthWrite: false,
    });
    return new THREE.LineSegments(geo, mat);
  }

  _createRoomFloorGeometry(config) {
    const boundary = this._roomBoundary(config);
    const shape = new THREE.Shape();
    boundary.forEach((point, index) => {
      if (index === 0) shape.moveTo(point.x, point.y);
      else shape.lineTo(point.x, point.y);
    });
    shape.closePath();
    return new THREE.ShapeGeometry(shape);
  }

  _createRoomWire(config, height) {
    const boundary = this._roomBoundary(config);
    const vertices = [];
    boundary.forEach((point, index) => {
      const next = boundary[(index + 1) % boundary.length];
      vertices.push(point.x, 0.02, point.y, next.x, 0.02, next.y);
      vertices.push(point.x, height, point.y, next.x, height, next.y);
      vertices.push(point.x, 0.02, point.y, point.x, height, point.y);
    });
    const geo = new THREE.BufferGeometry();
    geo.setAttribute('position', new THREE.Float32BufferAttribute(vertices, 3));
    return new THREE.LineSegments(geo, new THREE.LineBasicMaterial({
      color: C.greenDim, opacity: 0.3, transparent: true,
    }));
  }

  _disposeLine(line) {
    if (!line) return;
    line.geometry?.dispose();
    if (Array.isArray(line.material)) {
      line.material.forEach((mat) => mat.dispose());
    } else {
      line.material?.dispose();
    }
    this._scene.remove(line);
  }

  _syncRoomGeometry(env) {
    const config = this._roomConfig || DEFAULT_ROOM_CONFIG;
    const bounds = this._roomConfigBounds(config);
    const next = { width: bounds.width, height: bounds.height, depth: bounds.depth };
    const same =
      Math.abs(next.width - this._roomSize.width) < 1e-6 &&
      Math.abs(next.height - this._roomSize.height) < 1e-6 &&
      Math.abs(next.depth - this._roomSize.depth) < 1e-6;
    const sameBoundary = this._roomGeometryKey === JSON.stringify(config.room.boundary);
    if (same && sameBoundary) return;

    this._roomSize = next;
    this._roomGeometryKey = JSON.stringify(config.room.boundary);
    this._disposeLine(this._grid);
    this._grid = this._createRoomGrid(bounds);
    this._grid.visible = this.settings.grid;
    this._scene.add(this._grid);

    this._disposeLine(this._roomWire);
    this._roomWire = this._createRoomWire(config, bounds.height);
    this._roomWire.visible = this.settings.room;
    this._scene.add(this._roomWire);

    if (this._floor) {
      this._floor.geometry.dispose();
      this._floor.geometry = this._createRoomFloorGeometry(config);
    }
  }

  // ---- Topology devices ----

  _buildTopologyDevices() {
    this._topologyGroup = null;
    this._linkGroup = null;
    this._impactGroup = null;
    this._coverageGroup = null;
    this._deviceMeshes = new Map();
    this._linkMeshes = new Map();
    this._impactMarkers = new Map();
    this._coverageMeshes = new Map();
    this._wifiWaves = [];
  }

  _buildRoomConfigLayer() {
    this._roomConfigGroup = null;
  }

  _buildPresenceLayer() {
    this._presenceGroup = null;
  }

  _ensureTopologyGroups() {
    if (this._topologyGroup) return;
    this._topologyGroup = new THREE.Group();
    this._linkGroup = new THREE.Group();
    this._impactGroup = new THREE.Group();
    this._coverageGroup = new THREE.Group();
    this._scene.add(this._coverageGroup);
    this._scene.add(this._linkGroup);
    this._scene.add(this._impactGroup);
    this._scene.add(this._topologyGroup);
  }

  _ensureRoomConfigGroup() {
    if (this._roomConfigGroup) return;
    this._roomConfigGroup = new THREE.Group();
    this._roomConfigGroup.name = 'room-config-layer';
    this._scene.add(this._roomConfigGroup);
  }

  _ensurePresenceGroup() {
    if (this._presenceGroup) return;
    this._presenceGroup = new THREE.Group();
    this._presenceGroup.name = 'presence-update-silhouettes';
    this._scene.add(this._presenceGroup);
  }

  _initSceneStatusOverlay() {
    this._sceneStatusOverlay = document.getElementById('scene-status-overlay');
    this._setSceneStatus('connecting', 'En attente du serveur...');
  }

  _setSceneStatus(status, message = '') {
    this._connectionState = status;
    const overlay = this._sceneStatusOverlay;
    if (!overlay) return;

    overlay.classList.remove('scene-status-overlay--connecting', 'scene-status-overlay--no-nodes');
    if (status === 'live') {
      overlay.hidden = true;
      return;
    }

    overlay.hidden = false;
    if (status === 'no_nodes') {
      overlay.classList.add('scene-status-overlay--no-nodes');
      overlay.textContent = message || 'Aucun n\u0153ud ESP32 connect\u00e9';
      return;
    }

    overlay.classList.add('scene-status-overlay--connecting');
    overlay.textContent = message || 'En attente du serveur...';
  }

  _initRoomConfigPanel() {
    this._roomConfigPanel = document.getElementById('room-config-panel');
    this._roomConfigButton = document.getElementById('room-config-btn');
    this._roomConfigClose = document.getElementById('room-config-close');
    this._roomConfigApply = document.getElementById('room-config-apply');
    this._roomConfigReset = document.getElementById('room-config-reset');
    this._roomNodeFields = document.getElementById('room-node-fields');

    this._roomConfigButton?.addEventListener('click', () => this._toggleRoomConfigPanel());
    this._roomConfigClose?.addEventListener('click', () => this._closeRoomConfigPanel());
    this._roomConfigApply?.addEventListener('click', () => {
      const next = this._roomConfigFromPanel();
      if (next) this._applyRoomConfig(next, 'local', true);
    });
    this._roomConfigReset?.addEventListener('click', () => {
      try { localStorage.removeItem(ROOM_CONFIG_STORAGE_KEY); } catch {}
      this._loadRoomConfig(true);
    });
  }

  _toggleRoomConfigPanel() {
    if (!this._roomConfigPanel) return;
    this._roomConfigPanel.hidden = !this._roomConfigPanel.hidden;
    if (!this._roomConfigPanel.hidden) this._populateRoomConfigPanel();
  }

  _closeRoomConfigPanel() {
    if (this._roomConfigPanel) this._roomConfigPanel.hidden = true;
  }

  _readStoredRoomConfig() {
    try {
      const raw = localStorage.getItem(ROOM_CONFIG_STORAGE_KEY);
      return raw ? this._normalizeRoomConfig(JSON.parse(raw)) : null;
    } catch {
      return null;
    }
  }

  async _loadRoomConfig(ignoreStored = false) {
    try {
      const response = await fetch(ROOM_CONFIG_ENDPOINT, { cache: 'no-store' });
      if (response.ok) {
        const config = this._normalizeRoomConfig(await response.json());
        if (config) {
          this._applyRoomConfig(config, 'api', false);
          return;
        }
      }
    } catch {
      // The API may still be starting; use visual defaults until it responds.
    }

    this._applyRoomConfig(DEFAULT_ROOM_CONFIG, 'default', false);
  }

  _normalizeRoomConfig(raw) {
    if (!raw || typeof raw !== 'object') return null;
    if (Number(raw.version) !== 2 || raw.room?.shape !== 'polygon') return null;
    const boundary = Array.isArray(raw.room.boundary)
      ? raw.room.boundary
        .map((point) => {
          const x = Number(point?.x);
          const y = Number(point?.y);
          return Number.isFinite(x) && Number.isFinite(y) ? { x, y } : null;
        })
        .filter(Boolean)
      : [];
    const nodes = Array.isArray(raw.nodes) ? raw.nodes : [];
    if (boundary.length < 3) return null;

    return {
      version: 2,
      room: { shape: 'polygon', boundary },
      nodes: nodes.map((node, index) => {
        const id = Number(node?.id ?? node?.node_id ?? index + 1);
        const x = Number(node?.x);
        const y = Number(node?.y);
        return {
          id: Number.isFinite(id) ? id : index + 1,
          x: Number.isFinite(x) ? x : 0,
          y: Number.isFinite(y) ? y : 0,
          active: node?.active !== false,
        };
      }),
    };
  }

  _applyRoomConfig(rawConfig, source = 'runtime', persist = false) {
    const config = this._normalizeRoomConfig(rawConfig) || this._normalizeRoomConfig(DEFAULT_ROOM_CONFIG);
    this._roomConfig = config;
    this._roomConfigSource = source;

    if (persist) {
      try { localStorage.setItem(ROOM_CONFIG_STORAGE_KEY, JSON.stringify(config)); } catch {}
    }

    this._syncRoomGeometry();
    this._rebuildRoomConfigScene(null);
    this._populateRoomConfigPanel();
    this._syncRoomConfigNodes(this._currentData || this._liveData || null);
  }

  _populateRoomConfigPanel() {
    if (!this._roomConfig || !this._roomConfigPanel) return;
    const bounds = this._roomConfigBounds(this._roomConfig);
    const widthInput = document.getElementById('room-width-input');
    const heightInput = document.getElementById('room-height-input');
    if (widthInput) widthInput.value = bounds.width.toFixed(1);
    if (heightInput) heightInput.value = bounds.depth.toFixed(1);
    if (!this._roomNodeFields) return;

    this._roomNodeFields.replaceChildren();
    const title = document.createElement('div');
    title.className = 'room-node-title';
    title.textContent = 'Noeuds ESP32';
    this._roomNodeFields.appendChild(title);

    for (const node of this._roomConfig.nodes) {
      const row = document.createElement('label');
      row.className = 'room-node-field';
      row.dataset.nodeId = String(node.id);

      const label = document.createElement('span');
      label.textContent = node.label || `Node ${node.id}`;

      const xInput = document.createElement('input');
      xInput.type = 'number';
      xInput.step = '0.1';
      xInput.value = Number(node.x).toFixed(1);
      xInput.dataset.axis = 'x';
      xInput.setAttribute('aria-label', `${label.textContent} x`);

      const yInput = document.createElement('input');
      yInput.type = 'number';
      yInput.step = '0.1';
      yInput.value = Number(node.y).toFixed(1);
      yInput.dataset.axis = 'y';
      yInput.setAttribute('aria-label', `${label.textContent} y`);

      row.append(label, xInput, yInput);
      this._roomNodeFields.appendChild(row);
    }
  }

  _roomConfigFromPanel() {
    if (!this._roomConfig) return null;
    const nodes = this._roomConfig.nodes.map((node) => {
      const rows = [...(this._roomNodeFields?.querySelectorAll('.room-node-field') || [])];
      const row = rows.find((candidate) => candidate.dataset.nodeId === String(node.id));
      const x = Number(row?.querySelector('[data-axis="x"]')?.value);
      const y = Number(row?.querySelector('[data-axis="y"]')?.value);
      return {
        ...node,
        x: Number.isFinite(x) ? x : node.x,
        y: Number.isFinite(y) ? y : node.y,
      };
    });

    return {
      version: 2,
      room: {
        shape: 'polygon',
        boundary: this._roomBoundary(this._roomConfig),
      },
      nodes,
    };
  }

  _roomToScenePosition(x, y, elevation = 0) {
    return [
      Number(x || 0),
      elevation,
      Number(y || 0),
    ];
  }

  _clearObjectGroup(group) {
    if (!group) return;
    while (group.children.length) {
      const child = group.children[0];
      group.remove(child);
      child.traverse?.((obj) => {
        obj.geometry?.dispose?.();
        if (Array.isArray(obj.material)) {
          obj.material.forEach((mat) => {
            mat.map?.dispose?.();
            mat.dispose?.();
          });
        } else {
          obj.material?.map?.dispose?.();
          obj.material?.dispose?.();
        }
      });
    }
  }

  _createTextSprite(text, options = {}) {
    const canvas = document.createElement('canvas');
    canvas.width = options.width || 256;
    canvas.height = options.height || 96;
    const texture = new THREE.CanvasTexture(canvas);
    if ('colorSpace' in texture) texture.colorSpace = THREE.SRGBColorSpace;
    const material = new THREE.SpriteMaterial({
      map: texture,
      transparent: true,
      depthWrite: false,
      depthTest: false,
    });
    const sprite = new THREE.Sprite(material);
    sprite.userData.labelCanvas = canvas;
    sprite.userData.labelTexture = texture;
    sprite.userData.labelOptions = options;
    this._updateTextSprite(sprite, text, options.color);
    return sprite;
  }

  _updateTextSprite(sprite, text, color = null) {
    const canvas = sprite?.userData?.labelCanvas;
    const texture = sprite?.userData?.labelTexture;
    const options = sprite?.userData?.labelOptions || {};
    if (!canvas || !texture) return;
    const ctx = canvas.getContext('2d');
    const lines = String(text || '').split('\n');
    ctx.clearRect(0, 0, canvas.width, canvas.height);
    ctx.fillStyle = options.background || 'rgba(8, 16, 28, 0.72)';
    ctx.fillRect(0, 0, canvas.width, canvas.height);
    ctx.strokeStyle = options.border || 'rgba(255, 255, 255, 0.18)';
    ctx.lineWidth = 2;
    ctx.strokeRect(1, 1, canvas.width - 2, canvas.height - 2);
    ctx.fillStyle = color || options.color || '#e8ece0';
    ctx.font = options.font || '600 22px JetBrains Mono, Consolas, monospace';
    ctx.textAlign = 'center';
    ctx.textBaseline = 'middle';
    const lineHeight = options.lineHeight || 28;
    const start = canvas.height / 2 - ((lines.length - 1) * lineHeight) / 2;
    lines.forEach((line, index) => ctx.fillText(line, canvas.width / 2, start + index * lineHeight));
    texture.needsUpdate = true;
  }

  _rebuildRoomConfigScene(data) {
    this._syncNodeObjects(data);
  }

  _nodeIdOf(node) {
    const id = Number(node?.node_id ?? node?.id);
    return Number.isFinite(id) ? id : null;
  }

  _roomConfigNodeById(id) {
    if (!this._roomConfig?.nodes) return null;
    return this._roomConfig.nodes.find((node) => Number(node.id) === Number(id)) || null;
  }

  _defaultRoomPointForNode(id) {
    const bounds = this._roomConfigBounds(this._roomConfig || DEFAULT_ROOM_CONFIG);
    const corners = [
      [bounds.minX, bounds.minY],
      [bounds.maxX, bounds.minY],
      [bounds.maxX, bounds.maxY],
      [bounds.minX, bounds.maxY],
      [bounds.minX + bounds.width / 2, bounds.maxY],
      [bounds.minX + bounds.width / 2, bounds.minY],
    ];
    const slot = Math.max(0, Math.floor(Number(id || 1)) - 1);
    const point = corners[slot % corners.length];
    return { x: point[0], y: point[1] };
  }

  _scenePositionForNode(id, node = null) {
    const cfg = this._roomConfigNodeById(id);
    if (cfg) return this._roomToScenePosition(cfg.x, cfg.y, NODE_ELEVATION_M);
    const explicit = this._positionOf(node);
    if (explicit) return [explicit[0], NODE_ELEVATION_M, explicit[2]];
    const fallback = this._defaultRoomPointForNode(id);
    return this._roomToScenePosition(fallback.x, fallback.y, NODE_ELEVATION_M);
  }

  _nodeRangeRadius(node = null) {
    const direct = Number(node?.coverage?.radius_m ?? node?.coverage_radius_m ?? node?.range_m ?? node?.max_range_m);
    if (Number.isFinite(direct) && direct > 0) return Math.max(0.45, direct);
    const span = Math.max(
      this._roomConfigBounds(this._roomConfig || DEFAULT_ROOM_CONFIG).width,
      this._roomConfigBounds(this._roomConfig || DEFAULT_ROOM_CONFIG).depth,
      this._roomSize.width,
      this._roomSize.depth,
      2,
    );
    return Math.min(Math.max(span * 0.32, 0.75), 1.8);
  }

  _ensureNodeObject(id) {
    this._ensureRoomConfigGroup();
    const existing = this.nodeObjects.get(id);
    if (existing) return existing;

    const material = new THREE.MeshStandardMaterial({
      color: NODE_INACTIVE_COLOR,
      emissive: NODE_INACTIVE_COLOR,
      emissiveIntensity: 0.18,
      roughness: 0.45,
      transparent: true,
      opacity: 0.86,
    });
    const mesh = new THREE.Mesh(
      new THREE.BoxGeometry(NODE_CUBE_SIZE_M, NODE_CUBE_SIZE_M, NODE_CUBE_SIZE_M),
      material,
    );
    mesh.name = `esp32-node-${id}`;
    mesh.castShadow = true;
    mesh.receiveShadow = true;
    this._roomConfigGroup.add(mesh);

    const labelSprite = this._createTextSprite(`N${id}`, {
      width: 128,
      height: 56,
      font: '700 24px JetBrains Mono, Consolas, monospace',
      color: '#dce6ef',
      background: 'rgba(8, 16, 28, 0.64)',
    });
    labelSprite.name = `esp32-node-${id}-label`;
    labelSprite.scale.set(0.46, 0.18, 1);
    this._roomConfigGroup.add(labelSprite);

    const rangeMat = new THREE.MeshBasicMaterial({
      color: NODE_INACTIVE_COLOR,
      transparent: true,
      opacity: 0.08,
      side: THREE.DoubleSide,
      depthWrite: false,
    });
    const range = new THREE.Mesh(new THREE.CylinderGeometry(1, 1, 0.012, 72), rangeMat);
    range.name = `esp32-node-${id}-range`;
    this._roomConfigGroup.add(range);
    this._nodeRangeObjects.set(id, range);

    const entry = { mesh, labelSprite };
    this.nodeObjects.set(id, entry);
    return entry;
  }

  _disposeNodeObject(id) {
    const entry = this.nodeObjects.get(id);
    const range = this._nodeRangeObjects.get(id);
    for (const object of [entry?.mesh, entry?.labelSprite, range]) {
      if (!object) continue;
      this._roomConfigGroup?.remove(object);
      object.geometry?.dispose?.();
      const materials = Array.isArray(object.material) ? object.material : [object.material];
      for (const mat of materials) {
        mat?.map?.dispose?.();
        mat?.dispose?.();
      }
    }
    this.nodeObjects.delete(id);
    this._nodeRangeObjects.delete(id);
    this._nodeRuntimeState.delete(id);
  }

  _placeNodeObject(id, position, node = null) {
    const entry = this._ensureNodeObject(id);
    const radius = this._nodeRangeRadius(node);
    const y = Math.max(Number(position?.[1]) || 0, NODE_CUBE_SIZE_M / 2);
    entry.mesh.position.set(position[0], y, position[2]);
    entry.labelSprite.position.set(position[0], y + 0.36, position[2]);
    const range = this._nodeRangeObjects.get(id);
    if (range) {
      range.position.set(position[0], 0.006, position[2]);
      range.scale.set(radius, 1, radius);
    }
    entry.mesh.visible = true;
    entry.labelSprite.visible = true;
    if (range) range.visible = true;
  }

  _nodeObjectPosition(id) {
    const mesh = this.nodeObjects.get(Number(id))?.mesh;
    if (!mesh) return null;
    return [mesh.position.x, mesh.position.y, mesh.position.z];
  }

  _nodeIsLive(node) {
    const status = String(node?.status || node?.health_status || (node?.active === false ? 'offline' : 'live')).toLowerCase();
    return node?.active !== false && !['offline', 'stale', 'sync_only', 'no_nodes', 'connecting'].includes(status);
  }

  _markRuntimeNodesInactive() {
    for (const state of this._nodeRuntimeState.values()) state.active = false;
  }

  _syncRuntimeNodes(data) {
    if (!data) return;
    const status = String(data.system_status || this._connectionState || '').toLowerCase();
    if (['offline', 'no_nodes', 'connecting'].includes(status)) {
      this._markRuntimeNodesInactive();
      return;
    }

    const receivedAt = this._numberOrNull(data._receivedAt) ?? this._lastLiveAt ?? 0;
    for (const node of data.nodes || []) {
      const id = this._nodeIdOf(node);
      if (id == null) continue;
      const live = this._nodeIsLive(node);
      const previous = this._nodeRuntimeState.get(id) || {};
      this._nodeRuntimeState.set(id, {
        ...previous,
        node,
        active: live,
        lastSeenAt: live ? (receivedAt || performance.now()) : (previous.lastSeenAt || 0),
      });
    }
  }

  _nodeActiveFromRuntime(id) {
    const state = this._nodeRuntimeState.get(id);
    if (!state?.active || !state.lastSeenAt) return false;
    return performance.now() - state.lastSeenAt < NODE_ACTIVE_WINDOW_MS;
  }

  _updateNodeVisual(id) {
    const entry = this.nodeObjects.get(id);
    if (!entry) return;
    const active = this._nodeActiveFromRuntime(id);
    const color = active ? NODE_ACTIVE_COLOR : NODE_INACTIVE_COLOR;
    entry.mesh.material.color.setHex(color);
    entry.mesh.material.emissive?.setHex(color);
    entry.mesh.material.opacity = active ? 1 : 0.86;
    this._updateTextSprite(entry.labelSprite, `N${id}`, active ? '#dbeafe' : '#cbd5e1');
    const range = this._nodeRangeObjects.get(id);
    if (range?.material) {
      range.material.color.setHex(color);
      range.material.opacity = active ? 0.16 : 0.08;
    }
  }

  _syncNodesFromRoomConfig(desiredIds) {
    for (const node of this._roomConfig?.nodes || []) {
      const id = this._nodeIdOf(node);
      if (id == null) continue;
      desiredIds.add(id);
      this._placeNodeObject(id, this._scenePositionForNode(id, node), node);
    }
  }

  _syncNodeObjects(data) {
    if (!this._roomConfig) return;
    this._ensureRoomConfigGroup();
    this._syncRuntimeNodes(data);

    const desiredIds = new Set();
    this._syncNodesFromRoomConfig(desiredIds);

    for (const node of data?.nodes || []) {
      const id = this._nodeIdOf(node);
      if (id == null) continue;
      desiredIds.add(id);
      this._placeNodeObject(id, this._scenePositionForNode(id, node), node);
    }

    for (const id of this._nodeRuntimeState.keys()) desiredIds.add(id);
    for (const id of desiredIds) {
      const runtimeNode = this._nodeRuntimeState.get(id)?.node || null;
      if (!this.nodeObjects.has(id)) this._placeNodeObject(id, this._scenePositionForNode(id, runtimeNode), runtimeNode);
      this._updateNodeVisual(id);
    }

    for (const id of [...this.nodeObjects.keys()]) {
      if (!desiredIds.has(id)) this._disposeNodeObject(id);
    }
  }

  _syncRoomConfigNodes(data) {
    this._syncNodeObjects(data);
  }

  _presenceColor(confidence) {
    if (confidence < 0.4) return 0xff3040;
    if (confidence < 0.7) return 0xffb020;
    return 0x00d878;
  }

  _upsertPresenceSilhouette(id) {
    let entry = this._presenceSilhouettes.get(id);
    if (entry) return entry;
    this._ensurePresenceGroup();

    const group = new THREE.Group();
    const bodyMat = new THREE.MeshStandardMaterial({
      color: C.greenGlow,
      emissive: C.greenGlow,
      emissiveIntensity: 0.18,
      roughness: 0.36,
      metalness: 0.08,
    });
    const headMat = bodyMat.clone();
    const body = new THREE.Mesh(new THREE.CylinderGeometry(0.25, 0.25, PERSON_HEIGHT_M, 24), bodyMat);
    body.position.y = PERSON_HEIGHT_M / 2;
    body.castShadow = true;
    group.add(body);

    const head = new THREE.Mesh(new THREE.SphereGeometry(0.15, 24, 16), headMat);
    head.position.y = PERSON_HEIGHT_M + 0.15;
    head.castShadow = true;
    group.add(head);

    const label = this._createTextSprite('', {
      width: 320,
      height: 92,
      font: '600 20px JetBrains Mono, Consolas, monospace',
      lineHeight: 26,
      background: 'rgba(8, 16, 28, 0.76)',
    });
    label.scale.set(1.08, 0.34, 1);
    label.position.y = PERSON_HEIGHT_M + 0.48;
    group.add(label);

    this._presenceGroup.add(group);
    entry = { group, bodyMat, headMat, label, lastLabel: '', hasPosition: false };
    this._presenceSilhouettes.set(id, entry);
    return entry;
  }

  _removePresenceSilhouette(id, entry) {
    this._presenceGroup?.remove(entry.group);
    entry.group.traverse((obj) => {
      obj.geometry?.dispose?.();
      if (Array.isArray(obj.material)) {
        obj.material.forEach((mat) => {
          mat.map?.dispose?.();
          mat.dispose?.();
        });
      } else {
        obj.material?.map?.dispose?.();
        obj.material?.dispose?.();
      }
    });
    this._presenceSilhouettes.delete(id);
  }

  _updatePresenceSilhouettes(data, elapsed) {
    const persons = Array.isArray(data?.persons) ? data.persons : [];
    const activeIds = new Set();
    const frameKey = String(this._sceneTimestampMs(data));
    const evidence = data?.count_evidence && typeof data.count_evidence === 'object' ? data.count_evidence : {};
    const activeNodeCount = this._integerOrZero(evidence.supporting_nodes ?? evidence.active_nodes ?? data?.node_count);
    const localizationConfidence = activeNodeCount >= 4 ? 'élevée' : activeNodeCount === 3 ? 'moyenne' : 'faible';

    persons.forEach((person, index) => {
      if (person.is_present === false) return;
      const id = this._personIdentity(person, index);
      activeIds.add(id);
      const entry = this._upsertPresenceSilhouette(id);
      const position = this._personScenePosition(person);
      if (!position) return;

      const confidence = Math.max(0, Math.min(1, Number(person.confidence) || 0));
      const motionEnergy = Math.max(0, Math.min(1, Number(person.motion_energy ?? person.motion_score / 100) || 0));
      const color = this._presenceColor(confidence);
      let positionState = this._personPositions.get(id);
      if (!positionState) {
        positionState = {
          history: [],
          displayed: { x: position[0], y: position[2] },
          target: { x: position[0], y: position[2] },
          lastFrameKey: null,
        };
        this._personPositions.set(id, positionState);
      }

      if (positionState.lastFrameKey !== frameKey) {
        positionState.history.push({ x: position[0], y: position[2] });
        if (positionState.history.length > 5) positionState.history.shift();
        positionState.lastFrameKey = frameKey;
      }

      let totalWeight = 0;
      let sumX = 0;
      let sumY = 0;
      positionState.history.forEach((point, historyIndex) => {
        const weight = historyIndex + 1;
        sumX += point.x * weight;
        sumY += point.y * weight;
        totalWeight += weight;
      });
      positionState.displayed = totalWeight > 0
        ? { x: sumX / totalWeight, y: sumY / totalWeight }
        : { x: position[0], y: position[2] };
      positionState.target = { ...positionState.displayed };

      const yOffset = motionEnergy > 0.1 ? Math.sin(elapsed * 3.2 + index * 0.7) * 0.045 : 0;
      const targetY = yOffset;
      if (!entry.hasPosition) {
        entry.group.position.set(positionState.target.x, targetY, positionState.target.y);
        entry.hasPosition = true;
      } else {
        entry.group.position.x += (positionState.target.x - entry.group.position.x) * 0.1;
        entry.group.position.z += (positionState.target.y - entry.group.position.z) * 0.1;
        entry.group.position.y = targetY;
      }

      entry.bodyMat.color.setHex(color);
      entry.bodyMat.emissive.setHex(color);
      entry.headMat.color.setHex(color);
      entry.headMat.emissive.setHex(color);

      const confidencePercent = Math.round(confidence * 100);
      const label = `P${index + 1} — ${confidencePercent}%\nConfiance localisation : ${localizationConfidence}`;
      if (entry.lastLabel !== label) {
        this._updateTextSprite(entry.label, label, confidence < 0.4 ? '#ffb3ba' : confidence < 0.7 ? '#ffe4ad' : '#b7ffd4');
        entry.lastLabel = label;
      }
    });

    for (const [id, entry] of [...this._presenceSilhouettes.entries()]) {
      if (!activeIds.has(id)) this._removePresenceSilhouette(id, entry);
    }
    for (const id of [...this._personPositions.keys()]) {
      if (!activeIds.has(id)) this._personPositions.delete(id);
    }
  }
  // ---- WiFi Waves ----

  _buildWifiWaves() {
    this._wifiWaves = [];
  }

  _ensureWaveSource(id, position, active, scale = 1) {
    this._ensureTopologyGroups();
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

  _fetchEnvironment() {
    fetch('/api/v1/environment', { cache: 'no-store' })
      .then(r => r.ok ? r.json() : Promise.reject())
      .then(env => {
        this._environment = env;
        this._syncRoomGeometry(env);
        this._recomputeSceneFrame(env, env.nodes || []);
        this._frameCameraToSensors(true);
        this._setEnvironmentNotice(false);
      })
      .catch(() => {
        this._environment = null;
        this._setEnvironmentNotice(true);
        this._clearTopology();
      });
  }

  _mergeNodes(liveData) {
    const env = this._environment;
    if (!env) return [];
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

  _setEnvironmentNotice(visible) {
    if (!visible) {
      if (this._environmentNotice?.parentNode) this._environmentNotice.parentNode.removeChild(this._environmentNotice);
      this._environmentNotice = null;
      return;
    }
    if (!this._environmentNotice) {
      const notice = document.createElement('div');
      notice.className = 'environment-unavailable';
      notice.textContent = 'Configuration environnement indisponible: verifier le master et /api/v1/environment.';
      document.body.appendChild(notice);
      this._environmentNotice = notice;
    }
  }

  _clearTopology() {
    for (const [, entry] of this._deviceMeshes) entry.group.visible = false;
    for (const [, line] of this._linkMeshes) line.visible = false;
    this._hideImpactMarkers();
    for (const [, entry] of this._coverageMeshes) entry.group.visible = false;
    for (const waves of this._wifiWaves) {
      waves.active = false;
      for (const shell of waves.shells) shell.mesh.visible = false;
    }
    this._obstacleSummary = null;
    this._hud?.updateObstacleAttenuation?.();
  }

  _parseVector3(value) {
    if (Array.isArray(value) && value.length >= 3) {
      const parsed = [Number(value[0]), Number(value[1]), Number(value[2])];
      return parsed.every(Number.isFinite) ? parsed : null;
    }
    if (value && typeof value === 'object') {
      const parsed = [Number(value.x), Number(value.y), Number(value.z)];
      return parsed.every(Number.isFinite) ? parsed : null;
    }
    return null;
  }

  _personScenePosition(person) {
    const directX = this._numberOrNull(person?.x ?? person?.position?.x ?? person?.location?.x);
    const directY = this._numberOrNull(person?.y ?? person?.position?.y ?? person?.location?.y);
    if (directX != null && directY != null) return [directX, 0, directY];
    const metric = this._parseVector3(person?.position_m);
    if (metric) return [metric[0], 0, metric[2]];
    const vector = this._parseVector3(person?.position);
    return vector ? [vector[0], 0, vector[2]] : null;
  }

  _rawPositionOf(entity, { requirePositionM = false } = {}) {
    const metric = this._parseVector3(entity?.position_m);
    if (metric) return metric;
    if (requirePositionM) return null;
    return this._parseVector3(entity?.position);
  }

  _positionOf(entity, options = {}) {
    const raw = this._rawPositionOf(entity, options);
    if (!raw) return null;
    return [
      raw[0] - this._sensorOrigin[0],
      raw[1],
      raw[2] - this._sensorOrigin[2],
    ];
  }

  _recomputeSceneFrame(env, nodes = []) {
    const room = this._roomDimensions(env);
    const sourceNodes = nodes.length ? nodes : (env?.nodes || []);
    const sensors = [...(env?.access_points || []), ...sourceNodes];
    const positions = sensors
      .map(sensor => this._rawPositionOf(sensor))
      .filter(Boolean);

    if (!positions.length) {
      this._sensorOrigin = [0, 0, 0];
      this._sensorBounds = { width: room.width, height: room.height, depth: room.depth };
      this._sensorTargetY = Math.min(Math.max(room.height * 0.5, 0.8), room.height);
      this._sceneTarget.set(0, this._sensorTargetY, 0);
      return;
    }

    const xs = positions.map(pos => pos[0]);
    const ys = positions.map(pos => pos[1]);
    const zs = positions.map(pos => pos[2]);
    const minX = Math.min(...xs);
    const maxX = Math.max(...xs);
    const minY = Math.min(...ys);
    const maxY = Math.max(...ys);
    const minZ = Math.min(...zs);
    const maxZ = Math.max(...zs);
    const avgY = ys.reduce((sum, value) => sum + value, 0) / ys.length;
    this._sensorOrigin = [
      xs.reduce((sum, value) => sum + value, 0) / xs.length,
      0,
      zs.reduce((sum, value) => sum + value, 0) / zs.length,
    ];
    this._sensorBounds = {
      width: Math.max(maxX - minX, room.width, 2),
      height: Math.max(maxY - minY, room.height, 1),
      depth: Math.max(maxZ - minZ, room.depth, 2),
    };
    this._sensorTargetY = Math.min(Math.max(avgY, 0.8), Math.max(room.height, 0.8));
    this._sceneTarget.set(0, this._sensorTargetY, 0);
  }

  _frameCameraToSensors(force = false) {
    if (this._cameraFramedToSensors && !force) return;
    const span = Math.max(this._sensorBounds.width, this._sensorBounds.depth, 4);
    const distance = Math.min(Math.max(span * 1.35, 6), 50);
    const targetY = this._sensorTargetY || 1.2;
    const cameraY = targetY + Math.max(2.8, this._sensorBounds.height * 0.6, span * 0.35);
    this._camera.position.set(distance * 0.72, cameraY, distance);
    this._controls.target.copy(this._sceneTarget);
    this._controls.maxDistance = Math.max(25, distance * 2.2);
    this._controls.update();
    this._cameraFramedToSensors = true;
  }

  _sensingTypeOf(frame) {
    return String(frame?.type || frame?.msg_type || '').toLowerCase();
  }

  _numberOrNull(value) {
    const n = Number(value);
    return Number.isFinite(n) ? n : null;
  }

  _integerOrZero(value) {
    const n = Number(value);
    return Number.isFinite(n) ? Math.max(0, Math.floor(n)) : 0;
  }

  _countEvidence(frame) {
    const evidence = frame?.count_evidence && typeof frame.count_evidence === 'object'
      ? frame.count_evidence
      : null;
    const personCount = Array.isArray(frame?.persons) ? frame.persons.length : 0;
    const raw = this._integerOrZero(evidence?.raw_estimated_persons ?? frame?.estimated_persons ?? personCount);
    let rendered = this._integerOrZero(evidence?.rendered_persons ?? evidence?.stable_persons ?? frame?.estimated_persons ?? personCount);
    if (!evidence && rendered === 0 && personCount > 0) rendered = personCount;
    rendered = Math.min(rendered, MAX_SCENE_PERSONS);
    const stable = this._integerOrZero(evidence?.stable_persons ?? rendered);

    return {
      stable_persons: Math.min(stable, MAX_SCENE_PERSONS),
      raw_estimated_persons: raw,
      rendered_persons: rendered,
      active_nodes: this._integerOrZero(evidence?.active_nodes ?? frame?.nodes?.length ?? 0),
      supporting_nodes: this._integerOrZero(evidence?.supporting_nodes ?? 0),
      ambiguous: Boolean(evidence?.ambiguous || raw > rendered),
      reason: String(evidence?.reason || (raw > rendered ? 'ambiguous_multipath' : 'stable')),
    };
  }

  _isPresenceHold(data) {
    return String(data?.count_evidence?.reason || '').toLowerCase() === 'presence_hold';
  }

  _personIdentity(person, index) {
    const id = person?.id ?? person?.track_id ?? person?.person_id;
    return id == null || id === '' ? `person_${index + 1}` : String(id);
  }

  _personDedupePosition(person) {
    return this._personScenePosition(person);
  }

  _personConfidence(person) {
    const confidence = Number(person?.confidence ?? person?.tracking_confidence ?? person?.score ?? 0);
    return Number.isFinite(confidence) ? confidence : 0;
  }

  _presencePerson(person, index) {
    const next = {
      ...person,
      id: this._personIdentity(person, index),
    };
    delete next.pose;
    delete next.posture;
    delete next.pose_source;
    delete next.fallProgress;
    delete next.fall_progress;
    delete next.fall_detected;
    delete next.keypoints;
    delete next.keypoints_m;
    return next;
  }

  _dedupePersons(persons, renderLimit) {
    const byId = new Map();
    persons.forEach((person, index) => {
      const id = this._personIdentity(person, index);
      const current = byId.get(id);
      if (!current || this._personConfidence(person) >= this._personConfidence(current)) {
        byId.set(id, { ...person, id });
      }
    });

    const merged = [];
    for (const person of byId.values()) {
      const pos = this._personDedupePosition(person);
      const closeIndex = pos
        ? merged.findIndex((existing) => {
          const other = this._personDedupePosition(existing);
          if (!other) return false;
          return Math.hypot(pos[0] - other[0], pos[1] - other[1], pos[2] - other[2]) < PERSON_DEDUPE_RADIUS_M;
        })
        : -1;
      if (closeIndex < 0) {
        merged.push(person);
      } else if (this._personConfidence(person) >= this._personConfidence(merged[closeIndex])) {
        merged[closeIndex] = person;
      }
    }

    const limit = Math.max(0, Math.min(MAX_SCENE_PERSONS, this._integerOrZero(renderLimit)));
    return limit > 0 ? merged.slice(0, limit) : [];
  }

  _sceneTimestampMs(data) {
    const explicit = this._numberOrNull(data?.timestamp_ms);
    if (explicit != null) return explicit;
    const seconds = this._numberOrNull(data?.timestamp);
    if (seconds != null) return seconds * 1000;
    return performance.now();
  }

  _edgeVitalsFromFrame(frame) {
    if (!frame || typeof frame !== 'object') return null;
    const br = this._numberOrNull(frame.breathing_rate_bpm ?? frame.breathing_bpm);
    const hr = this._numberOrNull(frame.heart_rate_bpm ?? frame.heartrate_bpm ?? frame.hr_proxy_bpm);
    const confidence = this._numberOrNull(frame.presence_score ?? frame.signal_quality ?? frame.confidence);
    if (br == null && hr == null && confidence == null) return null;
    return {
      ...(br != null && br > 0 ? { breathing_rate_bpm: br } : {}),
      ...(hr != null && hr > 0 ? { heart_rate_bpm: hr } : {}),
      breathing_confidence: confidence ?? 0,
      heartbeat_confidence: confidence ?? 0,
      signal_quality: confidence ?? 0,
    };
  }

  _updateScenarioProps(data) {
    this._currentScenario = data?.scenario || this._currentScenario || null;
    if (!this._currentScenario && !this._scenarioProps) return;
    if (!this._scenarioProps) this._scenarioProps = new ScenarioProps(this._scene);
    this._scenarioProps.update(data, this._currentScenario);
  }

  _mergeEdgeVitals(frame, edgeFrame) {
    const edgeVitals = this._edgeVitalsFromFrame(edgeFrame);
    if (!edgeVitals) return frame;
    return {
      ...frame,
      vital_signs: {
        ...(frame?.vital_signs || {}),
        ...edgeVitals,
      },
    };
  }

  _ingestSocketFrame(frame) {
    const type = this._sensingTypeOf(frame);
    if (type === 'presence_update') {
      const normalized = this._normalizePresenceFrame(frame);
      if (!normalized) return;
      const status = normalized.system_status === 'no_nodes' ? 'no_nodes' : 'live';
      normalized._receivedAt = performance.now();
      this._liveData = normalized;
      this._lastLiveAt = normalized._receivedAt;
      this._setSceneStatus(status);
      this._hud.updateSourceBadge(status, this._ws);
      this._updateScenarioProps(normalized);
      this._syncTopology(normalized);
      this._syncRoomConfigNodes(normalized);
      return;
    }

    if (type === 'sensing_update') {
      const normalized = this._normalizeSensingFrame(frame);
      if (!normalized) return;
      const status = String(normalized.system_status || '').toLowerCase() === 'no_nodes' ? 'no_nodes' : 'live';
      normalized._receivedAt = performance.now();
      this._liveData = normalized;
      this._lastLiveAt = normalized._receivedAt;
      this._setSceneStatus(status);
      this._hud.updateSourceBadge(status, this._ws);
      this._updateScenarioProps(normalized);
      this._syncTopology(normalized);
      this._syncRoomConfigNodes(normalized);
      return;
    }

    if (type === 'edge_vitals' || type === 'edge_fused_vitals') {
      this._lastEdgeVitals = frame;
      if (this._liveData) {
        this._liveData = this._mergeEdgeVitals(this._liveData, frame);
      }
    }
  }

  _normalizeSensingFrame(rawFrame) {
    if (!rawFrame || typeof rawFrame !== 'object') return null;
    const frame = this._lastEdgeVitals ? this._mergeEdgeVitals(rawFrame, this._lastEdgeVitals) : { ...rawFrame };
    const rawPersons = Array.isArray(frame.persons) ? frame.persons : [];
    let persons = rawPersons.slice(0, MAX_SCENE_PERSONS).map((person, index) => this._presencePerson(person, index));

    let evidence = this._countEvidence({ ...frame, persons });
    const hasCountEvidence = Boolean(frame.count_evidence && typeof frame.count_evidence === 'object');
    const renderLimit = hasCountEvidence
      ? evidence.rendered_persons
      : Math.max(evidence.rendered_persons, persons.length);
    persons = this._dedupePersons(persons, renderLimit);
    const renderedPersons = hasCountEvidence ? evidence.rendered_persons : (persons.length || evidence.rendered_persons);
    const countEvidence = {
      ...evidence,
      rendered_persons: renderedPersons,
      stable_persons: Math.min(evidence.stable_persons || renderedPersons, renderedPersons),
      ambiguous: Boolean(evidence.ambiguous || evidence.raw_estimated_persons > renderedPersons),
    };
    const classification = {
      ...(frame.classification || {}),
    };
    if (persons.length && !classification.presence) {
      classification.presence = true;
      classification.motion_level = classification.motion_level || 'present';
      classification.confidence = classification.confidence ?? persons[0]?.confidence ?? 0.5;
    }

    return {
      ...frame,
      type: 'sensing_update',
      msg_type: frame.msg_type || 'sensing_update',
      nodes: this._nodesFromWsFrame(frame, persons),
      persons,
      estimated_persons: renderedPersons,
      count_evidence: countEvidence,
      classification,
    };
  }

  _nodesFromWsFrame(frame, persons = []) {
    if (Array.isArray(frame?.nodes) && frame.nodes.length) return frame.nodes;
    const nodeCount = this._integerOrZero(frame?.node_count);
    if (!nodeCount || !Array.isArray(persons) || !persons.length) return [];
    return (this._roomConfig?.nodes || DEFAULT_ROOM_CONFIG.nodes).slice(0, nodeCount).map((node) => {
      const position = this._roomToScenePosition(node.x, node.y, NODE_ELEVATION_M);
      return {
        id: node.id,
        node_id: node.id,
        label: node.label || `Node ${node.id}`,
        active: true,
        status: 'live',
        position,
        position_m: position.slice(),
      };
    });
  }

  _normalizePresenceFrame(rawFrame) {
    if (!rawFrame || typeof rawFrame !== 'object') return null;
    const rawPersons = Array.isArray(rawFrame.persons) ? rawFrame.persons : [];
    const persons = rawPersons
      .filter((person) => person?.is_present !== false)
      .slice(0, MAX_SCENE_PERSONS)
      .map((person, index) => {
        const x = this._numberOrNull(person?.x) ?? 0;
        const y = this._numberOrNull(person?.y) ?? 0;
        const confidence = Math.max(0, Math.min(1, this._numberOrNull(person?.confidence) ?? 0));
        const motionEnergy = Math.max(0, Math.min(1, this._numberOrNull(person?.motion_energy) ?? 0));
        const br = this._numberOrNull(person?.breathing_bpm);
        const hr = this._numberOrNull(person?.heart_rate_bpm);
        const position = this._roomToScenePosition(x, y, 0);

        return {
          id: this._personIdentity(person, index),
          x,
          y,
          confidence,
          breathing_bpm: br,
          heart_rate_bpm: hr,
          is_present: true,
          motion_energy: motionEnergy,
          motion_score: motionEnergy * 100,
          position,
          position_m: position.slice(),
          position_source: 'observatory_layout',
          vitals: {
            ...(br != null ? { breathing_bpm: br } : {}),
            ...(hr != null ? { heart_rate_bpm: hr } : {}),
          },
          vital_signs: {
            ...(br != null ? { breathing_rate_bpm: br } : {}),
            ...(hr != null ? { heart_rate_bpm: hr } : {}),
          },
        };
      });

    const nodeCount = this._integerOrZero(rawFrame.node_count);
    const nodes = (this._roomConfig?.nodes || DEFAULT_ROOM_CONFIG.nodes).map((node, index) => {
      const active = String(rawFrame.system_status || '').toLowerCase() !== 'no_nodes' && index < nodeCount;
      const position = this._roomToScenePosition(node.x, node.y, NODE_ELEVATION_M);
      return {
        id: node.id,
        node_id: node.id,
        label: node.label || `Node ${node.id}`,
        active,
        status: active ? 'online' : 'offline',
        position,
        position_m: position.slice(),
      };
    });

    const maxConfidence = persons.reduce((best, person) => Math.max(best, person.confidence || 0), 0);
    const avgMotion = persons.length
      ? persons.reduce((sum, person) => sum + (person.motion_energy || 0), 0) / persons.length
      : 0;
    const primaryVitals = persons.find((person) => person.breathing_bpm != null || person.heart_rate_bpm != null);
    const systemStatus = String(rawFrame.system_status || (nodeCount > 0 ? 'live' : 'no_nodes')).toLowerCase();

    return {
      type: 'sensing_update',
      msg_type: 'sensing_update',
      source: 'ws_presence',
      system_status: systemStatus,
      timestamp_ms: this._numberOrNull(rawFrame.timestamp_ms) ?? Date.now(),
      node_count: nodeCount,
      nodes,
      persons,
      estimated_persons: persons.length,
      count_evidence: {
        stable_persons: persons.length,
        raw_estimated_persons: persons.length,
        rendered_persons: persons.length,
        active_nodes: nodeCount,
        supporting_nodes: nodeCount,
        ambiguous: false,
        reason: 'presence_update',
      },
      features: {
        mean_rssi: 0,
        variance: 0,
        motion_band_power: avgMotion,
      },
      classification: {
        presence: persons.length > 0,
        motion_level: avgMotion > 0.15 ? 'active' : (persons.length ? 'present' : 'absent'),
        confidence: maxConfidence,
      },
      signal_field: null,
      vital_signs: primaryVitals?.vital_signs || null,
    };
  }

  _sceneFrameData(data) {
    if (!data) return data;
    const inputPersons = Array.isArray(data.persons) ? data.persons.slice(0, MAX_SCENE_PERSONS) : [];
    let persons = inputPersons.map((person, index) => {
      const position = this._personScenePosition(person);
      if (!position) return null;
      return this._presencePerson({
        ...person,
        position,
        ...(person.position_m ? { position_m: position } : {}),
      }, index);
    }).filter(Boolean);
    let evidence = this._countEvidence({ ...data, persons });
    const hasCountEvidence = Boolean(data.count_evidence && typeof data.count_evidence === 'object');
    const renderLimit = hasCountEvidence
      ? evidence.rendered_persons
      : Math.max(evidence.rendered_persons, persons.length);
    persons = this._dedupePersons(persons, renderLimit);
    const renderedPersons = hasCountEvidence ? evidence.rendered_persons : (persons.length || evidence.rendered_persons);
    const countEvidence = {
      ...evidence,
      rendered_persons: renderedPersons,
      stable_persons: Math.min(evidence.stable_persons || renderedPersons, renderedPersons),
      ambiguous: Boolean(evidence.ambiguous || evidence.raw_estimated_persons > renderedPersons),
    };
    return {
      ...data,
      persons,
      estimated_persons: renderedPersons,
      count_evidence: countEvidence,
      classification: {
        ...(data.classification || {}),
        presence: Boolean(data.classification?.presence || persons.length),
      },
    };
  }

  _upsertDevice(key, kind, label, position, active) {
    if (kind === 'node') return null;
    this._ensureTopologyGroups();
    let entry = this._deviceMeshes.get(key);
    if (!entry) {
      const group = new THREE.Group();
      const color = C.blueSignal;
      const bodyGeo = new THREE.BoxGeometry(0.54, 0.16, 0.34);
      const mat = new THREE.MeshBasicMaterial({ color, transparent: true, opacity: 0.9 });
      const body = new THREE.Mesh(bodyGeo, mat);
      body.castShadow = true;
      group.add(body);

      for (let i = -1; i <= 1; i++) {
        const ant = new THREE.Mesh(
          new THREE.CylinderGeometry(0.012, 0.012, 0.38, 8),
          new THREE.MeshBasicMaterial({ color: 0x9fb6c8, transparent: true, opacity: 0.75 })
        );
        ant.position.set(i * 0.16, 0.28, 0);
        ant.rotation.z = i * 0.18;
        group.add(ant);
      }

      const beacon = new THREE.Mesh(
        new THREE.SphereGeometry(0.055, 16, 12),
        new THREE.MeshBasicMaterial({ color, transparent: true, opacity: 1 })
      );
      beacon.position.y = 0.2;
      group.add(beacon);

      this._topologyGroup.add(group);
      entry = { group, mat, beacon, label };
      this._deviceMeshes.set(key, entry);
    }
    entry.group.position.set(position[0], position[1], position[2]);
    entry.mat.opacity = active ? 0.9 : 0.28;
    entry.beacon.material.opacity = active ? 1 : 0.28;
    entry.group.visible = true;
    return entry;
  }

  _upsertLink(key, from, to, active, collision = null) {
    this._ensureTopologyGroups();
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
    const obstructed = active && Boolean(collision);
    const nextBlending = obstructed ? THREE.AdditiveBlending : THREE.NormalBlending;
    line.geometry.setFromPoints([
      new THREE.Vector3(from[0], from[1], from[2]),
      new THREE.Vector3(to[0], to[1], to[2]),
    ]);
    line.material.opacity = !active ? 0.16 : (obstructed ? 0.95 : 0.55);
    line.material.color.set(!active ? 0x385060 : (obstructed ? C.obstacleHot : C.blueSignal));
    line.material.depthWrite = !obstructed;
    if (line.material.blending !== nextBlending) {
      line.material.blending = nextBlending;
      line.material.needsUpdate = true;
    }
    line.visible = true;
  }

  _linkCollisionTargets() {
    if (!this.settings.obstacles) return [];
    const targets = this._scenarioProps?.getActiveCollisionMeshes?.() || [];
    if (this._floor) targets.push(this._floor);
    return targets;
  }

  _detectLinkCollision(from, to, targets) {
    if (!targets.length) return null;
    const origin = new THREE.Vector3(from[0], from[1], from[2]);
    const target = new THREE.Vector3(to[0], to[1], to[2]);
    const delta = target.sub(origin);
    const length = delta.length();
    if (length <= 0.001) return null;

    const endpointMargin = Math.min(0.18, length * 0.18);
    if (length <= endpointMargin * 2) return null;

    this._linkRaycaster.set(origin, delta.normalize());
    this._linkRaycaster.near = endpointMargin;
    this._linkRaycaster.far = length - endpointMargin;

    const hit = this._linkRaycaster
      .intersectObjects(targets, false)
      .find(intersection => (
        intersection.distance > endpointMargin
        && intersection.distance < length - endpointMargin
      ));
    if (!hit) return null;

    return {
      point: hit.point.clone(),
      obstacleName: this._obstacleNameFromHit(hit),
      distance: hit.distance,
    };
  }

  _obstacleNameFromHit(hit) {
    let obj = hit?.object || null;
    while (obj) {
      if (obj.userData?.obstacleName) return obj.userData.obstacleName;
      obj = obj.parent;
    }
    return 'Obstacle';
  }

  _upsertImpactMarker(key, collision, active) {
    this._ensureTopologyGroups();
    let entry = this._impactMarkers.get(key);
    if (!active || !collision) {
      if (entry) entry.group.visible = false;
      return;
    }

    if (!entry) {
      const group = new THREE.Group();
      const mat = new THREE.MeshStandardMaterial({
        color: C.obstacleHot,
        emissive: C.obstacleHot,
        emissiveIntensity: 2.4,
        roughness: 0.25,
        transparent: true,
        opacity: 0.95,
      });
      const sphere = new THREE.Mesh(new THREE.SphereGeometry(0.095, 20, 14), mat);
      const glow = new THREE.PointLight(C.obstacleHot, 1.6, 1.8, 1.4);
      group.add(sphere);
      group.add(glow);
      this._impactGroup.add(group);
      entry = { group, sphere, glow };
      this._impactMarkers.set(key, entry);
    }

    entry.group.position.copy(collision.point);
    entry.group.userData.obstacleName = collision.obstacleName;
    entry.group.visible = true;
  }

  _hideImpactMarkers(visibleKeys = new Set()) {
    for (const [key, entry] of this._impactMarkers || []) {
      if (!visibleKeys.has(key)) entry.group.visible = false;
    }
  }

  obstacleAttenuation() {
    if (!this.settings.obstacles || !this._obstacleSummary) return null;
    return this._obstacleSummary;
  }

  _refreshObstacleMode() {
    const frame = this._currentData || this._liveData || this._emptyLiveFrame();
    const data = this._sceneFrameData(frame);
    this._updateScenarioProps(data);
    this._syncTopology(data);
  }

  _coverageBand(node, apById, env) {
    const ownBand = String(node.band || node.frequency_band || '').trim();
    if (ownBand) return ownBand;
    const linkedAp = node.linked_ap || env?.links?.find(link => Number(link.node_id) === Number(node.node_id))?.ap_id;
    const ap = linkedAp ? apById.get(linkedAp) : null;
    if (ap?.band) return String(ap.band);
    const channel = Number(node.channel || ap?.channel);
    if (Number.isFinite(channel) && channel > 14) return '5GHz';
    return '2.4GHz';
  }

  _coverageRadiusForBand(band, rangeHintM = null) {
    const normalized = String(band || '').toLowerCase();
    const roomSpan = Math.max(
      this._roomSize.width,
      this._roomSize.depth,
      this._sensorBounds.width,
      this._sensorBounds.depth,
      2,
    );
    let bandScale = 0.86;
    if (normalized.includes('6')) bandScale = 0.72;
    else if (normalized.includes('5')) bandScale = 0.90;
    else if (normalized.includes('2.4') || normalized.includes('2g')) bandScale = 1.08;
    const hinted = Number(rangeHintM);
    const hintRadius = Number.isFinite(hinted) && hinted > 0 ? hinted * 1.15 : 0;
    const radius = Math.max(roomSpan * bandScale, hintRadius, roomSpan * 0.65);
    return Math.min(Math.max(radius, 1.2), Math.max(roomSpan * 1.35, hintRadius));
  }

  _coverageRangeHint(entity, position, apById, env) {
    const direct = Number(entity?.coverage?.radius_m ?? entity?.coverage_radius_m ?? entity?.range_m ?? entity?.max_range_m);
    if (Number.isFinite(direct) && direct > 0) return direct;
    const linkedAp = entity?.linked_ap || env?.links?.find(link => Number(link.node_id) === Number(entity?.node_id))?.ap_id;
    const ap = linkedAp ? apById.get(linkedAp) : null;
    const apPosition = this._positionOf(ap);
    if (position && apPosition) {
      const distance = Math.hypot(position[0] - apPosition[0], position[1] - apPosition[1], position[2] - apPosition[2]);
      if (Number.isFinite(distance) && distance > 0) return distance;
    }
    return Math.max(this._roomSize.width, this._roomSize.depth, this._sensorBounds.width, this._sensorBounds.depth, 2) * 0.85;
  }

  _upsertCoverage(key, position, active, visible, band, rangeHintM = null) {
    this._ensureTopologyGroups();
    let entry = this._coverageMeshes.get(key);
    const radius = this._coverageRadiusForBand(band, rangeHintM);
    const heightScale = Math.min(0.45, Math.max(0.22, this._roomSize.height / Math.max(radius * 5, 1)));
    if (!entry) {
      const domeGeo = new THREE.SphereGeometry(1, 40, 16, 0, Math.PI * 2, 0, Math.PI * 0.54);
      const domeMat = new THREE.MeshBasicMaterial({
        color: C.blueSignal,
        transparent: true,
        opacity: 0,
        side: THREE.DoubleSide,
        blending: THREE.AdditiveBlending,
        depthWrite: false,
      });
      const dome = new THREE.Mesh(domeGeo, domeMat);

      const ringGeo = new THREE.RingGeometry(0.96, 1, 80);
      const ringMat = new THREE.MeshBasicMaterial({
        color: C.blueSignal,
        transparent: true,
        opacity: 0,
        side: THREE.DoubleSide,
        blending: THREE.AdditiveBlending,
        depthWrite: false,
      });
      const ring = new THREE.Mesh(ringGeo, ringMat);
      ring.rotation.x = -Math.PI / 2;
      ring.position.y = 0.025;

      const group = new THREE.Group();
      group.add(dome);
      group.add(ring);
      this._coverageGroup.add(group);
      entry = { group, dome, domeMat, ring, ringMat };
      this._coverageMeshes.set(key, entry);
    }

    const opacity = active ? 0.16 : 0.04;
    entry.group.position.set(position[0], 0.02, position[2]);
    entry.group.scale.set(radius, radius * heightScale, radius);
    entry.domeMat.opacity = opacity;
    entry.ringMat.opacity = active ? 0.28 : 0.08;
    entry.group.visible = visible;
  }

  _syncTopology(liveData) {
    const env = this._environment;
    if (!env || !liveData || !Array.isArray(liveData.nodes) || !liveData.nodes.length) {
      this._clearTopology();
      return;
    }
    const nodes = this._mergeNodes(liveData);
    this._syncNodeObjects({ ...liveData, nodes });
    this._recomputeSceneFrame(env, nodes);
    this._frameCameraToSensors();
    const nodeById = new Map(nodes.map(n => [Number(n.node_id ?? n.id), n]));
    const apById = new Map((env.access_points || []).map(ap => [ap.ap_id, ap]));
    const visibleDevices = new Set();
    const visibleLinks = new Set();
    const visibleCoverage = new Set();
    const visibleImpacts = new Set();
    const collisions = [];
    const collisionTargets = this._linkCollisionTargets();

    for (const ap of env.access_points || []) {
      const position = this._positionOf(ap);
      if (!position) continue;
      const key = `ap:${ap.ap_id}`;
      visibleDevices.add(key);
      this._upsertDevice(key, 'ap', ap.label || ap.ap_id, position, ap.active !== false);
      visibleCoverage.add(key);
      this._upsertCoverage(key, position, ap.active !== false, ap.active !== false, ap.band, this._coverageRangeHint(ap, position, apById, env));
      this._ensureWaveSource(key, position, ap.active !== false, 1.15);
    }

    for (const node of nodes) {
      const id = this._nodeIdOf(node);
      if (id == null) continue;
      const status = String(node.health_status || node.status || (node.active === false ? 'offline' : 'live')).toLowerCase();
      const active = node.active !== false && !['offline', 'stale', 'sync_only'].includes(status);
      const position = this._nodeObjectPosition(id) || this._scenePositionForNode(id, node);
      if (!position) continue;
      const key = `node:${id}`;
      this._ensureWaveSource(key, position, active, 0.55);
    }

    for (const link of env.links || []) {
      const ap = apById.get(link.ap_id);
      const node = nodeById.get(Number(link.node_id));
      if (!ap || !node) continue;
      const nodeStatus = String(node.health_status || node.status || (node.active === false ? 'offline' : 'live')).toLowerCase();
      const active = node.active !== false && !['offline', 'stale', 'sync_only'].includes(nodeStatus);
      const key = link.link_id || `${link.ap_id}:c6-${link.node_id}`;
      const from = this._positionOf(ap);
      const to = this._nodeObjectPosition(Number(link.node_id)) || this._scenePositionForNode(Number(link.node_id), node);
      if (!from || !to) continue;
      visibleLinks.add(key);
      const collision = active ? this._detectLinkCollision(from, to, collisionTargets) : null;
      if (collision) {
        visibleImpacts.add(key);
        collisions.push(collision);
      }
      this._upsertLink(key, from, to, active, collision);
      this._upsertImpactMarker(key, collision, active);
    }

    for (const [key, entry] of this._deviceMeshes) {
      if (!visibleDevices.has(key)) entry.group.visible = false;
    }
    for (const [key, line] of this._linkMeshes) {
      if (!visibleLinks.has(key)) line.visible = false;
    }
    this._hideImpactMarkers(visibleImpacts);
    this._obstacleSummary = collisions.length
      ? { obstacleName: collisions[0].obstacleName, count: collisions.length }
      : null;
    this._hud?.updateObstacleAttenuation?.();
    for (const [key, entry] of this._coverageMeshes) {
      if (!visibleCoverage.has(key)) entry.group.visible = false;
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
    if (this._mistPoints) this._mistPoints.material.uniforms.uColor.value.copy(wc);
  }

  // ---- WebSocket live data ----

  _connectLiveUpdates() {
    this._setSceneStatus('offline', 'HORS LIGNE');
    this._hud.updateSourceBadge('offline', null);
    if (!window.RuvSenseWS) return;
    this._unsubscribeWs = window.RuvSenseWS.onUpdate((message, state) => {
      if (!state?.connected) {
        this._goOffline();
        return;
      }
      if (message) this._ingestSocketFrame(message);
    });
    window.RuvSenseWS.connect(window.RUVSENSE_CONFIG?.api_base);
  }

  _goOffline() {
    this._liveData = null;
    this._currentData = null;
    this._lastLiveAt = 0;
    this._currentScenario = null;
    this._personPositions.clear();
    this._syncRoomConfigNodes(null);
    this._updatePresenceSilhouettes(null, 0);
    this._clearTopology();
    this._scenarioProps?.update({ scenario: null, classification: {} }, null);
    this._setSceneStatus('offline', 'HORS LIGNE');
    this._hud.updateSourceBadge('offline', null);
  }

  _emptyLiveFrame() {
    return {
      msg_type: 'sensing_update',
      source: this._connectionState || 'connecting',
      system_status: this._connectionState || 'connecting',
      node_count: 0,
      nodes: [],
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
    const data = this._sceneFrameData(this._currentData);

    // Updates
    this._nebula?.update(dt, elapsed);
    this._updateScenarioProps(data);
    this._updateDotMatrixMist(data, elapsed);
    this._updateParticleTrail(data, dt, elapsed);
    this._syncTopology(data);
    this._syncRoomConfigNodes(data);
    this._updatePresenceSilhouettes(data, elapsed);
    this._updateWifiWaves(elapsed);
    this._updateSignalField(data);
    this._hud.updateHUD(data);
    this._hud.updateSparkline(data);

    // Autopilot orbit
    if (this._autopilot) {
      this._autoAngle += dt * this.settings.orbitSpeed;
      const r = Math.min(Math.max(Math.max(this._sensorBounds.width, this._sensorBounds.depth) * 1.2, 8), 36);
      this._camera.position.set(
        Math.sin(this._autoAngle) * r,
        this._sceneTarget.y + Math.max(2.4, this._sensorBounds.height * 0.5) + Math.sin(this._autoAngle * 0.5),
        Math.cos(this._autoAngle) * r
      );
      this._controls.target.copy(this._sceneTarget);
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
    if (!this._mistPoints && (!isPresent || persons.length === 0)) return;
    if (!this._mistPoints) this._buildDotMatrixMist();
    const holdAlpha = this._isPresenceHold(data) ? 0.45 : 1.0;
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
    const pp = persons[0].position_m || persons[0].position;
    if (!pp) return;
    const px = pp[0], pz = pp[2];
    const motionEnergy = Math.max(0, Math.min(1, Number(persons[0].motion_energy ?? persons[0].motion_score / 100) || 0));
    const bodyH = 1.7;
    const bodyBaseY = Math.max(0.05, pp[1]);
    const spread = motionEnergy > 0.1 ? 0.6 : 0.4;

    for (let i = 0; i < this._mistCount; i++) {
      const drift = Math.sin(elapsed * 0.5 + i * 0.1) * 0.003;
      const angle = (i / this._mistCount) * Math.PI * 2 + elapsed * 0.1;
      const layerT = (i % 20) / 20;
      const layerY = bodyBaseY + layerT * bodyH;

      const bodyWidth = layerT > 0.75 ? 0.15 : (layerT > 0.45 ? 0.25 : 0.18);
      const r = bodyWidth * (0.5 + 0.5 * Math.sin(i * 1.7 + elapsed * 0.3)) * spread;

      const tx = px + Math.cos(angle + i * 0.3) * r + drift;
      const tz = pz + Math.sin(angle + i * 0.5) * r * 0.6;

      pos.array[i * 3] += (tx - pos.array[i * 3]) * 0.05;
      pos.array[i * 3 + 1] += (layerY - pos.array[i * 3 + 1]) * 0.05;
      pos.array[i * 3 + 2] += (tz - pos.array[i * 3 + 2]) * 0.05;

      const targetAlpha = (0.15 + Math.sin(elapsed * 2 + i * 0.5) * 0.08) * holdAlpha;
      alpha.array[i] += (targetAlpha - alpha.array[i]) * 0.08;
    }
    pos.needsUpdate = true;
    alpha.needsUpdate = true;
  }

  _updateParticleTrail(data, dt, elapsed) {
    if (this.settings.trail <= 0) return;
    const persons = data?.persons || [];
    const isPresent = data?.classification?.presence || false;
    if (!this._trail && (!isPresent || persons.length === 0)) return;
    if (!this._trail) this._buildParticleTrail();
    const pos = this._trail.geometry.attributes.position;
    const ages = this._trail.geometry.attributes.age;

    for (let i = 0; i < this._trailCount; i++) {
      ages.array[i] = Math.min(1, ages.array[i] + dt * 0.8);
    }

    // Emit from all active persons
    if (isPresent && persons.length > 0 && !this._isPresenceHold(data)) {
      this._trailTimer += dt;
      const ms = persons[0].motion_score || 0;
      const emitRate = ms > 50 ? 0.02 : 0.08;

      if (this._trailTimer >= emitRate) {
        this._trailTimer = 0;
        for (const p of persons) {
          const pp = p.position_m || p.position;
          if (!pp) continue;
          const idx = this._trailHead;
          pos.array[idx * 3] = pp[0] + (Math.random() - 0.5) * 0.15;
          pos.array[idx * 3 + 1] = pp[1] + Math.random() * 1.5 + 0.1;
          pos.array[idx * 3 + 2] = pp[2] + (Math.random() - 0.5) * 0.15;
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
    if (!this._fieldPoints) this._buildSignalField();
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
      this._nebula?.setQuality(nl);
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

const observatory = new Observatory();
window.__ruvsenseObservatoryTestApi = {
  ingestFrame: (frame) => observatory._ingestSocketFrame(frame),
  normalizeFrame: (frame) => observatory._normalizeSensingFrame(frame),
  sceneFrameData: (frame) => observatory._sceneFrameData(observatory._normalizeSensingFrame(frame)),
  resetSmoothing: () => observatory._personPositions.clear(),
  setEnvironment: (env) => {
    observatory._environment = env;
    observatory._syncRoomGeometry(env);
    observatory._syncTopology(observatory._currentData);
    return env;
  },
  coverageSnapshot: () => Array.from(observatory._coverageMeshes.entries()).map(([key, entry]) => ({
    key,
    visible: entry.group.visible,
    radius_x: entry.group.scale.x,
    radius_z: entry.group.scale.z,
  })),
  motionSnapshot: () => Array.from(observatory._personPositions.entries()).map(([id, filter]) => ({
    id,
    history: filter.history.map((point) => ({ ...point })),
    displayed: { ...filter.displayed },
    target: { ...filter.target },
  })),
};

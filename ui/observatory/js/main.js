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
import { ScenarioProps } from './scenario-props.js';
import { HudController, DEFAULTS, SETTINGS_VERSION } from './hud-controller.js';
import { initAlerts, processAlertState } from '../../alerts.js';

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
const SLEEP_BED_CENTER = [3.5, 0.54, -3.5];
const SLEEP_BED_YAW = 0;
const SLEEP_BED_POSES = new Set(['lying', 'fallen']);
const KNOWN_POSES = new Set([
  'standing', 'walking', 'lying', 'sitting', 'fallen', 'falling',
  'exercising', 'gesturing', 'crouching',
]);
const POSE_ALIASES = new Map([
  ['stand', 'standing'],
  ['upright', 'standing'],
  ['still', 'standing'],
  ['moving', 'walking'],
  ['laying', 'lying'],
  ['laying_down', 'lying'],
  ['lying_down', 'lying'],
  ['supine', 'lying'],
  ['recumbent', 'lying'],
  ['seated', 'sitting'],
  ['sit', 'sitting'],
  ['fall', 'fallen'],
  ['down', 'fallen'],
  ['collapsed', 'fallen'],
  ['on_floor', 'fallen'],
  ['fall_detected', 'fallen'],
  ['falling_down', 'falling'],
  ['crouched', 'crouching'],
]);
const ROOM_CONFIG_STORAGE_KEY = 'ruvsense:room-config';
const SHARED_CHANNEL_NAME = 'ruvsense';
const SHARED_STATE_STORAGE_KEY = 'ruvsense:shared-state';
const DEFAULT_ROOM_CONFIG = {
  room_width_meters: 5.0,
  room_height_meters: 4.0,
  nodes: [
    { id: 1, x: 0.0, y: 0.0, label: 'Node 1' },
    { id: 2, x: 5.0, y: 0.0, label: 'Node 2' },
    { id: 3, x: 2.5, y: 4.0, label: 'Node 3' },
  ],
};
const ROOM_VISUAL_HEIGHT_M = 2.6;

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
    this._personMotionFilters = new Map();
    this._linkRaycaster = new THREE.Raycaster();
    this._connectionState = 'connecting';
    this._roomConfig = null;
    this._roomConfigSource = 'default';
    this._roomConfigNodes = new Map();
    this._presenceSilhouettes = new Map();
    this._activeRoomNodeCount = 0;
    this._sceneStatusOverlay = null;
    this._sharedStateChannel = null;
    this._lastSharedState = null;

    // Build scene
    this._setupLighting();
    this._nebula = new NebulaBackground(this._scene);
    this._buildRoom();
    this._scenarioProps = new ScenarioProps(this._scene);
    this._buildTopologyDevices();
    this._buildRoomConfigLayer();
    this._buildPresenceLayer();
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
    this._initSharedStateChannel();

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
    const { width, height, depth } = this._roomSize;

    this._grid = this._createRoomGrid(width, depth);
    this._scene.add(this._grid);

    this._roomWire = this._createRoomWire(width, height, depth);
    this._scene.add(this._roomWire);

    // Reflective floor
    const floorGeo = new THREE.PlaneGeometry(width, depth);
    this._floorMat = new THREE.MeshStandardMaterial({
      color: 0x101810,
      roughness: 1.0 - this.settings.reflect * 0.7,
      metalness: this.settings.reflect * 0.5,
      emissive: 0x020404,
      emissiveIntensity: 0.08,
    });
    this._floor = new THREE.Mesh(floorGeo, this._floorMat);
    this._floor.rotation.x = -Math.PI / 2;
    this._floor.receiveShadow = true;
    this._floor.userData.obstacleName = 'Sol de la pièce';
    this._floor.userData.isFloorObstacle = true;
    this._scene.add(this._floor);

  }

  _roomDimensions(env) {
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

  _createRoomGrid(width, depth) {
    const divisions = Math.max(4, Math.min(80, Math.ceil(Math.max(width, depth) * 2)));
    const vertices = [];
    for (let i = 0; i <= divisions; i++) {
      const x = -width / 2 + (width * i) / divisions;
      const z = -depth / 2 + (depth * i) / divisions;
      vertices.push(x, 0.01, -depth / 2, x, 0.01, depth / 2);
      vertices.push(-width / 2, 0.01, z, width / 2, 0.01, z);
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

  _createRoomWire(width, height, depth) {
    const boxGeo = new THREE.BoxGeometry(width, height, depth);
    const edges = new THREE.EdgesGeometry(boxGeo);
    const wire = new THREE.LineSegments(edges, new THREE.LineBasicMaterial({
      color: C.greenDim, opacity: 0.3, transparent: true,
    }));
    wire.position.y = height / 2;
    return wire;
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
    const next = this._roomDimensions(env);
    const same =
      Math.abs(next.width - this._roomSize.width) < 1e-6 &&
      Math.abs(next.height - this._roomSize.height) < 1e-6 &&
      Math.abs(next.depth - this._roomSize.depth) < 1e-6;
    if (same) return;

    this._roomSize = next;
    this._disposeLine(this._grid);
    this._grid = this._createRoomGrid(next.width, next.depth);
    this._grid.visible = this.settings.grid;
    this._scene.add(this._grid);

    this._disposeLine(this._roomWire);
    this._roomWire = this._createRoomWire(next.width, next.height, next.depth);
    this._roomWire.visible = this.settings.room;
    this._scene.add(this._roomWire);

    if (this._floor) {
      this._floor.geometry.dispose();
      this._floor.geometry = new THREE.PlaneGeometry(next.width, next.depth);
    }
  }

  // ---- Topology devices ----

  _buildTopologyDevices() {
    this._topologyGroup = new THREE.Group();
    this._linkGroup = new THREE.Group();
    this._impactGroup = new THREE.Group();
    this._coverageGroup = new THREE.Group();
    this._deviceMeshes = new Map();
    this._linkMeshes = new Map();
    this._impactMarkers = new Map();
    this._coverageMeshes = new Map();
    this._wifiWaves = [];
    this._scene.add(this._coverageGroup);
    this._scene.add(this._linkGroup);
    this._scene.add(this._impactGroup);
    this._scene.add(this._topologyGroup);
  }

  _buildRoomConfigLayer() {
    this._roomConfigGroup = new THREE.Group();
    this._roomConfigGroup.name = 'room-config-layer';
    this._scene.add(this._roomConfigGroup);
  }

  _buildPresenceLayer() {
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
    const stored = ignoreStored ? null : this._readStoredRoomConfig();
    if (stored) {
      this._applyRoomConfig(stored, 'local', false);
      return;
    }

    try {
      const response = await fetch('/ui/room-config.json', { cache: 'no-store' });
      if (response.ok) {
        const config = this._normalizeRoomConfig(await response.json());
        if (config) {
          this._applyRoomConfig(config, 'file', false);
          return;
        }
      }
    } catch {
      // The file is provisioned by deployment or another agent; use visual defaults until it exists.
    }

    this._applyRoomConfig(DEFAULT_ROOM_CONFIG, 'default', false);
  }

  _normalizeRoomConfig(raw) {
    if (!raw || typeof raw !== 'object') return null;
    const width = Number(raw.room_width_meters);
    const depth = Number(raw.room_height_meters);
    const nodes = Array.isArray(raw.nodes) ? raw.nodes : [];
    if (!Number.isFinite(width) || width <= 0 || !Number.isFinite(depth) || depth <= 0) return null;

    return {
      room_width_meters: width,
      room_height_meters: depth,
      nodes: nodes.map((node, index) => {
        const id = Number(node?.id ?? node?.node_id ?? index + 1);
        const x = Number(node?.x);
        const y = Number(node?.y);
        return {
          id: Number.isFinite(id) ? id : index + 1,
          x: Number.isFinite(x) ? x : 0,
          y: Number.isFinite(y) ? y : 0,
          label: String(node?.label || node?.display_label || `Node ${index + 1}`),
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

    if (!this._environment) {
      this._syncRoomGeometry({
        room: { dimensions_m: [config.room_width_meters, ROOM_VISUAL_HEIGHT_M, config.room_height_meters] },
      });
    }
    this._rebuildRoomConfigScene();
    this._populateRoomConfigPanel();
    this._syncRoomConfigNodes(this._currentData || this._liveData || null);
  }

  _populateRoomConfigPanel() {
    if (!this._roomConfig || !this._roomConfigPanel) return;
    const widthInput = document.getElementById('room-width-input');
    const heightInput = document.getElementById('room-height-input');
    if (widthInput) widthInput.value = this._roomConfig.room_width_meters.toFixed(1);
    if (heightInput) heightInput.value = this._roomConfig.room_height_meters.toFixed(1);
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
    const width = Number(document.getElementById('room-width-input')?.value);
    const depth = Number(document.getElementById('room-height-input')?.value);
    if (!Number.isFinite(width) || width <= 0 || !Number.isFinite(depth) || depth <= 0) return null;

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
      room_width_meters: width,
      room_height_meters: depth,
      nodes,
    };
  }

  _roomToScenePosition(x, y, elevation = 0) {
    const width = Number(this._roomConfig?.room_width_meters || this._roomSize.width || 1);
    const depth = Number(this._roomConfig?.room_height_meters || this._roomSize.depth || 1);
    return [
      Number(x || 0) - width / 2,
      elevation,
      Number(y || 0) - depth / 2,
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

  _rebuildRoomConfigScene() {
    if (!this._roomConfigGroup || !this._roomConfig) return;
    this._clearObjectGroup(this._roomConfigGroup);
    this._roomConfigNodes.clear();

    const width = this._roomConfig.room_width_meters;
    const depth = this._roomConfig.room_height_meters;
    const vertices = [];
    const xDivisions = Math.max(1, Math.ceil(width));
    const zDivisions = Math.max(1, Math.ceil(depth));

    for (let i = 0; i <= xDivisions; i++) {
      const x = -width / 2 + (width * i) / xDivisions;
      vertices.push(x, 0.035, -depth / 2, x, 0.035, depth / 2);
    }
    for (let i = 0; i <= zDivisions; i++) {
      const z = -depth / 2 + (depth * i) / zDivisions;
      vertices.push(-width / 2, 0.035, z, width / 2, 0.035, z);
    }

    const gridGeo = new THREE.BufferGeometry();
    gridGeo.setAttribute('position', new THREE.Float32BufferAttribute(vertices, 3));
    const grid = new THREE.LineSegments(
      gridGeo,
      new THREE.LineBasicMaterial({ color: 0x9aa4ad, transparent: true, opacity: 0.24 })
    );
    this._roomConfigGroup.add(grid);

    const borderPoints = [
      new THREE.Vector3(-width / 2, 0.05, -depth / 2),
      new THREE.Vector3(width / 2, 0.05, -depth / 2),
      new THREE.Vector3(width / 2, 0.05, depth / 2),
      new THREE.Vector3(-width / 2, 0.05, depth / 2),
      new THREE.Vector3(-width / 2, 0.05, -depth / 2),
    ];
    const border = new THREE.Line(
      new THREE.BufferGeometry().setFromPoints(borderPoints),
      new THREE.LineBasicMaterial({ color: 0xb8c0c8, transparent: true, opacity: 0.55 })
    );
    this._roomConfigGroup.add(border);

    for (const node of this._roomConfig.nodes) {
      const entry = this._createRoomNodeIcon(node);
      const [x, y, z] = this._roomToScenePosition(node.x, node.y, 0.08);
      entry.group.position.set(x, y, z);
      this._roomConfigGroup.add(entry.group);
      this._roomConfigNodes.set(Number(node.id), entry);
    }
  }

  _createRoomNodeIcon(node) {
    const group = new THREE.Group();
    const bodyMat = new THREE.MeshStandardMaterial({
      color: C.redAlert,
      emissive: C.redAlert,
      emissiveIntensity: 0.22,
      roughness: 0.4,
    });
    const antennaMat = new THREE.MeshBasicMaterial({ color: 0xd8dee8, transparent: true, opacity: 0.74 });
    const ringMat = new THREE.MeshBasicMaterial({
      color: C.redAlert,
      transparent: true,
      opacity: 0.55,
      wireframe: true,
    });

    const body = new THREE.Mesh(new THREE.BoxGeometry(0.18, 0.1, 0.18), bodyMat);
    body.position.y = 0.08;
    group.add(body);

    const antenna = new THREE.Mesh(new THREE.CylinderGeometry(0.012, 0.012, 0.3, 8), antennaMat);
    antenna.position.y = 0.28;
    group.add(antenna);

    const ring = new THREE.Mesh(new THREE.TorusGeometry(0.18, 0.006, 6, 32), ringMat);
    ring.rotation.x = Math.PI / 2;
    ring.position.y = 0.08;
    group.add(ring);

    const label = this._createTextSprite(node.label || `Node ${node.id}`, {
      width: 192,
      height: 56,
      font: '600 20px JetBrains Mono, Consolas, monospace',
      color: '#dce6ef',
      background: 'rgba(8, 16, 28, 0.64)',
    });
    label.scale.set(0.72, 0.22, 1);
    label.position.y = 0.62;
    group.add(label);

    return { group, bodyMat, ringMat, label };
  }

  _syncRoomConfigNodes(data) {
    if (!this._roomConfig || !this._roomConfigNodes.size) return;
    const systemStatus = String(data?.system_status || '').toLowerCase();
    const fallbackActiveNodes = Array.isArray(data?.nodes)
      ? data.nodes.filter((node) => node.active !== false).length
      : 0;
    const nodeCount = Math.max(0, this._integerOrZero(
      data?.node_count ?? data?.count_evidence?.active_nodes ?? fallbackActiveNodes
    ));
    const activeIds = new Set();
    for (const node of data?.nodes || []) {
      const status = String(node.status || node.health_status || '').toLowerCase();
      const active = node.active !== false && !['offline', 'stale', 'sync_only'].includes(status);
      const id = Number(node.node_id ?? node.id);
      if (active && Number.isFinite(id)) activeIds.add(id);
    }

    this._roomConfig.nodes.forEach((node, index) => {
      const entry = this._roomConfigNodes.get(Number(node.id));
      if (!entry) return;
      const hasExplicitIds = activeIds.size > 0;
      const active = systemStatus !== 'no_nodes' && (hasExplicitIds ? activeIds.has(Number(node.id)) : index < nodeCount);
      const color = active ? C.greenGlow : C.redAlert;
      entry.bodyMat.color.setHex(color);
      entry.bodyMat.emissive.setHex(color);
      entry.bodyMat.opacity = active ? 1 : 0.72;
      entry.ringMat.color.setHex(color);
      entry.ringMat.opacity = active ? 0.8 : 0.36;
      entry.group.visible = true;
    });
  }

  _presenceColor(confidence) {
    if (confidence < 0.4) return 0xff3040;
    if (confidence < 0.7) return 0xffb020;
    return 0x00d878;
  }

  _presenceVitalsStatus(person, br, hr) {
    const statusText = [
      person?.status,
      person?.state,
      person?.vital_status,
      person?.alert_status,
      person?.event_type,
      person?.reason,
    ].filter(Boolean).join(' ').normalize('NFD').replace(/[\u0300-\u036f]/g, '').toLowerCase();
    const critical =
      /\b(apnea|apnee|apneic|respiratory_arrest|cardiac_arrest|heart_stop|asystole|arret_cardiaque|critical|critique)\b/.test(statusText)
      || person?.apnea_detected === true
      || person?.cardiac_arrest === true
      || person?.heart_stopped === true
      || (br != null && br <= 3)
      || (hr != null && hr <= 5);
    if (critical) return { level: 'critical', label: 'ALERTE', color: '#ff3040' };
    const warning =
      /\b(fall|fallen|falling|chute|anomaly|anomalie|warning)\b/.test(statusText)
      || (br != null && (br < 8 || br > 28))
      || (hr != null && (hr < 50 || hr > 130));
    if (warning) return { level: 'warning', label: 'ALERTE', color: '#ffe4ad' };
    return { level: 'normal', label: 'NORMAL', color: '#b7ffd4' };
  }

  _upsertPresenceSilhouette(id) {
    let entry = this._presenceSilhouettes.get(id);
    if (entry) return entry;

    const group = new THREE.Group();
    const bodyMat = new THREE.MeshStandardMaterial({
      color: C.greenGlow,
      emissive: C.greenGlow,
      emissiveIntensity: 0.18,
      roughness: 0.36,
      metalness: 0.08,
    });
    const headMat = bodyMat.clone();
    const body = new THREE.Mesh(new THREE.CylinderGeometry(0.18, 0.22, 1.12, 20), bodyMat);
    body.position.y = 0.7;
    body.castShadow = true;
    group.add(body);

    const head = new THREE.Mesh(new THREE.SphereGeometry(0.2, 24, 16), headMat);
    head.position.y = 1.42;
    head.castShadow = true;
    group.add(head);

    const label = this._createTextSprite('', {
      width: 256,
      height: 92,
      font: '600 20px JetBrains Mono, Consolas, monospace',
      background: 'rgba(8, 16, 28, 0.76)',
    });
    label.scale.set(0.95, 0.34, 1);
    label.position.y = 1.92;
    group.add(label);

    this._presenceGroup.add(group);
    entry = { group, bodyMat, headMat, label, lastLabel: '' };
    this._presenceSilhouettes.set(id, entry);
    return entry;
  }

  _removePresenceSilhouette(id, entry) {
    this._presenceGroup.remove(entry.group);
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
    persons.forEach((person, index) => {
      if (person.is_present === false) return;
      const id = this._personIdentity(person, index);
      activeIds.add(id);
      const entry = this._upsertPresenceSilhouette(id);
      const position = this._parseVector3(person.position) || this._fallbackPersonPosition(index, persons.length);
      const confidence = Math.max(0, Math.min(1, Number(person.confidence) || 0));
      const motionEnergy = Math.max(0, Math.min(1, Number(person.motion_energy ?? person.motion_score / 100) || 0));
      const color = this._presenceColor(confidence);
      const yOffset = Math.sin(elapsed * 3.2 + index * 0.7) * motionEnergy * 0.12;

      entry.group.position.set(position[0], yOffset, position[2]);
      entry.bodyMat.color.setHex(color);
      entry.bodyMat.emissive.setHex(color);
      entry.headMat.color.setHex(color);
      entry.headMat.emissive.setHex(color);

      const br = this._numberOrNull(person.breathing_bpm ?? person.vitals?.breathing_bpm ?? person.vital_signs?.breathing_rate_bpm);
      const hr = this._numberOrNull(person.heart_rate_bpm ?? person.vitals?.heart_rate_bpm ?? person.vital_signs?.heart_rate_bpm);
      const vitalsStatus = this._presenceVitalsStatus(person, br, hr);
      const displayColor = vitalsStatus.level === 'critical' ? C.redAlert : color;
      entry.bodyMat.color.setHex(displayColor);
      entry.bodyMat.emissive.setHex(displayColor);
      entry.headMat.color.setHex(displayColor);
      entry.headMat.emissive.setHex(displayColor);
      const label = `BR ${br == null ? '--' : br.toFixed(1)} BPM\nHR ${hr == null ? '--' : Math.round(hr)} BPM\n${vitalsStatus.label}`;
      if (entry.lastLabel !== label) {
        this._updateTextSprite(entry.label, label, vitalsStatus.level === 'normal'
          ? (confidence < 0.4 ? '#ffb3ba' : confidence < 0.7 ? '#ffe4ad' : '#b7ffd4')
          : vitalsStatus.color);
        entry.lastLabel = label;
      }
    });

    for (const [id, entry] of [...this._presenceSilhouettes.entries()]) {
      if (!activeIds.has(id)) this._removePresenceSilhouette(id, entry);
    }
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

  _fetchEnvironment() {
    fetch('/api/v1/environment', { cache: 'no-store' })
      .then(r => r.ok ? r.json() : Promise.reject())
      .then(env => {
        this._environment = env;
        this._syncRoomGeometry(env);
        this._recomputeSceneFrame(env, env.nodes || []);
        this._frameCameraToSensors(true);
        this._setEnvironmentNotice(false);
        this._syncTopology(this._currentData);
      })
      .catch(() => {
        this._environment = null;
        this._setEnvironmentNotice(true);
        this._clearTopology();
        this._syncTopology(this._currentData);
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
    return this._parseVector3(person?.position_m) || this._parseVector3(person?.position);
  }

  _personConfidence(person) {
    const confidence = Number(person?.confidence ?? person?.tracking_confidence ?? person?.score ?? 0);
    return Number.isFinite(confidence) ? confidence : 0;
  }

  _normalizePoseName(value) {
    const raw = String(value ?? '').trim().toLowerCase();
    if (!raw) return null;
    const slug = raw.replace(/[\s-]+/g, '_');
    return POSE_ALIASES.get(slug) || (KNOWN_POSES.has(slug) ? slug : null);
  }

  _framePose(frame) {
    return this._normalizePoseName(frame?.posture) || this._normalizePoseName(frame?.pose);
  }

  _personPose(person, frame) {
    return this._normalizePoseName(person?.pose)
      || this._normalizePoseName(person?.posture)
      || this._framePose(frame);
  }

  _fallProgress(person, frame) {
    const progress = this._numberOrNull(
      person?.fallProgress ?? person?.fall_progress ?? frame?.fallProgress ?? frame?.fall_progress,
    );
    return progress == null ? null : Math.max(0, Math.min(1, progress));
  }

  _shouldSnapToSleepBed(data, person) {
    const scenario = String(data?.scenario || this._currentScenario || '').toLowerCase();
    return scenario === 'sleep_monitoring' && SLEEP_BED_POSES.has(person?.pose);
  }

  _snapToSleepBed(data, person) {
    if (!this._shouldSnapToSleepBed(data, person)) return person;
    const position = SLEEP_BED_CENTER.slice();
    return {
      ...person,
      position,
      ...(person.position_m ? { position_m: position.slice() } : {}),
      position_source: 'observatory_layout',
      facing: SLEEP_BED_YAW,
      keypoints: undefined,
      keypoints_m: undefined,
    };
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

  _translateKeypoints(keypoints, delta) {
    if (!Array.isArray(keypoints)) return keypoints;
    return keypoints.map((point) => {
      if (Array.isArray(point) && point.length >= 3) {
        const next = point.slice();
        next[0] += delta[0];
        next[1] += delta[1];
        next[2] += delta[2];
        return next;
      }
      if (point && typeof point === 'object') {
        return {
          ...point,
          x: Number(point.x || 0) + delta[0],
          y: Number(point.y || 0) + delta[1],
          z: Number(point.z || 0) + delta[2],
        };
      }
      return point;
    });
  }

  _smoothPersonPosition(person, rawPosition, nowMs) {
    const key = String(person.id);
    const source = String(person?.position_source || person?.pose_source || '').toLowerCase();
    if (['observatory_layout', 'count_evidence', 'presence_hold'].includes(source)) {
      const previous = this._personMotionFilters.get(key);
      if (previous && nowMs - previous.at <= 1800) {
        previous.at = nowMs;
        return previous.position.slice();
      }
      this._personMotionFilters.delete(key);
      return rawPosition;
    }
    const confidence = this._personConfidence(person);
    const previous = this._personMotionFilters.get(key);
    if (!previous) {
      this._personMotionFilters.set(key, { position: rawPosition.slice(), at: nowMs });
      return rawPosition;
    }
    const dt = Math.min(0.25, Math.max(1 / 60, (nowMs - previous.at) / 1000 || 1 / 30));
    const delta = [
      rawPosition[0] - previous.position[0],
      rawPosition[1] - previous.position[1],
      rawPosition[2] - previous.position[2],
    ];
    const distance = Math.hypot(delta[0], delta[1], delta[2]);
    const maxSpeedMps = confidence < 0.55 ? 1.1 : 2.4;
    const maxStep = Math.max(0.12, maxSpeedMps * dt);
    let gated = rawPosition;
    if (distance > maxStep && distance > 0) {
      const scale = maxStep / distance;
      gated = [
        previous.position[0] + delta[0] * scale,
        previous.position[1] + delta[1] * scale,
        previous.position[2] + delta[2] * scale,
      ];
    }
    const speed = distance / Math.max(dt, 1 / 60);
    const alpha = Math.min(0.58, Math.max(0.16, 0.18 + speed * 0.045));
    const smoothed = [
      previous.position[0] + (gated[0] - previous.position[0]) * alpha,
      previous.position[1] + (gated[1] - previous.position[1]) * alpha,
      previous.position[2] + (gated[2] - previous.position[2]) * alpha,
    ];
    this._personMotionFilters.set(key, { position: smoothed.slice(), at: nowMs });
    return smoothed;
  }

  _smoothScenePersons(persons, data) {
    const nowMs = this._sceneTimestampMs(data);
    const seen = new Set();
    const smoothed = persons.map((person, index) => {
      const id = this._personIdentity(person, index);
      seen.add(id);
      const rawPosition = this._parseVector3(person.position);
      if (!rawPosition) return person;
      const nextPosition = this._smoothPersonPosition({ ...person, id }, rawPosition, nowMs);
      const delta = [
        nextPosition[0] - rawPosition[0],
        nextPosition[1] - rawPosition[1],
        nextPosition[2] - rawPosition[2],
      ];
      return {
        ...person,
        id,
        position: nextPosition,
        ...(person.position_m ? { position_m: nextPosition } : {}),
        ...(person.keypoints_m ? { keypoints_m: this._translateKeypoints(person.keypoints_m, delta) } : {}),
      };
    });
    this._prunePersonFilters(seen, nowMs);
    return smoothed;
  }

  _prunePersonFilters(seen, nowMs) {
    for (const [key, filter] of this._personMotionFilters) {
      if (!seen.has(key) || nowMs - filter.at > 2500) this._personMotionFilters.delete(key);
    }
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
      this._liveData = normalized;
      this._lastLiveAt = performance.now();
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
      this._liveData = normalized;
      this._lastLiveAt = performance.now();
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

  _keypointObjects(source) {
    if (!Array.isArray(source) || source.length < 17) return null;
    const names = [
      'nose', 'left_eye', 'right_eye', 'left_ear', 'right_ear',
      'left_shoulder', 'right_shoulder', 'left_elbow', 'right_elbow',
      'left_wrist', 'right_wrist', 'left_hip', 'right_hip',
      'left_knee', 'right_knee', 'left_ankle', 'right_ankle',
    ];
    const points = [];
    for (let i = 0; i < 17; i++) {
      const kp = source[i];
      const x = Array.isArray(kp) ? Number(kp[0]) : Number(kp?.x);
      const y = Array.isArray(kp) ? Number(kp[1]) : Number(kp?.y);
      const z = Array.isArray(kp) ? Number(kp[2] ?? 0) : Number(kp?.z ?? 0);
      const confidence = Array.isArray(kp) ? Number(kp[3] ?? 0.8) : Number(kp?.confidence ?? 0.8);
      if (![x, y, z, confidence].every(Number.isFinite)) return null;
      points.push({ name: names[i], x, y, z, confidence });
    }
    return points;
  }

  _fallbackPersonPosition(index, total) {
    const count = Math.max(1, total);
    const spacing = Math.min(Math.max(this._sensorBounds.width * 0.18, 0.65), 1.25);
    const half = (count - 1) / 2;
    const zJitter = count > 1 ? ((index % 2) - 0.5) * Math.min(this._sensorBounds.depth * 0.12, 0.7) : 0;
    return [(index - half) * spacing, 0, zJitter];
  }

  _inferredPersonsFromCount(frame, evidence, startIndex = 0) {
    const rendered = Math.min(MAX_SCENE_PERSONS, this._integerOrZero(evidence?.rendered_persons));
    if (rendered <= startIndex) return [];
    const reason = String(evidence?.reason || '').toLowerCase();
    const confidence = this._numberOrNull(frame?.classification?.confidence)
      ?? (reason === 'presence_hold' ? 0.35 : 0.5);
    return Array.from({ length: rendered - startIndex }, (_, offset) => {
      const index = startIndex + offset;
      return {
        id: reason === 'presence_hold' ? `held_${index + 1}` : `inferred_${index + 1}`,
        confidence,
        position: this._fallbackPersonPosition(index, rendered),
        position_source: 'observatory_layout',
        pose_source: reason === 'presence_hold' ? 'presence_hold' : 'count_evidence',
        pose: 'standing',
        detection_state: reason === 'presence_hold' ? 'held' : 'inferred',
      };
    });
  }

  _normalizeSensingFrame(rawFrame) {
    if (!rawFrame || typeof rawFrame !== 'object') return null;
    const frame = this._lastEdgeVitals ? this._mergeEdgeVitals(rawFrame, this._lastEdgeVitals) : { ...rawFrame };
    const rawPersons = Array.isArray(frame.persons) ? frame.persons : [];
    let persons = rawPersons.slice(0, MAX_SCENE_PERSONS).map((person, index) => {
      const pose = this._personPose(person, frame);
      const fallProgress = this._fallProgress(person, frame);
      return {
        ...person,
        id: this._personIdentity(person, index),
        ...(pose ? { pose } : {}),
        ...(fallProgress != null ? { fallProgress } : {}),
      };
    });

    if (!persons.length) {
      const keypoints = this._keypointObjects(frame.pose_keypoints);
      if (keypoints) {
        const confidence = this._numberOrNull(frame.classification?.confidence) ?? 0.5;
        const pose = this._framePose(frame);
        persons.push({
          id: 'pose_1',
          confidence,
          keypoints,
          position: this._fallbackPersonPosition(0, 1),
          position_source: 'observatory_layout',
          pose_source: 'pose_keypoints',
          ...(pose ? { pose } : {}),
        });
      }
    }

    let evidence = this._countEvidence({ ...frame, persons });
    const inferredPersons = this._inferredPersonsFromCount(frame, evidence, persons.length);
    if (inferredPersons.length) {
      persons = persons.concat(inferredPersons);
      evidence = this._countEvidence({ ...frame, persons });
    }
    const hasCountEvidence = Boolean(frame.count_evidence && typeof frame.count_evidence === 'object');
    const renderLimit = hasCountEvidence
      ? evidence.rendered_persons
      : Math.max(evidence.rendered_persons, persons.length);
    persons = this._dedupePersons(persons, renderLimit);
    persons = this._smoothScenePersons(persons, frame);
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
      persons,
      estimated_persons: renderedPersons,
      count_evidence: countEvidence,
      classification,
    };
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
          pose_source: 'ws_pose',
          pose: 'standing',
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
      const position = this._roomToScenePosition(node.x, node.y, 0.08);
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
      source: 'ws_pose',
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
      const layoutPosition = String(person?.position_source || '').toLowerCase() === 'observatory_layout'
        ? this._parseVector3(person.position)
        : null;
      const position = layoutPosition
        || this._positionOf(person)
        || this._fallbackPersonPosition(index, inputPersons.length);
      const keypointsM = this._transformMetricKeypoints(person.keypoints_m);
      const scenePerson = {
        ...person,
        position,
        ...(person.position_m ? { position_m: position } : {}),
        ...(keypointsM ? { keypoints_m: keypointsM } : {}),
      };
      return this._snapToSleepBed(data, scenePerson);
    }).filter(Boolean);
    let evidence = this._countEvidence({ ...data, persons });
    const inferredPersons = this._inferredPersonsFromCount(data, evidence, persons.length);
    if (inferredPersons.length) {
      persons = persons.concat(inferredPersons);
      evidence = this._countEvidence({ ...data, persons });
    }
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

  _transformMetricKeypoints(source) {
    if (!Array.isArray(source) || source.length < 17) return null;
    const points = [];
    for (const kp of source.slice(0, 17)) {
      const raw = this._parseVector3(kp);
      if (!raw) return null;
      points.push([
        raw[0] - this._sensorOrigin[0],
        raw[1],
        raw[2] - this._sensorOrigin[2],
      ]);
    }
    return points;
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

  _upsertLink(key, from, to, active, collision = null) {
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
    if (!env) {
      this._clearTopology();
      return;
    }
    const nodes = this._mergeNodes(liveData);
    this._recomputeSceneFrame(env, nodes);
    this._frameCameraToSensors();
    const nodeById = new Map(nodes.map(n => [Number(n.node_id), n]));
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
      const status = String(node.health_status || node.status || (node.active === false ? 'offline' : 'live')).toLowerCase();
      const active = node.active !== false && !['offline', 'stale', 'sync_only'].includes(status);
      const position = this._positionOf(node);
      if (!position) continue;
      const key = `node:${node.node_id}`;
      visibleDevices.add(key);
      this._upsertDevice(key, 'node', node.display_label || node.label || `C6-${node.node_id}`, position, active);
      visibleCoverage.add(key);
      const band = this._coverageBand(node, apById, env);
      const coverageVisible = active || ['stale', 'sync_only'].includes(status);
      this._upsertCoverage(key, position, active, coverageVisible, band, this._coverageRangeHint(node, position, apById, env));
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
      const to = this._positionOf(node);
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
    const jc = new THREE.Color(this.settings.jointColor);
    this._figurePool.applyColors(wc, jc);
    this._mistPoints.material.uniforms.uColor.value.copy(wc);
  }

  // ---- WebSocket live data ----

  _initSharedStateChannel() {
    this._setSceneStatus('connecting', 'En attente de la console 2D...');
    this._hud.updateSourceBadge('connecting', null);
    this._loadStoredSharedState();

    if (typeof BroadcastChannel === 'undefined') return;
    this._sharedStateChannel = new BroadcastChannel(SHARED_CHANNEL_NAME);
    this._sharedStateChannel.addEventListener('message', (event) => {
      const message = event.data || {};
      if (message.type === 'state' && message.state) {
        this._ingestSharedState(message.state);
      }
    });
  }

  _loadStoredSharedState() {
    try {
      const raw = localStorage.getItem(SHARED_STATE_STORAGE_KEY);
      if (raw) this._ingestSharedState(JSON.parse(raw));
    } catch {}
  }

  _environmentFromSharedState(shared) {
    const topology = shared?.topology;
    if (!topology || typeof topology !== 'object') return null;

    const accessPoints = (topology.access_points || []).map((ap, index) => {
      const apId = String(ap.ap_id || ap.bssid || ap.id || `ap-${index + 1}`);
      return {
        ...ap,
        ap_id: apId,
        label: ap.label || ap.ssid || ap.bssid || apId,
        position_m: ap.position_m || ap.position,
        active: ap.active !== false && ap.status !== 'offline',
      };
    });
    const apIdByBssid = new Map(accessPoints.map((ap) => [String(ap.bssid || ap.ap_id).toLowerCase(), ap.ap_id]));
    const nodes = this._nodesFromSharedState(shared);

    const links = (topology.links || []).map((link, index) => {
      const apId = link.ap_id
        || apIdByBssid.get(String(link.ap_bssid || link.bssid || '').toLowerCase())
        || link.ap_bssid
        || link.bssid
        || accessPoints[0]?.ap_id
        || `ap-${index + 1}`;
      return {
        ...link,
        ap_id: String(apId),
        node_id: Number(link.node_id ?? link.node ?? nodes[index % Math.max(1, nodes.length)]?.node_id ?? index + 1),
        link_id: link.link_id || `${apId}:node-${link.node_id ?? index + 1}`,
      };
    });

    return {
      ...topology,
      room: topology.room || { dimensions_m: [5.2, 2.6, 4.8] },
      access_points: accessPoints,
      nodes,
      links,
    };
  }

  _nodesFromSharedState(shared) {
    const topologyNodes = Array.isArray(shared?.topology?.nodes) ? shared.topology.nodes : [];
    const latestNodes = Array.isArray(shared?.latest?.nodes) ? shared.latest.nodes : [];
    const nodesById = new Map();
    for (const node of [...topologyNodes, ...latestNodes]) {
      const nodeId = Number(node?.node_id ?? node?.id);
      if (!Number.isFinite(nodeId)) continue;
      const status = String(node.health_status || node.status || '').toLowerCase();
      const previous = nodesById.get(nodeId) || {};
      nodesById.set(nodeId, {
        ...previous,
        ...node,
        node_id: nodeId,
        id: nodeId,
        label: node.display_label || node.label || previous.label || `C6-${nodeId}`,
        position_m: node.position_m || node.position || previous.position_m || previous.position,
        active: node.active !== false && !['offline', 'stale', 'sync_only'].includes(status),
      });
    }
    return [...nodesById.values()];
  }

  _primaryVitalsFromSharedState(shared) {
    const latestVitals = shared?.latest?.vital_signs || {};
    const restVitals = shared?.vitals?.vital_signs || shared?.vitals || {};
    const edgeVitals = shared?.edgeVitals?.edge_vitals || shared?.edgeVitals || {};
    const heart = this._numberOrNull(
      latestVitals.heart_rate_bpm ?? latestVitals.heartrate_bpm
      ?? edgeVitals.heart_rate_bpm ?? edgeVitals.heartrate_bpm
      ?? restVitals.heart_rate_bpm ?? restVitals.heartrate_bpm,
    );
    const breathing = this._numberOrNull(
      latestVitals.breathing_rate_bpm ?? latestVitals.breathing_bpm
      ?? edgeVitals.breathing_rate_bpm ?? edgeVitals.breathing_bpm
      ?? restVitals.breathing_rate_bpm ?? restVitals.breathing_bpm,
    );
    const quality = this._numberOrNull(
      latestVitals.signal_quality ?? latestVitals.presence_score
      ?? edgeVitals.signal_quality ?? edgeVitals.presence_score
      ?? restVitals.signal_quality ?? restVitals.presence_score,
    );
    if (heart == null && breathing == null && quality == null) return null;
    return {
      ...(heart != null ? { heart_rate_bpm: heart } : {}),
      ...(breathing != null ? { breathing_rate_bpm: breathing } : {}),
      ...(quality != null ? { signal_quality: quality } : {}),
    };
  }

  _personsFromSharedState(shared) {
    const latestPersons = Array.isArray(shared?.latest?.persons) ? shared.latest.persons : [];
    const posePersons = Array.isArray(shared?.pose?.persons) ? shared.pose.persons : [];
    const locationPersons = Array.isArray(shared?.location?.persons) ? shared.location.persons : [];
    if (latestPersons.length) return latestPersons;
    if (posePersons.length) return posePersons;

    return locationPersons.map((person, index) => {
      const x = this._numberOrNull(person.x) ?? this._numberOrNull(person.position?.x) ?? 0;
      const z = this._numberOrNull(person.y) ?? this._numberOrNull(person.z) ?? this._numberOrNull(person.position?.z) ?? 0;
      const confidence = this._numberOrNull(person.confidence) ?? 0.35;
      return {
        id: person.id || `location_${index + 1}`,
        confidence,
        position: [x, 0, z],
        position_m: [x, 0, z],
        position_source: 'shared_location',
        pose: 'standing',
      };
    });
  }

  _frameFromSharedState(shared) {
    if (!shared || typeof shared !== 'object') return null;
    const latest = shared.latest && typeof shared.latest === 'object' ? { ...shared.latest } : {};
    const nodes = this._nodesFromSharedState(shared);
    const persons = this._personsFromSharedState(shared);
    const vitalSigns = this._primaryVitalsFromSharedState(shared) || latest.vital_signs || null;
    const activeNodes = nodes.filter((node) => node.active !== false).length;
    const classification = {
      ...(latest.classification || {}),
      presence: latest.classification?.presence ?? persons.length > 0,
      motion_level: latest.classification?.motion_level || (persons.length ? 'present' : 'absent'),
      confidence: latest.classification?.confidence ?? persons[0]?.confidence ?? 0,
    };

    return {
      ...latest,
      type: 'sensing_update',
      msg_type: 'sensing_update',
      source: latest.source || 'shared_state',
      system_status: latest.system_status || (activeNodes > 0 ? 'live' : 'no_nodes'),
      timestamp_ms: latest.timestamp_ms || shared.updatedAt || Date.now(),
      node_count: latest.node_count ?? activeNodes,
      nodes: Array.isArray(latest.nodes) && latest.nodes.length ? latest.nodes : nodes,
      persons,
      estimated_persons: latest.estimated_persons ?? persons.length,
      count_evidence: {
        ...(latest.count_evidence || {}),
        rendered_persons: latest.count_evidence?.rendered_persons ?? persons.length,
        stable_persons: latest.count_evidence?.stable_persons ?? persons.length,
        raw_estimated_persons: latest.count_evidence?.raw_estimated_persons ?? persons.length,
        active_nodes: latest.count_evidence?.active_nodes ?? activeNodes,
        supporting_nodes: latest.count_evidence?.supporting_nodes ?? activeNodes,
      },
      classification,
      vital_signs: vitalSigns,
    };
  }

  _ingestSharedState(shared) {
    this._lastSharedState = shared;
    processAlertState(shared);

    const env = this._environmentFromSharedState(shared);
    if (env) {
      this._environment = env;
      this._syncRoomGeometry(env);
      this._setEnvironmentNotice(false);
    }

    const frame = this._frameFromSharedState(shared);
    const normalized = this._normalizeSensingFrame(frame);
    if (!normalized) return;

    const status = String(normalized.system_status || '').toLowerCase() === 'no_nodes' ? 'no_nodes' : 'live';
    this._liveData = normalized;
    this._lastLiveAt = performance.now();
    this._setSceneStatus(status, status === 'no_nodes' ? 'Aucun noeud ESP32 connecte' : '');
    this._hud.updateSourceBadge(status, null);
    this._updateScenarioProps(normalized);
    this._syncTopology(normalized);
    this._syncRoomConfigNodes(normalized);
  }

  _poseWsCandidates() {
    const host = window.location.hostname || 'localhost';
    const candidates = [];
    if (window.location.protocol === 'http:' || window.location.protocol === 'https:') {
      const proto = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
      candidates.push(`${proto}//${window.location.host}/ws/pose`);
    }
    candidates.push(`ws://${host}:3000/ws/pose`);
    return [...new Set(candidates)];
  }

  _autoDetectLegacyLive() {
    // Probe sensing server health on same origin, then common ports
    const host = window.location.hostname || 'localhost';
    const candidates = [
      window.location.origin,                   // same origin (e.g. :3000)
      `http://${host}:3000`,                     // Rust server port
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
            const wsUrl = `${wsProto}//${urlObj.host}/ws/pose`;
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

  _autoDetectLive() {
    const candidates = this._poseWsCandidates();
    const url = candidates[this._wsCandidateIndex % candidates.length];
    this._wsCandidateIndex += 1;
    this.settings.dataSource = 'ws';
    this.settings.wsUrl = url;
    this._setSceneStatus('connecting', 'En attente du serveur...');
    this._hud.updateSourceBadge('connecting', null);
    this._connectWS(url);
  }

  _connectWS(url) {
    this._disconnectWS();
    try {
      this._ws = new WebSocket(url);
      this._ws.onopen = () => {
        console.log('[Observatory] WebSocket connected:', url);
        this._setSceneStatus('connecting', 'En attente du serveur...');
        this._hud.updateSourceBadge('connecting', this._ws);
      };
      this._ws.onmessage = (evt) => {
        try {
          this._ingestSocketFrame(JSON.parse(evt.data));
        } catch {}
      };
      this._ws.onclose = () => {
        console.log('[Observatory] WebSocket closed; retrying pose stream');
        this._ws = null;
        this._setSceneStatus('connecting', 'En attente du serveur...');
        this._hud.updateSourceBadge('connecting', null);
        this._scheduleReconnect();
      };
      this._ws.onerror = () => {};
    } catch {
      this._setSceneStatus('connecting', 'En attente du serveur...');
      this._hud.updateSourceBadge('connecting', null);
      this._scheduleReconnect();
    }
  }

  _scheduleReconnect() {
    if (this._wsReconnectTimer) return;
    this._wsReconnectTimer = window.setTimeout(() => {
      this._wsReconnectTimer = null;
      this._autoDetectLive();
    }, 3000);
  }

  _disconnectWS() {
    if (this._ws) {
      this._ws.onclose = null;
      this._ws.close();
      this._ws = null;
    }
    this._liveData = null;
    this._personMotionFilters.clear();
    this._syncRoomConfigNodes(null);
    this._updatePresenceSilhouettes(null, 0);
  }

  _emptyLiveFrame() {
    return {
      msg_type: 'sensing_update',
      source: this._connectionState || 'connecting',
      system_status: this._connectionState || 'connecting',
      node_count: 0,
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
    const data = this._sceneFrameData(this._currentData);

    // Updates
    this._nebula.update(dt, elapsed);
    this._updateScenarioProps(data);
    this._figurePool.update(data, elapsed);
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
    const ms = persons[0].motion_score || 0;
    const pose = persons[0].pose || 'standing';
    const isLying = pose === 'lying' || pose === 'fallen';
    const bodyH = isLying ? 0.4 : 1.7;
    const bodyBaseY = isLying ? pp[1] + 0.05 : Math.max(0.05, pp[1]);
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

const observatory = new Observatory();
window.__ruvsenseObservatoryTestApi = {
  ingestFrame: (frame) => observatory._ingestSocketFrame(frame),
  normalizeFrame: (frame) => observatory._normalizeSensingFrame(frame),
  sceneFrameData: (frame) => observatory._sceneFrameData(observatory._normalizeSensingFrame(frame)),
  resetSmoothing: () => observatory._personMotionFilters.clear(),
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
  motionSnapshot: () => Array.from(observatory._personMotionFilters.entries()).map(([id, filter]) => ({
    id,
    position: filter.position.slice(),
    at: filter.at,
  })),
};

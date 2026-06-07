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

const MAX_SCENE_PERSONS = 8;

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
    this._lastLiveAt = 0;
    this._lastEdgeVitals = null;

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
    this._coverageGroup = new THREE.Group();
    this._deviceMeshes = new Map();
    this._linkMeshes = new Map();
    this._coverageMeshes = new Map();
    this._wifiWaves = [];
    this._scene.add(this._coverageGroup);
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
    for (const [, entry] of this._coverageMeshes) entry.group.visible = false;
    for (const waves of this._wifiWaves) {
      waves.active = false;
      for (const shell of waves.shells) shell.mesh.visible = false;
    }
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
    if (type === 'sensing_update') {
      const normalized = this._normalizeSensingFrame(frame);
      if (!normalized) return;
      this._liveData = normalized;
      this._lastLiveAt = performance.now();
      this._syncTopology(normalized);
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

  _normalizeSensingFrame(rawFrame) {
    if (!rawFrame || typeof rawFrame !== 'object') return null;
    const frame = this._lastEdgeVitals ? this._mergeEdgeVitals(rawFrame, this._lastEdgeVitals) : { ...rawFrame };
    const rawPersons = Array.isArray(frame.persons) ? frame.persons : [];
    const persons = rawPersons.slice(0, MAX_SCENE_PERSONS).map((person, index) => ({
      ...person,
      id: person?.id ?? person?.track_id ?? `person_${index + 1}`,
    }));

    if (!persons.length) {
      const keypoints = this._keypointObjects(frame.pose_keypoints);
      if (keypoints) {
        const confidence = this._numberOrNull(frame.classification?.confidence) ?? 0.5;
        persons.push({
          id: 'pose_1',
          confidence,
          keypoints,
          position: this._fallbackPersonPosition(0, 1),
          position_source: 'observatory_layout',
          pose_source: 'pose_keypoints',
        });
      }
    }

    const estimatedPersons = Math.max(this._integerOrZero(frame.estimated_persons), persons.length);
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
      estimated_persons: estimatedPersons,
      classification,
    };
  }

  _sceneFrameData(data) {
    if (!data) return data;
    const inputPersons = Array.isArray(data.persons) ? data.persons.slice(0, MAX_SCENE_PERSONS) : [];
    const persons = inputPersons.map((person, index) => {
      const layoutPosition = String(person?.position_source || '').toLowerCase() === 'observatory_layout'
        ? this._parseVector3(person.position)
        : null;
      const position = layoutPosition
        || this._positionOf(person)
        || this._fallbackPersonPosition(index, inputPersons.length);
      const keypointsM = this._transformMetricKeypoints(person.keypoints_m);
      return {
        ...person,
        position,
        ...(person.position_m ? { position_m: position } : {}),
        ...(keypointsM ? { keypoints_m: keypointsM } : {}),
      };
    }).filter(Boolean);
    const estimatedPersons = Math.max(this._integerOrZero(data.estimated_persons), persons.length);
    return {
      ...data,
      persons,
      estimated_persons: estimatedPersons,
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

  _coverageRadiusForBand(band) {
    const normalized = String(band || '').toLowerCase();
    const roomSpan = Math.max(this._roomSize.width, this._roomSize.depth, 2);
    let radius = 4.5;
    if (normalized.includes('6')) radius = 2.8;
    else if (normalized.includes('5')) radius = 3.4;
    else if (normalized.includes('2.4') || normalized.includes('2g')) radius = 4.8;
    return Math.min(radius, Math.max(1.2, roomSpan * 0.55));
  }

  _upsertCoverage(key, position, active, visible, band) {
    let entry = this._coverageMeshes.get(key);
    const radius = this._coverageRadiusForBand(band);
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

    for (const ap of env.access_points || []) {
      const position = this._positionOf(ap);
      if (!position) continue;
      const key = `ap:${ap.ap_id}`;
      visibleDevices.add(key);
      this._upsertDevice(key, 'ap', ap.label || ap.ap_id, position, ap.active !== false);
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
      this._upsertCoverage(key, position, active, coverageVisible, band);
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
      this._upsertLink(key, from, to, active);
    }

    for (const [key, entry] of this._deviceMeshes) {
      if (!visibleDevices.has(key)) entry.group.visible = false;
    }
    for (const [key, line] of this._linkMeshes) {
      if (!visibleLinks.has(key)) line.visible = false;
    }
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
          this._ingestSocketFrame(JSON.parse(evt.data));
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
    const data = this._sceneFrameData(this._currentData);

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

new Observatory();

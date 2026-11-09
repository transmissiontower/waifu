import * as THREE from 'three';
import { GLTFLoader } from 'three/examples/jsm/loaders/GLTFLoader.js';
import { VRM, VRMLoaderPlugin, VRMUtils } from '@pixiv/three-vrm';
import { OrbitControls } from 'three/examples/jsm/controls/OrbitControls.js';

/** emotion tag -> VRM expression weights (+ head pose offsets). Edit freely. */
const EMOTIONS: Record<string, { w: Record<string, number>; headX?: number; headZ?: number }> = {
  neutral:   { w: {} },
  happy:     { w: { relaxed: 0.65, happy: 0.25 } },
  sad:       { w: { sad: 0.7 }, headX: 0.1 },
  angry:     { w: { angry: 0.75 }, headX: -0.05 },
  surprised: { w: { surprised: 0.8 }, headX: -0.06 },
  shy:       { w: { relaxed: 0.45, happy: 0.15 }, headX: 0.14, headZ: 0.08 },
  thinking:  { w: { relaxed: 0.25 }, headZ: -0.12, headX: -0.06 },
  smug:      { w: { relaxed: 0.5, happy: 0.15 }, headZ: 0.1 },
};
const EXPRESSIONS = ['happy', 'angry', 'sad', 'relaxed', 'surprised'];
// Arm angle (radians) to leave the T-pose.
const ARM_DOWN = 1.2;

export class Avatar {
  private renderer: THREE.WebGLRenderer;
  private scene = new THREE.Scene();
  private camera = new THREE.PerspectiveCamera(28, 1, 0.1, 20);
  private controls: OrbitControls;
  private clock = new THREE.Clock();
  private vrm?: VRM;
  private weights: Record<string, number> = {};
  private target = EMOTIONS.neutral;
  private mouth = 0;
  private mouthTarget = 0;
  private blinkAt = 2;
  private blinkT = -1;
  private gesture: { name: string; t: number } | null = null;
  private defaultTarget = new THREE.Vector3(0, 1.35, 0);
  private defaultCameraPos = new THREE.Vector3(0, 1.37, 1.35);
  state = 'idle';
  getLevel: () => number = () => 0;

  constructor(private canvas: HTMLCanvasElement) {
    this.renderer = new THREE.WebGLRenderer({ canvas, alpha: true, antialias: true });
    this.renderer.setPixelRatio(Math.min(devicePixelRatio, 2));
    this.scene.add(new THREE.AmbientLight(0xffffff, 1.1));
    const d = new THREE.DirectionalLight(0xffffff, 1.6);
    d.position.set(1, 2, 2);
    this.scene.add(d);

    this.controls = new OrbitControls(this.camera, canvas);
    this.controls.enableDamping = true;
    this.controls.dampingFactor = 0.05;
    this.controls.enablePan = true;
    this.controls.minDistance = 0.6;
    this.controls.maxDistance = 3.5;
    this.controls.maxPolarAngle = Math.PI / 2 + 0.15;
    this.controls.target.copy(this.defaultTarget);

    canvas.addEventListener('dblclick', () => this.resetCamera());
    addEventListener('resize', () => this.resize());
    this.resize();
    this.loop();
  }

  private resize() {
    const w = innerWidth, h = innerHeight;
    this.renderer.setSize(w, h, false);
    const aspect = w / h;
    this.camera.aspect = aspect;
    if (aspect < 1) {
      this.camera.fov = Math.min(50, 28 / aspect);
    } else {
      this.camera.fov = 28;
    }
    this.camera.updateProjectionMatrix();
  }

  resetCamera() {
    this.camera.position.copy(this.defaultCameraPos);
    this.controls.target.copy(this.defaultTarget);
    this.controls.update();
  }

  private setExpressionWeight(name: string, value: number) {
    if (!this.vrm?.expressionManager) return;
    const em = this.vrm.expressionManager;
    em.setValue(name, value);
    if (em.getValue(name) === null) {
      const match = em.expressions.find(
        (e) => e.expressionName.toLowerCase() === name.toLowerCase()
      );
      if (match) {
        em.setValue(match.expressionName, value);
      }
    }
  }

  async load(url: string): Promise<void> {
    const loader = new GLTFLoader();
    loader.register((p) => new VRMLoaderPlugin(p));
    const gltf = await loader.loadAsync(url);
    const vrm: VRM = gltf.userData.vrm;
    VRMUtils.removeUnnecessaryVertices(gltf.scene);
    VRMUtils.rotateVRM0(vrm); // VRM0 models face -Z; normalise to +Z
    if (this.vrm) { this.scene.remove(this.vrm.scene); VRMUtils.deepDispose(this.vrm.scene); }
    this.vrm = vrm;
    this.weights = {};
    this.scene.add(vrm.scene);
    if (vrm.lookAt) vrm.lookAt.target = this.camera;

    // frame head & upper body with comfortable headroom
    const head = vrm.humanoid.getNormalizedBoneNode('head');
    const p = new THREE.Vector3();
    (head ?? vrm.scene).getWorldPosition(p);
    this.defaultTarget.set(0, p.y - 0.06, 0);
    this.defaultCameraPos.set(0, p.y - 0.04, 1.35);
    this.resetCamera();
  }

  get loaded() { return !!this.vrm; }

  setEmotion(name: string) { this.target = EMOTIONS[name] ?? EMOTIONS.neutral; }
  setState(s: string) { this.state = s; if (s === 'thinking') this.setEmotion('thinking'); }
  play(name: string) { this.gesture = { name, t: 0 }; }

  private loop = () => {
    requestAnimationFrame(this.loop);
    const dt = Math.min(this.clock.getDelta(), 0.1);
    const t = this.clock.elapsedTime;
    const vrm = this.vrm;
    this.controls.update();

    if (vrm) {
      const hum = vrm.humanoid;
      hum.resetNormalizedPose();

      // detect arm sign from normalized bone rest pose:
      // VRM0 models have negative X for leftLowerArm; VRM1 models have positive X.
      const armSign = (hum.getNormalizedBoneNode('leftLowerArm')?.position.x ?? 1) >= 0 ? 1 : -1;

      // arms down + breathing + idle sway
      const set = (bone: string, x = 0, y = 0, z = 0) => {
        const n = hum.getNormalizedBoneNode(bone as any);
        if (n) n.rotation.set(x, y, z);
      };
      const breath = Math.sin(t * 1.6) * 0.012;
      set('leftUpperArm', 0, 0, -armSign * ARM_DOWN + Math.sin(t * 1.1) * 0.02);
      set('rightUpperArm', 0, 0, armSign * ARM_DOWN - Math.sin(t * 1.1 + 1) * 0.02);
      set('leftLowerArm', 0, 0, -armSign * 0.1);
      set('rightLowerArm', 0, 0, armSign * 0.1);
      set('spine', breath, Math.sin(t * 0.4) * 0.03, 0);
      set('chest', breath * 0.8, 0, 0);

      // head: emotion pose + speaking bob + gestures
      const mouthLevel = this.getLevel();
      let hx = (this.target.headX ?? 0) + Math.sin(t * 0.7) * 0.02 + mouthLevel * 0.05;
      let hz = (this.target.headZ ?? 0) + Math.sin(t * 0.5) * 0.02;
      let hy = Math.sin(t * 0.33) * 0.05;
      if (this.state === 'listening') { hx -= 0.04; hz += 0.06; }
      if (this.gesture) {
        this.gesture.t += dt;
        const g = this.gesture;
        if (g.name === 'nod') hx += Math.sin(g.t * 9) * 0.12 * Math.max(0, 1 - g.t / 0.9);
        if (g.name === 'tilt') hz += Math.sin(Math.min(g.t, 1) * Math.PI) * 0.2;
        if (g.t > 1.2) this.gesture = null;
      }
      set('head', hx, hy, hz);
      set('neck', hx * 0.4, hy * 0.4, hz * 0.4);

      // emotions: smooth towards target weights
      for (const k of EXPRESSIONS) {
        const goal = this.target.w[k] ?? 0;
        const cur = this.weights[k] ?? 0;
        this.weights[k] = cur + (goal - cur) * Math.min(1, dt * 6);
        this.setExpressionWeight(k, this.weights[k]);
      }

      // lip-sync from the audio actually being played
      this.mouthTarget = mouthLevel;
      this.mouth += (this.mouthTarget - this.mouth) * Math.min(1, dt * 22);
      const m = this.mouth;
      this.setExpressionWeight('aa', m * 0.85);
      this.setExpressionWeight('oh', m * 0.25 * (0.5 + 0.5 * Math.sin(t * 13)));
      this.setExpressionWeight('ih', m * 0.2 * (0.5 + 0.5 * Math.sin(t * 9 + 1)));

      // blinking
      this.blinkAt -= dt;
      if (this.blinkAt <= 0 && this.blinkT < 0) { this.blinkT = 0; this.blinkAt = 2 + Math.random() * 4; }
      let blink = 0;
      if (this.blinkT >= 0) {
        this.blinkT += dt;
        blink = Math.sin(Math.min(this.blinkT / 0.16, 1) * Math.PI);
        if (this.blinkT > 0.16) this.blinkT = -1;
      }
      this.setExpressionWeight('blink', blink);

      vrm.update(dt);
    }
    this.renderer.render(this.scene, this.camera);
  };
}

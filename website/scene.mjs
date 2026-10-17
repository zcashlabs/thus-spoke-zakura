import * as THREE from 'three';
import { EffectComposer } from 'jsm/postprocessing/EffectComposer.js';
import { RenderPass } from 'jsm/postprocessing/RenderPass.js';
import { UnrealBloomPass } from 'jsm/postprocessing/UnrealBloomPass.js';

const canvas = document.querySelector('.webgl');
const reduced = matchMedia('(prefers-reduced-motion: reduce)');

/* Bail out before touching WebGL so the CSS hero stays intact on failure. */
function supported() {
  try {
    return !!document.createElement('canvas').getContext('webgl2');
  } catch { return false; }
}
if (!canvas || !supported()) throw new Error('webgl-unavailable');

const PETALS = 5;
const ROSE = 0xd4718c;
const CORE = new THREE.Color(0xff9ec0);

const scene = new THREE.Scene();
const scrollGroup = new THREE.Group();
const blossom = new THREE.Group();
scene.add(scrollGroup);
scrollGroup.add(blossom);

const camera = new THREE.PerspectiveCamera(42, innerWidth / innerHeight, 0.1, 100);
camera.position.z = 7.2;

const renderer = new THREE.WebGLRenderer({ canvas, antialias: true });
renderer.setSize(innerWidth, innerHeight);
renderer.setPixelRatio(Math.min(devicePixelRatio, 2));
renderer.toneMapping = THREE.ACESFilmicToneMapping;
renderer.toneMappingExposure = 1.05;

const composer = new EffectComposer(renderer);
composer.addPass(new RenderPass(scene, camera));
const bloom = new UnrealBloomPass(new THREE.Vector2(innerWidth, innerHeight), 0.62, 0.5, 0.72);
composer.addPass(bloom);

const ambient = new THREE.AmbientLight(0xffffff, 0.55);
const key = new THREE.DirectionalLight(0xfff1f4, 2.4);
key.position.set(3, 4, 5);
const rim = new THREE.DirectionalLight(0xbfd6ff, 1.5);
rim.position.set(-4, -1, -3);
const under = new THREE.PointLight(0xff9ec0, 0.9, 8);
under.position.set(0, 0, 1.2);
scene.add(under);
scene.add(ambient, key, rim);

/* Petal outline: the same curve the falling petals use, as a 2D shape that gets
   extruded so it catches light instead of reading as a flat card. */
function petalShape() {
  const s = new THREE.Shape();
  s.moveTo(0, 0);
  s.bezierCurveTo(-0.38, 0.48, -0.68, 1.04, -0.68, 1.62);
  s.bezierCurveTo(-0.68, 2.12, -0.46, 2.52, -0.22, 2.56);
  s.bezierCurveTo(-0.10, 2.58, -0.04, 2.48, 0, 2.36);
  s.bezierCurveTo(0.04, 2.48, 0.10, 2.58, 0.22, 2.56);
  s.bezierCurveTo(0.46, 2.52, 0.68, 2.12, 0.68, 1.62);
  s.bezierCurveTo(0.68, 1.04, 0.38, 0.48, 0, 0);
  return s;
}

const petalGeo = new THREE.ExtrudeGeometry(petalShape(), {
  depth: 0.04, bevelEnabled: true, bevelSize: 0.05, bevelThickness: 0.05, bevelSegments: 4, curveSegments: 48,
});
petalGeo.center();

const petalMat = new THREE.MeshPhysicalMaterial({
  color: ROSE, roughness: 0.52, metalness: 0, clearcoat: 0.5, clearcoatRoughness: 0.5,
  sheen: 1, sheenColor: new THREE.Color(0xffd9e6), side: THREE.DoubleSide,
  transparent: true, opacity: 0.82,
});

const petals = [];
for (let i = 0; i < PETALS; i += 1) {
  const pivot = new THREE.Group();
  const angle = (i / PETALS) * Math.PI * 2;
  pivot.rotation.z = angle;
  const mesh = new THREE.Mesh(petalGeo, petalMat);
  const wobble = (i * 2654435761 % 1000) / 1000;      // stable per-petal jitter
  mesh.position.y = 1.42;
  mesh.rotation.x = -0.34 + (wobble - 0.5) * 0.22;    // cup, not lie flat
  mesh.rotation.z = (wobble - 0.5) * 0.18;
  mesh.scale.setScalar(0.92 + wobble * 0.16);
  pivot.add(mesh);
  blossom.add(pivot);
  petals.push({
    pivot, mesh, angle, lift: 0,
    outward: new THREE.Vector3(Math.sin(-angle), Math.cos(angle), 0.45).normalize(),
    spin: (i % 2 ? 1 : -1) * (0.5 + Math.random() * 0.7),
    centre: new THREE.Vector3(Math.sin(-angle) * 1.42, Math.cos(angle) * 1.42, 0),
  });
}

/* Flower centre: a small domed disc that sits inside the petal bases. The
   wireframe sphere this replaces was wider than the bases and read as noise. */
const coreMat = new THREE.MeshStandardMaterial({
  color: 0x6f2b40, roughness: 0.78, metalness: 0,
  emissive: new THREE.Color(0xff9ec0), emissiveIntensity: 0,
});
const core = new THREE.Mesh(new THREE.SphereGeometry(0.3, 40, 28), coreMat);
core.scale.z = 0.45;
core.position.z = 0.2;            // petals are cupped forward; meet them there
blossom.add(core);
/* Park the blossom in the left column and size it off the visible frustum so it
   stays put at any viewport, rather than guessing world units. */
let homeX = 0;
function layout() {
  const halfH = Math.tan((camera.fov * Math.PI) / 360) * camera.position.z;
  const halfW = halfH * camera.aspect;
  const narrow = innerWidth < 900;
  homeX = narrow ? 0 : -halfW * 0.5;
  /* Stacked layout: lift the blossom into the stage band above the copy. */
  blossom.position.set(homeX, narrow ? halfH * 0.52 : 0, 0);
  blossom.scale.setScalar(narrow ? Math.min(0.42, halfW * 0.22) : Math.min(0.78, halfH * 0.26));
}
layout();

/* Drifting petals around the blossom, replacing the DOM-based ones. */
const DRIFT = 44;
const driftGeo = new THREE.ExtrudeGeometry(petalShape(), { depth: 0.02, bevelEnabled: false, curveSegments: 10 });
driftGeo.center();
driftGeo.scale(0.075, 0.075, 0.075);
const drift = new THREE.InstancedMesh(driftGeo, petalMat, DRIFT);
drift.instanceMatrix.setUsage(THREE.DynamicDrawUsage);
const driftState = [];
const dummy = new THREE.Object3D();
for (let i = 0; i < DRIFT; i += 1) {
  driftState.push({
    x: (Math.random() - 0.5) * 18, y: Math.random() * 14 - 7, z: (Math.random() - 0.5) * 6 - 1,
    fall: 0.28 + Math.random() * 0.5, sway: Math.random() * Math.PI * 2,
    rx: Math.random() * 1.6, ry: Math.random() * 1.6, rz: Math.random() * 1.6,
    scale: 0.55 + Math.random() * 0.9,
  });
}
scene.add(drift);

/* Hover: raycast a wide invisible sphere so the blossom reacts well before the
   cursor is literally on a petal. */
const hitSphere = new THREE.Mesh(
  new THREE.SphereGeometry(2.9, 24, 24),
  new THREE.MeshBasicMaterial({ visible: false }),
);
blossom.add(hitSphere);

const raycaster = new THREE.Raycaster();
const pointer = new THREE.Vector2(-999, -999);
const hover = { point: new THREE.Vector3(), strength: 0 };
const localHover = new THREE.Vector3();
addEventListener('pointermove', event => {
  pointer.x = (event.clientX / innerWidth) * 2 - 1;
  pointer.y = -(event.clientY / innerHeight) * 2 + 1;
}, { passive: true });

const smoothstep = (a, b, v) => {
  const t = Math.min(1, Math.max(0, (v - a) / (b - a)));
  return t * t * (3 - 2 * t);
};

let running = true;
let visible = true;
const clock = new THREE.Clock();
let last = 0;

function frame() {
  requestAnimationFrame(frame);
  if (!running || document.hidden) return;

  const time = clock.getElapsedTime();
  const delta = Math.min(time - last, 0.1);
  last = time;

  if (!reduced.matches) {
    blossom.rotation.z += delta * 0.07;
    blossom.rotation.x = Math.sin(time * 0.3) * 0.12;
  }

  raycaster.setFromCamera(pointer, camera);
  const hit = raycaster.intersectObject(hitSphere);
  if (hit.length) {
    blossom.worldToLocal(localHover.copy(hit[0].point));
    hover.point.copy(localHover);
    hover.strength = Math.min(hover.strength + delta * 4, 1);
  } else {
    hover.strength = Math.max(hover.strength - delta * 2, 0);
  }

  for (const petal of petals) {
    const distance = petal.centre.distanceTo(hover.point);
    const target = (1 - smoothstep(0.6, 3.1, distance)) * hover.strength;
    const speed = target > petal.lift ? 0.14 : 0.06;
    petal.lift += (target - petal.lift) * speed;
    const lift = petal.lift;
    petal.mesh.position.set(0, 1.42 + lift * 1.15, lift * 0.55);
    petal.mesh.rotation.x = -0.34 - lift * 1.05;
    petal.mesh.rotation.z = lift * petal.spin;
    petal.mesh.scale.setScalar(1 - lift * 0.06);
  }
  coreMat.emissiveIntensity = hover.strength * 0.6;

  if (!reduced.matches) {
    for (let i = 0; i < DRIFT; i += 1) {
      const d = driftState[i];
      d.y -= d.fall * delta;
      d.sway += delta * 0.6;
      if (d.y < -7) { d.y = 7; d.x = (Math.random() - 0.5) * 16; }
      dummy.position.set(d.x + Math.sin(d.sway) * 0.5, d.y, d.z);
      dummy.rotation.set(time * d.rx, time * d.ry, time * d.rz);
      dummy.scale.setScalar(d.scale);
      dummy.updateMatrix();
      drift.setMatrixAt(i, dummy.matrix);
    }
    drift.instanceMatrix.needsUpdate = true;
  }

  composer.render();
}
frame();

addEventListener('resize', () => {
  camera.aspect = innerWidth / innerHeight;
  camera.updateProjectionMatrix();
  renderer.setSize(innerWidth, innerHeight);
  renderer.setPixelRatio(Math.min(devicePixelRatio, 2));
  composer.setSize(innerWidth, innerHeight);
  bloom.resolution.set(innerWidth, innerHeight);
  layout();
});

document.documentElement.classList.add('webgl-ready');

export const sceneApi = {
  scrollGroup,
  blossom,
  drift,
  setRunning(value) { running = value; },
  mirrorX() { return -2 * homeX; },
  setTheme(theme) {
    const dark = theme === 'dark';
    const bg = getComputedStyle(document.documentElement).getPropertyValue('--bg').trim();
    if (bg) scene.background = new THREE.Color(bg);
    ambient.intensity = dark ? 0.42 : 0.72;
    key.intensity = dark ? 2.6 : 2.1;
    bloom.strength = dark ? 0.6 : 0.06;
    bloom.threshold = dark ? 0.72 : 0.95;
    petalMat.color.set(dark ? ROSE : 0xd87e9a);
    petalMat.opacity = dark ? 0.85 : 0.95;
    petalMat.roughness = dark ? 0.52 : 0.72;
    under.intensity = dark ? 0.9 : 0.25;
    coreMat.color.set(dark ? 0x6f2b40 : 0x8e3752);
  },
  get visible() { return visible; },
  set visible(v) { visible = v; blossom.visible = v; drift.visible = v; },
};

import { storyPhase, storyProgress } from './story.mjs';
import { shuffleContributors } from './contributor-order.mjs';

const root = document.documentElement;
const contributorNodes = [...document.querySelectorAll('.contrib-node')];
const profiles = shuffleContributors(contributorNodes.map(node => ({
  href: node.getAttribute('href'),
  label: node.getAttribute('aria-label'),
  title: node.querySelector('title').textContent,
  avatar: node.querySelector('image').getAttribute('href'),
})));
contributorNodes.forEach((node, index) => {
  const profile = profiles[index];
  node.setAttribute('href', profile.href);
  node.setAttribute('aria-label', profile.label);
  node.querySelector('title').textContent = profile.title;
  node.querySelector('image').setAttribute('href', profile.avatar);
});

const themeButton = document.querySelector('.theme-toggle');
const themeColor = document.querySelector('meta[name="theme-color"]');

function syncTheme() {
  const dark = root.dataset.theme === 'dark';
  const label = `Switch to ${dark ? 'light' : 'dark'} theme`;
  themeButton.setAttribute('aria-label', label);
  themeButton.title = label;
  themeColor.content = dark ? '#0b0a0b' : '#f9f4f2';
}

syncTheme();
themeButton.addEventListener('click', () => {
  root.dataset.theme = root.dataset.theme === 'dark' ? 'light' : 'dark';
  try { localStorage.setItem('tsz-theme', root.dataset.theme); } catch { /* Storage may be disabled. */ }
  syncTheme();
});

const menuButton = document.querySelector('.menu-toggle');
const nav = document.querySelector('.site-nav');
function closeMenu() {
  document.body.classList.remove('menu-open');
  menuButton.setAttribute('aria-expanded', 'false');
  menuButton.setAttribute('aria-label', 'Open menu');
}
menuButton.addEventListener('click', () => {
  const open = document.body.classList.toggle('menu-open');
  menuButton.setAttribute('aria-expanded', String(open));
  menuButton.setAttribute('aria-label', open ? 'Close menu' : 'Open menu');
  if (open) nav.querySelector('a').focus();
});
nav.addEventListener('click', event => {
  if (document.body.classList.contains('menu-open') && event.target.closest('a')) {
    closeMenu();
    menuButton.focus();
  }
});
document.addEventListener('keydown', event => {
  if (event.key === 'Escape' && document.body.classList.contains('menu-open')) {
    closeMenu();
    menuButton.focus();
  }
});
window.addEventListener('resize', () => { if (innerWidth > 900) closeMenu(); });

const copyStatus = document.querySelector('#copy-status');
const installCommand = document.querySelector('#install-command');
for (const copyButton of document.querySelectorAll('[data-copy-install]')) {
  const label = copyButton.textContent;
  let timer;
  copyButton.addEventListener('click', async () => {
    let message;
    try {
      await navigator.clipboard.writeText(installCommand.textContent);
      copyButton.textContent = 'Copied';
      message = 'Install command copied.';
    } catch {
      copyButton.textContent = 'Copy failed';
      message = 'Copy failed. Select the command in the hero to copy it.';
    }
    copyStatus.textContent = message;
    clearTimeout(timer);
    timer = setTimeout(() => {
      copyButton.textContent = label;
      copyStatus.textContent = '';
    }, 2500);
  });
}

const story = document.querySelector('.story');
const reducedMotion = matchMedia('(prefers-reduced-motion: reduce)');
const steps = [...document.querySelectorAll('[data-story-step]')];
const compact = matchMedia('(max-width: 900px)');
const phases = ['start', 'send', 'confirm', 'inspect'];
let frame;

function updateStory() {
  frame = null;
  if (compact.matches || reducedMotion.matches) {
    story.dataset.phase = 'confirm';
    steps.forEach(step => step.removeAttribute('aria-current'));
    return;
  }
  const rect = story.getBoundingClientRect();
  const phase = storyPhase(storyProgress(rect.top, rect.height, innerHeight));
  story.dataset.phase = phases[phase];
  steps.forEach((step, index) => {
    if (index === phase) step.setAttribute('aria-current', 'step');
    else step.removeAttribute('aria-current');
  });
}
function scheduleStory() { if (!frame) frame = requestAnimationFrame(updateStory); }
window.addEventListener('scroll', scheduleStory, { passive: true });
window.addEventListener('resize', scheduleStory);
reducedMotion.addEventListener('change', scheduleStory);
compact.addEventListener('change', scheduleStory);
updateStory();

const header = document.querySelector('.site-header');
const setHeaderState = () => header.classList.toggle('scrolled', scrollY > 8);
setHeaderState();
window.addEventListener('scroll', setHeaderState, { passive: true });

document.documentElement.classList.add('reveal');
const reveal = new IntersectionObserver(entries => {
  for (const entry of entries) {
    if (!entry.isIntersecting) continue;
    entry.target.classList.add('in');
    reveal.unobserve(entry.target);
  }
}, { rootMargin: '0px 0px -12%' });
for (const el of document.querySelectorAll('[data-reveal], [data-reveal-group]')) {
  [...el.children].forEach((child, index) => child.style.setProperty('--i', index));
  reveal.observe(el);
}

const navLinks = [...document.querySelectorAll('.site-nav a[href^="#"]')];
const railLinks = [...document.querySelectorAll('.rail-item')];
const visible = new Set();
const spy = new IntersectionObserver(entries => {
  for (const entry of entries) {
    if (entry.isIntersecting) visible.add(entry.target.id);
    else visible.delete(entry.target.id);
  }
  for (const [links, activeClass] of [[navLinks, 'active'], [railLinks, 'is-active']]) {
    const current = links.find(link => visible.has(link.hash.slice(1)));
    for (const link of links) link.classList.toggle(activeClass, link === current);
  }
}, { rootMargin: '-45% 0px -45%' });
for (const id of new Set([...navLinks, ...railLinks].map(link => link.hash.slice(1)))) {
  const section = document.getElementById(id);
  if (section) spy.observe(section);
}

for (const card of document.querySelectorAll('.feature-list article')) {
  card.addEventListener('pointermove', event => {
    const box = card.getBoundingClientRect();
    card.style.setProperty('--mx', `${event.clientX - box.left}px`);
    card.style.setProperty('--my', `${event.clientY - box.top}px`);
  });
}

/* ---------------------------------------------------------------------
   WebGL stage. Everything below is progressive: if three.js or GSAP fail to
   load the page keeps working exactly as it did without them.
   ------------------------------------------------------------------- */
function startHud() {
  const readout = document.querySelector('#hud-readout');
  if (!readout) return;
  addEventListener('pointermove', event => {
    const phi = ((event.clientX / innerWidth) * 360).toFixed(1).padStart(5, '0');
    const theta = ((event.clientY / innerHeight) * 180).toFixed(1).padStart(5, '0');
    readout.innerHTML = `POINTER&nbsp; \u03c6 ${phi}\u00b0<br>&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;\u03b8 ${theta}\u00b0<br>PETALS&nbsp;&nbsp; 5 \u00b7 DRIFT 90`;
  }, { passive: true });
}

async function startStage() {
  let scene;
  try {
    ({ sceneApi: scene } = await import('./scene.mjs'));
  } catch {
    return;                                   // No WebGL: the CSS hero stands alone.
  }
  scene.setTheme(root.dataset.theme);
  themeButton.addEventListener('click', () => scene.setTheme(root.dataset.theme));
  startHud();

  if (typeof gsap === 'undefined' || typeof ScrollTrigger === 'undefined') return;
  gsap.registerPlugin(ScrollTrigger);

  /* A restored scroll position parks the scrub timelines at progress 1 before
     the first frame measures anything. Own the position. */
  if ('scrollRestoration' in history) history.scrollRestoration = 'manual';
  if (!location.hash) scrollTo(0, 0);

  gsap.matchMedia().add('(prefers-reduced-motion: no-preference)', () => {
    gsap.from(scene.blossom.rotation, { y: Math.PI * 1.2, duration: 2.2, ease: 'power3.out' });
    gsap.from(scene.scrollGroup.position, { y: -2.4, duration: 2.2, ease: 'power3.out' });

    /* Hero centred -> walkthrough hides it -> Controls left -> Connect right. */
    gsap.timeline({ scrollTrigger: { trigger: '.hero', start: 'top top', end: 'bottom top', scrub: 1.2 } })
      .to(scene.scrollGroup.position, { y: 1.6, z: -2.4, ease: 'none' }, 0)
      .to(scene.scrollGroup.rotation, { x: 0.4, ease: 'none' }, 0);

    gsap.timeline({ scrollTrigger: { trigger: '#features', start: 'top bottom', end: 'center center', scrub: 1.4 } })
      .fromTo(scene.scrollGroup.position, { x: 0, y: 1.6, z: -2.4 }, { x: 0, y: 0, z: 0, ease: 'power1.inOut', immediateRender: false }, 0)
      .fromTo(scene.scrollGroup.rotation, { x: 0.4, y: 0, z: 0 }, { x: -0.25, y: 0.8, z: 0.25, ease: 'power1.inOut', immediateRender: false }, 0);

    gsap.timeline({ scrollTrigger: { trigger: '#connect', start: 'top bottom', end: 'center center', scrub: 1.4 } })
      .fromTo(scene.scrollGroup.position, { x: 0 }, { x: scene.mirrorX(), ease: 'power1.inOut', immediateRender: false }, 0)
      .fromTo(scene.scrollGroup.rotation, { y: 0.8, z: 0.25 }, { y: -0.8, z: -0.25, ease: 'power1.inOut', immediateRender: false }, 0);
  });

  /* Stop rendering entirely once the canvas is covered by opaque sections. */
  ScrollTrigger.create({
    trigger: '#docs',
    start: 'top 80%',
    onEnter: () => scene.setRunning(false),
    onLeaveBack: () => scene.setRunning(true),
  });

  /* Fonts and the sticky walkthrough settle after first paint; without this the
     trigger positions are measured against the wrong layout. */
  addEventListener('load', () => ScrollTrigger.refresh());
  ScrollTrigger.refresh();
}

startStage();

import assert from 'node:assert/strict';
import { after, test } from 'node:test';

const { chromium } = await import(process.env.THS_PLAYWRIGHT_MODULE || 'playwright');
const browser = await chromium.launch({ executablePath: process.env.THS_CHROMIUM_EXECUTABLE });
const origin = process.env.THS_WEBSITE_URL || 'http://127.0.0.1:8767';
after(() => browser.close());

for (const width of [360, 390, 768, 1024, 1440]) {
  for (const theme of ['light', 'dark']) {
    test(`${width}px ${theme}: layout, navigation, theme and copy controls`, async () => {
      const context = await browser.newContext({
        viewport: { width, height: 900 },
        colorScheme: theme,
        permissions: ['clipboard-read', 'clipboard-write'],
      });
      try {
        const page = await context.newPage();
        const errors = [];
        page.on('pageerror', error => errors.push(error.message));
        await page.goto(origin, { waitUntil: 'networkidle' });
        await page.evaluate(() => document.fonts.ready);
        assert.equal(await page.locator('html').getAttribute('data-theme'), theme);
        assert.equal(await page.evaluate(() => document.documentElement.scrollWidth), width);
        assert.ok(await page.locator('#install-command').evaluate(el => el.scrollWidth <= el.clientWidth));
        if (width <= 900) {
          const stage = await page.locator('.hero-stage').boundingBox();
          const title = await page.locator('#hero-title').boundingBox();
          assert.ok(stage.y + stage.height <= title.y, 'the hero stage must sit above the copy');
          assert.ok(await page.locator('.contrib').evaluate(el => {
            const heading = el.querySelector('.contrib-head').getBoundingClientRect();
            return [...el.querySelectorAll('.contrib-node')].every(node => node.getBoundingClientRect().top >= heading.bottom);
          }), 'contributor faces must sit below the heading and link');
        }
        await page.locator('.theme-toggle').click();
        const savedTheme = theme === 'light' ? 'dark' : 'light';
        assert.equal(await page.locator('html').getAttribute('data-theme'), savedTheme);
        await page.reload({ waitUntil: 'networkidle' });
        assert.equal(await page.locator('html').getAttribute('data-theme'), savedTheme);
        await page.locator('.theme-toggle').focus();
        await page.keyboard.press('Enter');
        assert.equal(await page.locator('html').getAttribute('data-theme'), theme);
        await page.reload({ waitUntil: 'networkidle' });
        assert.equal(await page.locator('html').getAttribute('data-theme'), theme);
        if (width <= 900) {
          const menu = page.locator('.menu-toggle');
          await menu.focus();
          await page.keyboard.press('Enter');
          assert.equal(await menu.getAttribute('aria-expanded'), 'true');
          assert.ok(await page.locator('.site-nav a').first().evaluate(el => el === document.activeElement));
          await page.keyboard.press('Escape');
          assert.equal(await menu.getAttribute('aria-expanded'), 'false');
          assert.ok(await menu.evaluate(el => el === document.activeElement));
          await menu.click();
        }
        await page.locator('.site-nav a[href="#how"]').click();
        assert.equal(new URL(page.url()).hash, '#how');
        assert.equal(await page.locator('.menu-toggle').getAttribute('aria-expanded'), 'false');
        await page.locator('[data-copy-install]').first().click();
        await page.waitForFunction(() => document.querySelector('[data-copy-install]').textContent === 'Copied');
        assert.equal(await page.evaluate(() => navigator.clipboard.readText()), await page.locator('#install-command').textContent());
        await page.locator('[data-copy-install]').last().focus();
        await page.keyboard.press('Enter');
        await page.waitForFunction(() => [...document.querySelectorAll('[data-copy-install]')].at(-1).textContent === 'Copied');
        assert.equal(await page.evaluate(() => navigator.clipboard.readText()), await page.locator('#install-command').textContent());
        await page.locator('.footer-bottom a').focus();
        await page.keyboard.press('Enter');
        assert.equal(new URL(page.url()).hash, '#top');
        assert.deepEqual(errors, []);
      } finally {
        await context.close();
      }
    });
  }
}

test('reduced motion disables the entrance and scroll choreography, including preference changes', async () => {
  const context = await browser.newContext({ viewport: { width: 1440, height: 900 }, reducedMotion: 'reduce' });
  try {
    const page = await context.newPage();
    await page.goto(origin, { waitUntil: 'networkidle' });
    await page.waitForFunction(() => document.documentElement.classList.contains('webgl-ready'));
    const motionTriggers = () => page.evaluate(() => ScrollTrigger.getAll().filter(trigger => trigger.vars.scrub).length);
    assert.equal(await motionTriggers(), 0, 'reduced motion must not create scroll animations');
    const entranceTweens = await page.evaluate(async () => {
      const { sceneApi } = await import('./scene.mjs');
      return gsap.getTweensOf([sceneApi.blossom.rotation, sceneApi.scrollGroup.position]).length;
    });
    assert.equal(entranceTweens, 0, 'reduced motion must not create entrance animations');
    const pose = () => page.evaluate(async () => {
      const { sceneApi } = await import('./scene.mjs');
      const matrices = [];
      sceneApi.blossom.traverse(node => matrices.push([...node.matrixWorld.elements]));
      return matrices;
    });
    const initialPose = await pose();
    await page.mouse.move(360, 450);
    await page.waitForTimeout(250);
    assert.deepEqual(await pose(), initialPose, 'reduced motion must disable hover deformation');
    await page.locator('#features').scrollIntoViewIfNeeded();
    await page.waitForTimeout(250);
    assert.deepEqual(await pose(), initialPose, 'reduced motion must keep the blossom still while scrolling');
    await page.emulateMedia({ reducedMotion: 'no-preference' });
    await page.waitForFunction(() => ScrollTrigger.getAll().filter(trigger => trigger.vars.scrub).length === 3);
    await page.emulateMedia({ reducedMotion: 'reduce' });
    await page.waitForFunction(() => ScrollTrigger.getAll().filter(trigger => trigger.vars.scrub).length === 0);
    assert.equal(await page.locator('.story').getAttribute('data-phase'), 'confirm');
  } finally {
    await context.close();
  }
});

test('navigation and copy remain usable when the animation CDN is unavailable', async () => {
  const context = await browser.newContext({ permissions: ['clipboard-read', 'clipboard-write'] });
  try {
    const page = await context.newPage();
    await page.route('https://cdn.jsdelivr.net/**', route => route.abort());
    await page.goto(origin, { waitUntil: 'networkidle' });
    assert.equal(await page.locator('html').evaluate(el => el.classList.contains('webgl-ready')), false);
    await page.locator('.theme-toggle').click();
    assert.equal(await page.locator('html').getAttribute('data-theme'), 'dark');
    await page.locator('[data-copy-install]').first().click();
    await page.waitForFunction(() => document.querySelector('[data-copy-install]').textContent === 'Copied');
    assert.equal(await page.evaluate(() => navigator.clipboard.readText()), await page.locator('#install-command').textContent());
    await page.locator('.site-nav a[href="#docs"]').click();
    assert.equal(new URL(page.url()).hash, '#docs');
  } finally {
    await context.close();
  }
});

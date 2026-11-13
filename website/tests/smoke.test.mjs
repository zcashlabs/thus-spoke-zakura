import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const html = await readFile(new URL('../index.html', import.meta.url), 'utf8');
const readme = await readFile(new URL('../../README.md', import.meta.url), 'utf8');

test('navigation targets and interactive controls are present', () => {
  const ids = new Set([...html.matchAll(/\bid="([^"]+)"/g)].map(match => match[1]));
  const anchors = [...html.matchAll(/<a\b[^>]*\bhref="#([^"]+)"/g)].map(match => match[1]);
  assert.ok(anchors.length > 0);
  for (const anchor of anchors) assert.ok(ids.has(anchor), `missing #${anchor}`);
  assert.match(html, /class="icon-button theme-toggle"[^>]*aria-label="Switch to dark theme"/);
  assert.match(html, /class="icon-button menu-toggle"[^>]*aria-controls="primary-nav"[^>]*aria-expanded="false"/);
  assert.equal([...html.matchAll(/data-copy-install/g)].length, 2);
  assert.equal([...html.matchAll(/href="https:\/\/forms\.gle\/h5vPosfsbHu4tenA6"/g)].length, 2);
});

test('search metadata and install command match the page', () => {
  assert.equal([...html.matchAll(/<h1\b/g)].length, 1);
  assert.match(html, /<title>Thus Spoke Zakura \| Local Zcash Regtest Network<\/title>/);
  assert.match(html, /<meta name="description" content="[^"]+">/);
  assert.match(html, /<meta property="og:image" content="https:\/\/[^"]+">/);
  assert.match(html, /<link rel="icon" href="\.\/assets\/favicon\.svg"/);
  const command = html.match(/<code id="install-command">([^<]+)<\/code>/)?.[1];
  assert.ok(command);
  assert.ok(readme.includes(command));
});

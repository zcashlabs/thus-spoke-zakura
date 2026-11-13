import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const read = name => readFile(new URL(`../${name}`, import.meta.url), 'utf8');
const html = await read('index.html');
const markdown = await read('index.md');
const llms = await read('llms.txt');
const robots = await read('robots.txt');
const notFound = await read('404.html');
const styles = await read('styles.css');
const vercel = JSON.parse(await read('vercel.json'));

const BOOK = 'https://ths.zypherpunk.xyz/book/';

test('structured data is one valid graph describing the software', () => {
  const blocks = [...html.matchAll(/<script type="application\/ld\+json">([\s\S]*?)<\/script>/g)];
  assert.equal(blocks.length, 1);
  const data = JSON.parse(blocks[0][1]);
  assert.equal(data['@context'], 'https://schema.org');
  const graph = data['@graph'];
  const ids = new Set(graph.map(node => node['@id']));
  for (const node of graph) {
    assert.ok(node['@type'] && node['@id'], 'every node needs @type and @id');
    for (const value of Object.values(node)) {
      if (value && typeof value === 'object' && Object.keys(value).length === 1 && value['@id']) {
        assert.ok(ids.has(value['@id']), `dangling reference ${value['@id']}`);
      }
    }
  }
  const app = graph.find(node => node['@type'] === 'SoftwareApplication');
  assert.equal(app.name, 'Thus Spoke Zakura');
  assert.equal(app.applicationCategory, 'DeveloperApplication');
  assert.equal(app.offers.price, '0');
  assert.ok(app.operatingSystem.includes('macOS') && app.operatingSystem.includes('Linux'));
  // Rich-result types Google has retired; they add nothing and invite stale markup.
  assert.ok(!graph.some(node => ['FAQPage', 'HowTo'].includes(node['@type'])));
});

test('documentation links point at the published book in every format', () => {
  const docsSection = html.match(/<div class="docs-list"[\s\S]*?<\/div>/)[0];
  const docs = [...docsSection.matchAll(/href="([^"]+)"/g)].map(match => match[1]);
  assert.equal(docs.length, 9);
  for (const href of docs) {
    assert.ok(href.startsWith(BOOK) && href.endsWith('.html'), `unexpected docs link ${href}`);
    assert.ok(llms.includes(`](${href})`), `llms.txt is missing ${href}`);
    assert.ok(markdown.includes(`](${href})`), `index.md is missing ${href}`);
  }
  // GitHub source links must not pin a branch that can be deleted.
  for (const [, href] of html.matchAll(/href="(https:\/\/github\.com\/[^"]+\/(?:blob|tree)\/[^"]+)"/g)) {
    assert.match(href, /\/(?:blob|tree)\/main\//);
  }
});

test('llms.txt passes the Lighthouse llms-txt rules', () => {
  assert.match(llms, /^# \S/);
  assert.match(llms, /^> \S/m);
  assert.match(llms, /\[[^\]]+\]\(https:\/\/[^)]+\)/);
  assert.ok(llms.length > 50);
});

test('robots.txt keeps search and AI-search crawlers allowed', () => {
  const groups = robots.split(/\n\s*\n/).map(block => ({
    agents: [...block.matchAll(/^User-agent:\s*(\S+)/gim)].map(match => match[1]),
    disallow: [...block.matchAll(/^Disallow:\s*(\S*)/gim)].map(match => match[1]),
    signal: block.match(/^Content-Signal:\s*(.+)$/im)?.[1],
  })).filter(group => group.agents.length);
  for (const agent of ['*', 'OAI-SearchBot', 'Claude-SearchBot', 'PerplexityBot']) {
    const group = groups.find(candidate => candidate.agents.includes(agent));
    assert.ok(group, `no robots.txt group for ${agent}`);
    assert.ok(!group.disallow.includes('/'), `${agent} is blocked`);
  }
  for (const group of groups) assert.match(group.signal ?? '', /search=yes, ai-input=yes/);
});

test('every inline script is allowed by the Content-Security-Policy hash list', () => {
  const csp = vercel.headers.flatMap(rule => rule.headers)
    .find(header => header.key === 'Content-Security-Policy').value;
  const inline = [...html.matchAll(/<script(?![^>]*\bsrc=)([^>]*)>([\s\S]*?)<\/script>/g)]
    .filter(([, attrs]) => !attrs.includes('application/ld+json'));
  assert.ok(inline.length > 0);
  for (const [, , body] of inline) {
    const hash = `'sha256-${createHash('sha256').update(body).digest('base64')}'`;
    assert.ok(csp.includes(hash), `CSP is missing ${hash}; update vercel.json after editing the inline script`);
  }
  for (const [, src] of html.matchAll(/<script[^>]*\bsrc="(https:[^"]+)"/g)) {
    assert.ok(csp.includes(new URL(src).origin), `CSP script-src is missing ${new URL(src).origin}`);
  }
  if (html.includes('https://avatars.githubusercontent.com/')) assert.match(csp, /img-src[^;]*https:\/\/avatars\.githubusercontent\.com/);
  // styles.css draws its noise texture and petal from data: SVGs.
  if (/url\(["']?data:image\//.test(styles)) assert.match(csp, /img-src[^;]* data:/);
});

test('the landing page CSP does not block mdBook scripts', () => {
  const rules = vercel.headers.filter(rule => rule.headers.some(header => header.key === 'Content-Security-Policy'));
  assert.ok(rules.length > 0);
  for (const rule of rules) {
    const pattern = new RegExp(`^${rule.source}$`);
    assert.ok(pattern.test('/'));
    assert.ok(pattern.test('/index.html'));
    assert.ok(!pattern.test('/book/index.html'));
    assert.ok(!pattern.test('/book/architecture/overview.html'));
  }
});

test('decorative rail links stay out of the tab order', () => {
  const rail = html.match(/<div class="rail" aria-hidden="true">([\s\S]*?)<\/div>/)[1];
  const links = [...rail.matchAll(/<a\b[^>]*>/g)].map(match => match[0]);
  assert.ok(links.length > 0);
  for (const link of links) assert.match(link, /tabindex="-1"/);
});

test('the not-found page is excluded from the index', () => {
  assert.match(notFound, /<meta name="robots" content="noindex">/);
  assert.match(notFound, /<a class="primary-link" href="\/">/);
});

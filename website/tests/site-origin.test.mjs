import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const read = name => readFile(new URL(`../${name}`, import.meta.url), 'utf8');
const html = await read('index.html');
const robots = await read('robots.txt');
const sitemap = await read('sitemap.xml');
const llms = await read('llms.txt');
const vercel = JSON.parse(await read('vercel.json'));

const canonical = html.match(/<link rel="canonical" href="([^"]+)">/)?.[1];
const origin = canonical && new URL(canonical).origin;
const meta = key => html.match(new RegExp(`<meta (?:property|name)="${key}" content="([^"]+)">`))?.[1];

test('canonical URL is the HTTPS site root', () => {
  assert.ok(canonical, 'missing canonical link');
  assert.equal(canonical, `${origin}/`);
  assert.equal(new URL(canonical).protocol, 'https:');
});

test('social, sitemap, robots and JSON-LD URLs use the canonical origin', () => {
  assert.equal(meta('og:url'), canonical);
  assert.equal(meta('og:image'), `${origin}/assets/social.png`);
  assert.equal(meta('twitter:image'), `${origin}/assets/social.png`);
  assert.deepEqual([...sitemap.matchAll(/<loc>([^<]+)<\/loc>/g)].map(match => match[1]), [canonical]);
  assert.match(robots, new RegExp(`^Sitemap: ${origin}/sitemap\\.xml$`, 'm'));
  assert.ok(llms.includes(`](${origin}/index.md)`), 'llms.txt must link the Markdown page on the site origin');

  const markdownRule = vercel.headers.find(rule => rule.source === '/index.md');
  const link = markdownRule.headers.find(header => header.key === 'Link').value;
  assert.equal(link, `<${canonical}>; rel="canonical"`);

  const graph = JSON.parse(html.match(/<script type="application\/ld\+json">([\s\S]*?)<\/script>/)[1])['@graph'];
  for (const node of graph.filter(node => node['@type'] !== 'Organization')) {
    assert.ok(node['@id'].startsWith(`${origin}/#`), `${node['@type']} @id is off-site`);
  }
});

/* The public HTTPS address is not decided yet. `.example` is reserved (RFC 2606), so the
   placeholder can never resolve to someone else's site. Reported as TODO, not a failure. */
test('public origin replaces the placeholder', { todo: 'replace https://thus-spoke-zakura.example in website/' }, () => {
  assert.notEqual(new URL(canonical).hostname, 'thus-spoke-zakura.example');
});

import assert from 'node:assert/strict';
import test from 'node:test';
import { execFileSync } from 'node:child_process';
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('..', import.meta.url));

test('packed package works without ZcashJS or a source checkout and exposes usable types', async t => {
  const directory = await mkdtemp(join(tmpdir(), 'thsjs-consumer-'));
  t.after(() => rm(directory, { recursive: true, force: true }));
  let pack;
  assert.doesNotThrow(() => {
    pack = JSON.parse(execFileSync('npm', ['pack', '--json', '--pack-destination', directory],
      { cwd: root, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] }))[0];
  }, 'thsjs must be independently packable');
  assert(pack.files.some(file => file.path === 'LICENSE'));
  assert(pack.files.some(file => file.path === 'dist/index.d.ts'));
  assert(pack.files.every(file => !/^(src|tests|integration|node_modules)\//.test(file.path)));
  await writeFile(join(directory, 'package.json'), '{"private":true,"type":"module"}');
  execFileSync('npm', ['install', '--ignore-scripts', '--offline', '--no-audit', '--no-fund',
    join(directory, pack.filename)], { cwd: directory, stdio: 'pipe' });
  const installed = JSON.parse(await readFile(join(directory, 'node_modules/@zcashlabs/thsjs/package.json')));
  assert.deepEqual(installed.dependencies ?? {}, {});
  assert.deepEqual(installed.peerDependencies ?? {}, {});
  const fixture = fileURLToPath(new URL('./ths-fixture.mjs', import.meta.url));
  const consumer = `import assert from 'node:assert/strict';
import { withTestTHS } from '@zcashlabs/thsjs';
let url;
await withTestTHS(async ths => {
  url = ths.config().dashboard;
  assert.equal(await ths.rpc('getblockcount'), 200);
  await ths.produceBlock(3);
  assert.equal(await ths.rpc('getblockcount'), 203);
}, { executable: ${JSON.stringify(fixture)}, runtimeDir: 'normal', timeoutMs: 5000 });
await assert.rejects(fetch(url));
`;
  await writeFile(join(directory, 'consumer.mjs'), consumer);
  execFileSync(process.execPath, ['consumer.mjs'], { cwd: directory, stdio: 'pipe' });
  assert.throws(() => execFileSync(process.execPath,
    ['--conditions=browser', '--input-type=module', '-e', "import('@zcashlabs/thsjs')"],
    { cwd: directory, stdio: 'pipe' }), error => /ERR_PACKAGE_PATH_NOT_EXPORTED/.test(String(error.stderr)));
  await writeFile(join(directory, 'consumer.mts'), `import { createTestTHSClient, withTestTHS, type TestTHSClient } from '@zcashlabs/thsjs';
const ths: TestTHSClient = createTestTHSClient({ timeoutMs: 1000 });
const result: Promise<number> = withTestTHS(async client => { await client.produceBlock(1); return 42; });
const funding = ths.fundAddress('example', 100n, { idempotencyKey: 'test-key' });
// @ts-expect-error Amounts must stay exact bigint zatoshi.
ths.fundAddress('example', 100);
// @ts-expect-error Existing instances are never attached to.
createTestTHSClient({ instance: 'another-owner' });
void result; void funding;
`);
  const compiler = join(dirname(fileURLToPath(import.meta.resolve('typescript'))), 'tsc.js');
  execFileSync(process.execPath, [compiler, '--noEmit', '--strict', '--module', 'NodeNext',
    '--moduleResolution', 'NodeNext', '--target', 'ES2022', 'consumer.mts'],
  { cwd: directory, stdio: 'pipe', encoding: 'utf8' });
  execFileSync(process.execPath, [compiler, '--noEmit', '--strict', '--module', 'ESNext',
    '--moduleResolution', 'Bundler', '--target', 'ES2022', 'consumer.mts'],
  { cwd: directory, stdio: 'pipe', encoding: 'utf8' });
});

import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';

test('native package includes executable components, web assets, pins and licenses', () => {
  const dir = mkdtempSync(join(tmpdir(), 'ths-package-'));
  try {
    const bin = join(dir, 'bin');
    const web = join(dir, 'web');
    const output = join(dir, 'output');
    mkdirSync(bin);
    mkdirSync(web);
    for (const name of ['zakurad', 'lightwalletd', 'ths-server']) {
      writeFileSync(join(bin, name), '#!/bin/sh\nexit 0\n', { mode: 0o755 });
    }
    writeFileSync(join(web, 'index.html'), '<html>THS</html>');
    const pack = () => spawnSync(process.execPath, ['tools/package_native.mjs', bin, web, output], { encoding: 'utf8' });
    chmodSync(join(bin, 'zakurad'), 0o644);
    assert.notEqual(pack().status, 0);
    assert.equal(existsSync(output), false);
    chmodSync(join(bin, 'zakurad'), 0o755);
    const result = pack();
    assert.equal(result.status, 0, result.stderr);
    const manifest = JSON.parse(readFileSync(join(output, 'ths-runtime/manifest.json')));
    const pins = JSON.parse(readFileSync('native/runtime.json'));
    assert.equal(manifest.format, 1);
    assert.equal(manifest.zakura_commit, pins.zakura_commit);
    assert.equal(manifest.lightwalletd_commit, pins.lightwalletd_commit);
    const archive = readdirSync(output).find(name => name.endsWith('.tar.gz'));
    assert.equal(archive, `ths-runtime-${manifest.target}.tar.gz`);
    const contents = spawnSync('tar', ['-tzf', join(output, archive)], { encoding: 'utf8' });
    assert.equal(contents.status, 0, contents.stderr);
    for (const path of ['bin/zakurad', 'bin/lightwalletd', 'bin/ths-server', 'web/index.html', 'manifest.json', 'licenses/ths', 'licenses/zakura-MIT', 'licenses/lightwalletd']) {
      assert(contents.stdout.split('\n').includes(`ths-runtime/${path}`), path);
    }
    assert(contents.stdout.trim().split('\n').every(path => path.startsWith('ths-runtime/')));
    writeFileSync(join(output, 'keep'), 'existing output');
    assert.notEqual(pack().status, 0);
    assert.equal(readFileSync(join(output, 'keep'), 'utf8'), 'existing output');
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

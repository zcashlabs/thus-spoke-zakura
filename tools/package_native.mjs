#!/usr/bin/env node
import { constants, promises as fs } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const [bin, web, output, ...extra] = process.argv.slice(2);
if (!bin || !web || !output || extra.length) {
  throw new Error('Usage: node tools/package_native.mjs BIN_DIR WEB_DIR NEW_OUTPUT_DIR');
}
const targets = {
  'linux-x64': 'x86_64-unknown-linux-gnu',
  'linux-arm64': 'aarch64-unknown-linux-gnu',
  'darwin-x64': 'x86_64-apple-darwin',
  'darwin-arm64': 'aarch64-apple-darwin',
};
const target = targets[`${process.platform}-${process.arch}`];
if (!target) throw new Error('Unsupported native runtime platform');
const cargo = await fs.readFile(join(root, 'Cargo.toml'), 'utf8');
const version = cargo.match(/\[workspace\.package\]\s+version = "([^"]+)"/)?.[1];
if (!version) throw new Error('Cannot determine THS version');
const pins = JSON.parse(await fs.readFile(join(root, 'native/runtime.json'), 'utf8'));
const binaries = ['zakurad', 'lightwalletd', 'ths-server'];
for (const name of binaries) await fs.access(join(bin, name), constants.X_OK);
await fs.access(join(web, 'index.html'), constants.R_OK);
// The destination must be new so a failed package never removes somebody's files.
await fs.mkdir(output);
try {
  const bundle = join(output, 'ths-runtime');
  await fs.mkdir(join(bundle, 'bin'), { recursive: true });
  for (const name of binaries) {
    await fs.copyFile(join(bin, name), join(bundle, 'bin', name));
    await fs.chmod(join(bundle, 'bin', name), 0o755);
  }
  await fs.cp(web, join(bundle, 'web'), { recursive: true, dereference: true });
  await fs.cp(join(root, 'native/licenses'), join(bundle, 'licenses'), { recursive: true });
  await fs.copyFile(join(root, 'LICENSE'), join(bundle, 'licenses/ths'));
  await fs.writeFile(join(bundle, 'manifest.json'), JSON.stringify({
    format: 1, ths_version: version, target,
    zakura_commit: pins.zakura_commit, lightwalletd_commit: pins.lightwalletd_commit,
  }, null, 2) + '\n');
  const archive = resolve(output, `ths-runtime-${target}.tar.gz`);
  const result = spawnSync('tar', ['-czf', archive, '-C', output, 'ths-runtime'], { stdio: 'inherit' });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`Packaging native runtime exited ${result.status}`);
  process.stdout.write(`${archive}\n`);
} catch (error) {
  await fs.rm(output, { recursive: true, force: true });
  throw error;
}

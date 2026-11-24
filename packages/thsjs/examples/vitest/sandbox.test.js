import { afterAll, beforeAll, describe, expect, test } from 'vitest';
import { createTestTHSClient, withTestTHS } from '@zcashlabs/thsjs';

// Unset means: use `ths` from PATH.
const launcher = { executable: process.env.THS_BINARY, runtimeDir: process.env.THS_RUNTIME_DIR };

describe('shared sandbox', () => {
  const ths = createTestTHSClient(launcher);
  // Register stop first so the sandbox is cleaned up even if start fails.
  afterAll(() => ths.stop());
  beforeAll(() => ths.start());

  test('gives your app local endpoints', () => {
    const { network, lightwalletd } = ths.config();
    expect(network).toBe('regtest');
    expect(lightwalletd).toMatch(/^http:\/\/127\.0\.0\.1:\d+/);
  });

  test('mines blocks when you ask', async () => {
    const before = await ths.rpc('getblockcount');
    await ths.produceBlock(3);
    expect(await ths.rpc('getblockcount')).toBe(before + 3);
  });

  test('replaces or removes recent blocks', async () => {
    const height = await ths.rpc('getblockcount');
    const tip = await ths.rpc('getbestblockhash');
    await ths.reorg(1); // Same height, different tip.
    expect(await ths.rpc('getblockcount')).toBe(height);
    expect(await ths.rpc('getbestblockhash')).not.toBe(tip);
    await ths.rollback(1); // One block shorter.
    expect(await ths.rpc('getblockcount')).toBe(height - 1);
  });
});

// Its own sandbox, stopped when the test ends, fails or times out.
test('per-test sandbox', async ({ signal }) => {
  await withTestTHS(async ths => {
    await ths.produceBlock(1);
    expect(await ths.rpc('getblockcount')).toBeGreaterThan(0);
  }, { ...launcher, signal });
});

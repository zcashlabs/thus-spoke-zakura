import { describe, expect, it } from 'vitest';
import { statusSchema } from './schemas';

const baseStatus = {
  instance: 'default',
  node: null,
  account_count: 5,
  auto_mine: true,
  network: 'Regtest',
};

describe('statusSchema', () => {
  it('accepts the server wallet synchronization status', () => {
    const status = statusSchema.parse({
      ...baseStatus,
      wallet_sync: {
        state: 'error',
        fully_scanned_height: 120,
        observed_height: 121,
        last_success_at: 1_789_700_000,
        error: 'lightwalletd unavailable',
      },
    });

    expect(status.wallet_sync?.state).toBe('error');
    expect(status.wallet_sync?.fully_scanned_height).toBe(120);
  });

  it('remains compatible with servers that predate wallet status', () => {
    expect(statusSchema.parse(baseStatus).wallet_sync).toBeUndefined();
  });
});

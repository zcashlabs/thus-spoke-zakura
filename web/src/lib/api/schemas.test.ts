import { describe, expect, it } from 'vitest';
import { accountSchema, miningJobSchema, miningJobResponseSchema, statusSchema } from './schemas';

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

  it('retains settlement status and blocker from the server', () => {
    const status = statusSchema.parse({
      ...baseStatus,
      settled: false,
      settlement_blocker: 'wallet_behind',
    });

    expect(status.settled).toBe(false);
    expect(status.settlement_blocker).toBe('wallet_behind');
  });
});

describe('accountSchema', () => {
  const legacyAccount = {
    id: 1,
    name: 'Account 1',
    unified_address: 'account-unified-address',
    transparent_address: 'account-transparent-address',
    transparent_zatoshi: 123,
    ironwood_zatoshi: 456,
  };

  it('retains the optional viewing key supplied by the server', () => {
    const account = accountSchema.parse({
      ...legacyAccount,
      unified_full_viewing_key: 'opaque-viewing-key-for-schema-test',
    });
    expect(account.unified_full_viewing_key).toBe('opaque-viewing-key-for-schema-test');
    expect(account.ironwood_zatoshi).toBe(456n);
  });

  it('accepts packaged 0.2.1 accounts without a viewing key', () => {
    const account = accountSchema.parse(legacyAccount);
    expect(account.unified_full_viewing_key).toBeUndefined();
    expect(account.transparent_zatoshi).toBe(123n);
  });
});

const job = {
  id: '588e9911-20b6-4c4a-a9ae-5f6c1abae953',
  requested_blocks: 10,
  completed_blocks: 2,
  state: 'mining',
  error: null,
  progress_uncertain: false,
};

describe('miningJobSchema', () => {
  it('accepts job snapshots and an empty latest job', () => {
    expect(miningJobSchema.parse(job)).toEqual(job);
    expect(miningJobResponseSchema.parse({ job: null })).toEqual({ job: null });
  });
  it.each([
    { state: 'unknown' },
    { completed_blocks: -1 },
    { completed_blocks: 11 },
    { requested_blocks: 0 },
    { requested_blocks: 10001 },
    { id: 'bad' },
    { error: undefined },
    { progress_uncertain: undefined },
  ])('rejects malformed snapshots: %j', (change) => {
    expect(miningJobSchema.safeParse({ ...job, ...change }).success).toBe(false);
  });
});

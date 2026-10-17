import { describe, expect, it } from 'vitest';
import {
  accountSchema,
  activitySchema,
  miningJobSchema,
  miningJobResponseSchema,
  statusSchema,
} from './schemas';

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

describe('activitySchema', () => {
  it('rejects unknown activity kinds instead of treating them as sends', () => {
    const result = activitySchema.safeParse({
      id: 'wallet:unknown-kind',
      kind: 'receive',
      source_pool: 'ironwood',
      destination_pool: 'ironwood',
      amount_zatoshi: 1,
      txid: 'ab'.repeat(32),
      status: 'confirmed',
      created_at: '2026-10-10 00:00:00',
    });

    expect(result.success).toBe(false);
  });

  it('preserves every recipient address returned for a transaction', () => {
    const activity = activitySchema.parse({
      id: 'wallet:multi-recipient',
      kind: 'transaction',
      direction: 'outgoing',
      from_account: 1,
      to_account: null,
      to_address: 'uregtest1external1',
      recipient_addresses: ['uregtest1external1', 'uregtest1external2'],
      source_pool: 'ironwood',
      destination_pool: 'ironwood',
      amount_zatoshi: 100_000_000,
      txid: 'ab'.repeat(32),
      block_hash: null,
      status: 'confirmed',
      last_error: null,
      created_at: '2026-10-15 12:00:00',
    });

    expect(activity.recipient_addresses).toEqual(['uregtest1external1', 'uregtest1external2']);
  });

  it('accepts an incoming transaction whose sender creation time is unknown', () => {
    const activity = activitySchema.parse({
      id: 'wallet:incoming',
      kind: 'transaction',
      direction: 'incoming',
      from_account: null,
      to_account: null,
      from_accounts: [],
      to_accounts: [2, 3],
      to_address: null,
      recipient_addresses: ['uregtest1account2'],
      source_pool: null,
      destination_pool: 'ironwood',
      amount_zatoshi: 10_000_000,
      txid: 'cd'.repeat(32),
      block_hash: 'ef'.repeat(32),
      mined_height: 121,
      status: 'confirmed',
      last_error: null,
      created_at: null,
    });

    expect(activity.created_at).toBeNull();
    expect(activity.mined_height).toBe(121);
    expect(activity.to_accounts).toEqual([2, 3]);
  });

  it('rejects the hidden treasury account in affected account lists', () => {
    expect(() =>
      activitySchema.parse({
        id: 'wallet:treasury',
        kind: 'transaction',
        direction: 'incoming',
        from_account: null,
        to_account: null,
        from_accounts: [],
        to_accounts: [2, 6],
        to_address: null,
        source_pool: null,
        destination_pool: 'ironwood',
        amount_zatoshi: 10,
        txid: 'ab'.repeat(32),
        block_hash: null,
        status: 'confirmed',
        last_error: null,
        created_at: null,
      }),
    ).toThrow();
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

import { describe, expect, it } from 'vitest';
import {
  accountSchema,
  activitySchema,
  walletTransactionSchema,
  addressSchema,
  miningJobSchema,
  miningJobResponseSchema,
  statusSchema,
  transactionSchema,
} from './schemas';

const baseStatus = {
  instance: 'default',
  node: null,
  account_count: 5,
  mining_mode: 'auto' as const,
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

  it('accepts every mining mode and their fields', () => {
    expect(statusSchema.parse(baseStatus).mining_mode).toBe('auto');
    const interval = statusSchema.parse({
      ...baseStatus,
      mining_mode: 'interval',
      block_time: 5,
    });
    expect(interval.block_time).toBe(5);
    expect(statusSchema.parse({ ...baseStatus, mining_mode: 'mempool' }).mine_on_mempool).toBe(
      undefined,
    );
    expect(statusSchema.parse({ ...baseStatus, mining_mode: 'manual' }).mining_mode).toBe('manual');
  });

  it('remains compatible with servers that predate mining modes', () => {
    const legacy: Record<string, unknown> = { ...baseStatus };
    delete legacy.mining_mode;
    expect(statusSchema.parse(legacy).mining_mode).toBeUndefined();
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

  it('keeps the text memos the wallet decoded', () => {
    const activity = activitySchema.parse({
      id: 'wallet:memo',
      kind: 'transaction',
      direction: 'internal',
      from_account: 1,
      to_account: 2,
      source_pool: 'ironwood',
      destination_pool: 'ironwood',
      amount_zatoshi: 10_000_000,
      txid: 'ab'.repeat(32),
      status: 'confirmed',
      created_at: null,
      memos: ['order 42'],
    });

    expect(activity.memos).toEqual(['order 42']);
  });

  it('preserves every known source and destination pool', () => {
    const activity = activitySchema.parse({
      id: 'wallet:multi-pool',
      kind: 'transaction',
      direction: 'internal',
      from_account: 1,
      to_account: 2,
      to_address: null,
      source_pool: null,
      destination_pool: null,
      source_pools: ['ironwood', 'transparent'],
      destination_pools: ['ironwood', 'transparent'],
      amount_zatoshi: 100_000_000,
      received_zatoshi: 90_000_000,
      spent_zatoshi: 200_000_000,
      net_zatoshi: -110_000_000,
      txid: 'ab'.repeat(32),
      block_hash: null,
      status: 'confirmed',
      last_error: null,
      created_at: '2026-10-15 12:00:00',
    });

    expect(activity.source_pools).toEqual(['ironwood', 'transparent']);
    expect(activity.destination_pools).toEqual(['ironwood', 'transparent']);
    expect(activity.received_zatoshi).toBe(90_000_000n);
    expect(activity.spent_zatoshi).toBe(200_000_000n);
    expect(activity.net_zatoshi).toBe(-110_000_000n);
  });

  it('uses null value effects when canonical wallet data is unavailable', () => {
    const activity = activitySchema.parse({
      id: 'pending-send',
      kind: 'send',
      from_account: 1,
      to_account: 2,
      source_pool: 'ironwood',
      destination_pool: 'ironwood',
      amount_zatoshi: 100_000_000,
      received_zatoshi: null,
      spent_zatoshi: null,
      net_zatoshi: null,
      txid: '',
      status: 'preparing',
      created_at: '2026-10-15 12:00:00',
    });

    expect(activity.received_zatoshi).toBeNull();
    expect(activity.spent_zatoshi).toBeNull();
    expect(activity.net_zatoshi).toBeNull();
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

describe('transactionSchema', () => {
  const transaction = {
    txid: 'ab'.repeat(32),
    vin: [],
    vShieldedSpend: [],
    vShieldedOutput: [],
  };

  it('accepts outputs with distinct node-provided indices', () => {
    const parsed = transactionSchema.parse({
      ...transaction,
      vout: [
        { n: 0, valueZat: 1 },
        { n: 1, valueZat: 2 },
      ],
    });
    expect(parsed.vout.map((output) => output.n)).toEqual([0, 1]);
  });

  it.each([
    { vout: [{ valueZat: 1 }, { n: 1, valueZat: 2 }] },
    {
      vout: [
        { n: 0, valueZat: 1 },
        { n: 0, valueZat: 2 },
      ],
    },
  ])('rejects outputs with missing or duplicate indices: $vout', ({ vout }) => {
    expect(transactionSchema.safeParse({ ...transaction, vout }).success).toBe(false);
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

describe('addressSchema', () => {
  it('accepts integer balances and fills missing fields with zero', () => {
    expect(
      addressSchema.parse({
        address: 'tmAddress',
        balance: { balance: 100_000_000, received: 250_000_000 },
      }).balance,
    ).toEqual({ balance: 100_000_000, received: 250_000_000 });
    expect(addressSchema.parse({ address: 'tmAddress', balance: {} }).balance).toEqual({
      balance: 0,
      received: 0,
    });
  });
  it.each([{ balance: 1.5 }, { received: 100.25 }, { balance: -1 }, { received: -5 }])(
    'rejects non-integer balances: %j',
    (balance) => {
      expect(
        addressSchema.safeParse({
          address: 'tmAddress',
          balance: { balance: 0, received: 0, ...balance },
        }).success,
      ).toBe(false);
    },
  );
});

describe('walletTransactionSchema', () => {
  const view = {
    txid: 'ab'.repeat(32),
    status: 'confirmed',
    mined_height: 107,
    confirmations: 1,
    fee_zatoshi: 10_000,
    details: 'complete',
    outputs: [
      {
        pool: 'ironwood',
        index: 0,
        value_zatoshi: 10_000_000,
        from: 'faucet',
        to: 2,
        to_address: 'uregtest1two',
        change: false,
        memo: { text: 'order 42' },
      },
    ],
    spends: [{ account: 1, pool: 'ironwood', value_zatoshi: 100_000_000 }],
  };

  it('parses values as bigint and keeps the faucet as a sender', () => {
    const parsed = walletTransactionSchema.parse(view);
    expect(parsed.fee_zatoshi).toBe(10_000n);
    expect(parsed.outputs[0]?.from).toBe('faucet');
    expect(parsed.outputs[0]?.value_zatoshi).toBe(10_000_000n);
  });

  it.each([
    ['an unknown details state', { details: 'partial' }],
    ['the faucet as a recipient', { outputs: [{ ...view.outputs[0], to: 'faucet' }] }],
    ['the treasury account', { spends: [{ account: 6, pool: 'ironwood', value_zatoshi: 1 }] }],
    [
      'a memo with both forms',
      { outputs: [{ ...view.outputs[0], memo: { text: 'a', hex: '61' } }] },
    ],
  ])('rejects %s', (_, change) => {
    expect(walletTransactionSchema.safeParse({ ...view, ...change }).success).toBe(false);
  });
});

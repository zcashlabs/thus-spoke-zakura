import { describe, expect, it } from 'vitest';
import { actionSummary, summariseShielding, transactionFee } from './shielding';
import type { Transaction, TxOutput } from '@/lib/api';

const output = (valueZat = 0): TxOutput => ({ n: 0, valueZat });

const base: Transaction = {
  txid: 'a'.repeat(64),
  vin: [],
  vout: [],
  vShieldedSpend: [],
  vShieldedOutput: [],
};

describe('summariseShielding', () => {
  it('recognises a fully shielded Orchard transfer', () => {
    // Shape taken from a real /transactions response on a live regtest.
    const summary = summariseShielding({
      ...base,
      orchard: { actions: [{}, {}], valueBalanceZat: 0 },
    });
    expect(summary.orchardActions).toBe(2);
    expect(summary.transparentInputs).toBe(0);
    expect(summary.transparentOutputs).toBe(0);
    expect(summary.fullyShielded).toBe(true);
    expect(summary.mixed).toBe(false);
  });

  it('recognises a shielding/deshielding transaction as mixed', () => {
    const summary = summariseShielding({
      ...base,
      vout: [output(39_072_500)],
      orchard: { actions: [{}, {}] },
    });
    expect(summary.fullyShielded).toBe(false);
    expect(summary.mixed).toBe(true);
  });

  it('treats a transparent-only transaction as neither', () => {
    const summary = summariseShielding({ ...base, vin: [{}], vout: [output(), output()] });
    expect(summary.fullyShielded).toBe(false);
    expect(summary.mixed).toBe(false);
    expect(summary.transparentOutputs).toBe(2);
  });

  it('does not claim full shielding when the pool value balance is public', () => {
    // No transparent inputs or outputs, but a non-zero Orchard value balance:
    // the fee is visible on chain even though sender, recipient and amount are
    // not. ZIP 224 makes this field part of the transaction.
    const summary = summariseShielding({
      txid: 'ab'.repeat(32),
      vin: [],
      vout: [],
      vShieldedSpend: [],
      vShieldedOutput: [],
      orchard: { actions: [{}, {}], valueBalanceZat: 10_000 },
    });

    expect(summary.shieldedOnly).toBe(true);
    expect(summary.fullyShielded).toBe(false);
    expect(summary.publicValues).toEqual([{ pool: 'Orchard', zatoshi: 10_000 }]);
  });

  it('claims full shielding only when the balance is zero too', () => {
    const summary = summariseShielding({
      txid: 'ab'.repeat(32),
      vin: [],
      vout: [],
      vShieldedSpend: [],
      vShieldedOutput: [],
      orchard: { actions: [{}, {}], valueBalanceZat: 0 },
    });

    expect(summary.fullyShielded).toBe(true);
  });

  it('counts Ironwood actions as shielded once NU6.3 is active', () => {
    // Shape of a v6 send on a live NU6.3 regtest: two Ironwood actions, an
    // empty Orchard bundle, and the fee as the Ironwood value balance.
    const summary = summariseShielding({
      ...base,
      orchard: { actions: [], valueBalanceZat: 0 },
      ironwood: { actions: [{}, {}], valueBalanceZat: 10_000 },
    });

    expect(summary.ironwoodActions).toBe(2);
    expect(summary.orchardActions).toBe(0);
    expect(summary.shieldedActions).toBe(2);
    expect(summary.shieldedOnly).toBe(true);
    expect(summary.fullyShielded).toBe(false);
    expect(summary.publicValues).toEqual([{ pool: 'Ironwood', zatoshi: 10_000 }]);
  });

  it('keeps equal and opposite pool balances separate', () => {
    const summary = summariseShielding({
      ...base,
      orchard: { actions: [{}], valueBalanceZat: -5_000 },
      ironwood: { actions: [{}], valueBalanceZat: 5_000 },
    });

    expect(summary.publicValues).toEqual([
      { pool: 'Ironwood', zatoshi: 5_000 },
      { pool: 'Orchard', zatoshi: -5_000 },
    ]);
    expect(summary.fullyShielded).toBe(false);
  });

  it('does not claim full shielding when a used pool omits its balance', () => {
    expect(summariseShielding({ ...base, orchard: { actions: [{}] } }).fullyShielded).toBe(false);
  });
});

describe('transactionFee', () => {
  it('calculates a shielded-only fee from complete pool balances', () => {
    expect(
      transactionFee({ ...base, ironwood: { actions: [{}], valueBalanceZat: 10_000 } }),
    ).toEqual({ kind: 'known', zatoshi: 10_000n });
  });

  it('calculates a mixed-pool fee without discarding either public balance', () => {
    expect(
      transactionFee({
        ...base,
        vin: [{ valueZat: 100_000_000 }],
        vout: [output(49_985_000)],
        ironwood: { actions: [{}], valueBalanceZat: -50_000_000 },
        orchard: { actions: [{}], valueBalanceZat: 5_000 },
      }),
    ).toEqual({ kind: 'known', zatoshi: 20_000n });
  });

  it('does not guess a fee with an unresolved transparent input or pool balance', () => {
    expect(
      transactionFee({ ...base, vin: [{ txid: 'a'.repeat(64), vout: 0 }], vout: [output(1)] }),
    ).toEqual({ kind: 'unknown', reason: 'A transparent input value is unavailable.' });
    expect(transactionFee({ ...base, vin: [{ valueZat: 100_000 }], vout: [{ n: 0 }] })).toEqual({
      kind: 'unknown',
      reason: 'A transparent output value is unavailable.',
    });
    expect(transactionFee({ ...base, orchard: { actions: [{}] } })).toEqual({
      kind: 'unknown',
      reason: 'A shielded pool value balance is unavailable.',
    });
  });

  it('handles coinbase explicitly', () => {
    expect(
      transactionFee({ ...base, vin: [{ coinbase: '03' }], vout: [output(100_000_000)] }),
    ).toEqual({ kind: 'coinbase' });
  });

  it('does not omit legacy JoinSplit values from a fee calculation', () => {
    expect(
      transactionFee({
        ...base,
        vjoinsplit: [{}],
        vin: [{ valueZat: 100_000 }],
        vout: [output(90_000)],
      }),
    ).toEqual({
      kind: 'unknown',
      reason: 'Legacy JoinSplit values are not available in this view.',
    });
  });
});

describe('actionSummary', () => {
  it('names actions by pool', () => {
    const summarise = (ironwood: number, orchard: number) =>
      actionSummary(
        summariseShielding({
          ...base,
          ironwood: { actions: Array.from({ length: ironwood }, () => ({})) },
          orchard: { actions: Array.from({ length: orchard }, () => ({})) },
        }),
      );

    expect(summarise(2, 0)).toBe('2 Ironwood actions');
    expect(summarise(0, 1)).toBe('1 Orchard action');
    expect(summarise(1, 2)).toBe('1 Ironwood action and 2 Orchard actions');
    expect(summarise(0, 0)).toBe('0 shielded actions');
  });

  it('names Sapling spends and outputs, which external clients can still create', () => {
    const summary = summariseShielding({
      ...base,
      vShieldedSpend: [{}],
      vShieldedOutput: [{}, {}],
      ironwood: { actions: [{}, {}] },
    });
    expect(actionSummary(summary)).toBe(
      '2 Ironwood actions, 1 Sapling spend and 2 Sapling outputs',
    );
  });
});

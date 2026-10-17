import type { Transaction } from '@/lib/api';

export interface ShieldingSummary {
  ironwoodActions: number;
  orchardActions: number;
  /** Ironwood and Orchard actions together. */
  shieldedActions: number;
  transparentInputs: number;
  transparentOutputs: number;
  saplingSpends: number;
  saplingOutputs: number;
  publicValues: Array<{ pool: 'Ironwood' | 'Orchard' | 'Sapling'; zatoshi: number | null }>;
  /** True when the transfer touches no transparent input or output. */
  shieldedOnly: boolean;
  /** True when no transparent value or pool value movement is disclosed. */
  fullyShielded: boolean;
  /** True when value crosses the shielded/transparent boundary. */
  mixed: boolean;
}

/**
 * Summarises what a transaction actually reveals publicly. An Ironwood or
 * Orchard transfer hides sender, recipient and amount inside its actions — but the
 * value balance of each used pool stays public. Equal and opposite balances
 * across pools still disclose a transfer between those pools.
 */
export function summariseShielding(tx: Transaction): ShieldingSummary {
  const ironwoodActions = tx.ironwood?.actions.length ?? 0;
  const orchardActions = tx.orchard?.actions.length ?? 0;
  const shieldedActions = ironwoodActions + orchardActions;
  const transparentInputs = tx.vin.length;
  const transparentOutputs = tx.vout.length;
  const saplingSpends = tx.vShieldedSpend.length;
  const saplingOutputs = tx.vShieldedOutput.length;

  const shieldedPresent = shieldedActions > 0 || saplingSpends > 0 || saplingOutputs > 0;
  const transparentPresent = transparentInputs > 0 || transparentOutputs > 0;
  const publicValues: ShieldingSummary['publicValues'] = [];
  if (ironwoodActions > 0 || tx.ironwood?.valueBalanceZat) {
    publicValues.push({ pool: 'Ironwood', zatoshi: tx.ironwood?.valueBalanceZat ?? null });
  }
  if (orchardActions > 0 || tx.orchard?.valueBalanceZat) {
    publicValues.push({ pool: 'Orchard', zatoshi: tx.orchard?.valueBalanceZat ?? null });
  }
  if (saplingSpends + saplingOutputs > 0 || tx.valueBalanceZat) {
    publicValues.push({ pool: 'Sapling', zatoshi: tx.valueBalanceZat ?? null });
  }

  return {
    ironwoodActions,
    orchardActions,
    shieldedActions,
    transparentInputs,
    transparentOutputs,
    saplingSpends,
    saplingOutputs,
    publicValues,
    shieldedOnly: shieldedPresent && !transparentPresent,
    fullyShielded:
      shieldedPresent &&
      !transparentPresent &&
      publicValues.length > 0 &&
      publicValues.every(({ zatoshi }) => zatoshi === 0),
    mixed: shieldedPresent && transparentPresent,
  };
}

export type FeeDisclosure =
  { kind: 'known'; zatoshi: bigint } | { kind: 'unknown'; reason: string } | { kind: 'coinbase' };

/** The fee is the transparent input total minus outputs plus each pool's signed value balance. */
export function transactionFee(tx: Transaction): FeeDisclosure {
  if (tx.vin.some((input) => input.coinbase !== undefined)) return { kind: 'coinbase' };
  if (tx.vjoinsplit?.length) {
    return { kind: 'unknown', reason: 'Legacy JoinSplit values are not available in this view.' };
  }
  if (tx.vin.some((input) => !Number.isSafeInteger(input.valueZat))) {
    return { kind: 'unknown', reason: 'A transparent input value is unavailable.' };
  }
  if (tx.vout.some((output) => !Number.isSafeInteger(output.valueZat))) {
    return { kind: 'unknown', reason: 'A transparent output value is unavailable.' };
  }
  const publicValues = summariseShielding(tx).publicValues;
  if (publicValues.some(({ zatoshi }) => !Number.isSafeInteger(zatoshi))) {
    return { kind: 'unknown', reason: 'A shielded pool value balance is unavailable.' };
  }
  if (tx.vin.length === 0 && publicValues.length === 0) {
    return { kind: 'unknown', reason: 'No spend values are available.' };
  }

  const inputTotal = tx.vin.reduce((sum, input) => sum + BigInt(input.valueZat!), 0n);
  const outputTotal = tx.vout.reduce((sum, output) => sum + BigInt(output.valueZat!), 0n);
  const poolTotal = publicValues.reduce((sum, pool) => sum + BigInt(pool.zatoshi!), 0n);
  const zatoshi = inputTotal - outputTotal + poolTotal;
  if (zatoshi < 0n) return { kind: 'unknown', reason: 'The available values do not balance.' };
  return { kind: 'known', zatoshi };
}

function count(n: number, what: string): string {
  return `${n} ${what}${n === 1 ? '' : 's'}`;
}

/** Names the shielded parts by pool, e.g. "2 Ironwood actions" or "1 Sapling spend". */
export function actionSummary(summary: ShieldingSummary): string {
  const parts = [];
  if (summary.ironwoodActions > 0) parts.push(count(summary.ironwoodActions, 'Ironwood action'));
  if (summary.orchardActions > 0) parts.push(count(summary.orchardActions, 'Orchard action'));
  if (summary.saplingSpends > 0) parts.push(count(summary.saplingSpends, 'Sapling spend'));
  if (summary.saplingOutputs > 0) parts.push(count(summary.saplingOutputs, 'Sapling output'));
  const last = parts.pop();
  if (last === undefined) return count(0, 'shielded action');
  return parts.length === 0 ? last : `${parts.join(', ')} and ${last}`;
}

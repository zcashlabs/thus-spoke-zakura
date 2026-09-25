import { z } from 'zod';
import { parseZec, ZATOSHIS_PER_ZEC } from '@/lib/money';

/** Server-side faucet ceiling (`api.rs`: `5 * ZATOSHIS_PER_ZEC`). */
export const FAUCET_MAX_ZATOSHI = 5n * ZATOSHIS_PER_ZEC;

/** Server-side mining bounds (`api.rs`: `(1..=10_000)`). */
export const MINE_MIN_BLOCKS = 1;
export const MINE_MAX_BLOCKS = 10_000;

const poolField = z.enum(['transparent', 'orchard']);

/** Selects and inputs hand back strings; the schema owns the conversion. */
const accountIdField = z
  .string()
  .regex(/^[1-5]$/, 'Choose a development account.')
  .transform(Number);

/**
 * Amounts stay as strings through the form so the user's keystrokes are never
 * reinterpreted, and are converted to bigint zatoshi exactly once, here.
 */
const amountField = z
  .string()
  .min(1, 'Enter an amount.')
  .transform((value, ctx) => {
    const zatoshi = parseZec(value);
    if (zatoshi === null) {
      ctx.addIssue({ code: 'custom', message: 'Enter a number with up to 8 decimal places.' });
      return z.NEVER;
    }
    if (zatoshi <= 0n) {
      ctx.addIssue({ code: 'custom', message: 'Amount must be greater than zero.' });
      return z.NEVER;
    }
    return zatoshi;
  });

/** ZIP-302 memo size; the limit is in UTF-8 bytes, not characters. */
export const MEMO_MAX_BYTES = 512;

export const memoByteLength = (memo: string) => new TextEncoder().encode(memo).length;

// A disabled textarea can surface as undefined; treat that as "no memo".
const memoField = z
  .string()
  .default('')
  .refine(
    (memo) => memoByteLength(memo) <= MEMO_MAX_BYTES,
    `Memos are limited to ${MEMO_MAX_BYTES} bytes.`,
  )
  // Memos are zero-padded, so a trailing NUL would be lost on decode.
  .refine((memo) => !memo.endsWith('\0'), 'A memo cannot end with a NUL character.');

export const sendSchema = z
  .object({
    from_account: accountIdField,
    to_account: accountIdField,
    source_pool: poolField,
    destination_pool: poolField,
    amount: amountField,
    memo: memoField,
  })
  .refine((values) => values.from_account !== values.to_account, {
    message: 'Pick a different account — sending to yourself only costs the fee.',
    path: ['to_account'],
  })
  .refine((values) => values.memo === '' || values.destination_pool === 'orchard', {
    message: 'Transparent outputs cannot carry a memo. Choose the orchard pool.',
    path: ['memo'],
  });
export type SendInput = z.input<typeof sendSchema>;
export type SendValues = z.output<typeof sendSchema>;

export const faucetSchema = z.object({
  account_id: accountIdField,
  pool: poolField,
  amount: amountField.refine(
    (zatoshi) => zatoshi <= FAUCET_MAX_ZATOSHI,
    'The faucet is limited to 5 ZEC per request.',
  ),
});
export type FaucetInput = z.input<typeof faucetSchema>;
export type FaucetValues = z.output<typeof faucetSchema>;

export const mineSchema = z.object({
  blocks: z
    .string()
    .min(1, 'Enter a number of blocks.')
    .transform((value, ctx) => {
      const blocks = Number(value);
      if (!Number.isInteger(blocks)) {
        ctx.addIssue({ code: 'custom', message: 'Enter a whole number of blocks.' });
        return z.NEVER;
      }
      if (blocks < MINE_MIN_BLOCKS || blocks > MINE_MAX_BLOCKS) {
        ctx.addIssue({
          code: 'custom',
          message: `Enter between ${MINE_MIN_BLOCKS} and ${MINE_MAX_BLOCKS.toLocaleString()} blocks.`,
        });
        return z.NEVER;
      }
      return blocks;
    }),
});
export type MineInput = z.input<typeof mineSchema>;
export type MineValues = z.output<typeof mineSchema>;

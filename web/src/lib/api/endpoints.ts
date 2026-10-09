import { z } from 'zod';
import { post, request } from './client';
import {
  accountSchema,
  activitySchema,
  addressSchema,
  blockPageSchema,
  blockSchema,
  miningJobSchema,
  miningJobResponseSchema,
  paymentUriSchema,
  sendQuoteSchema,
  statusSchema,
  transactionSchema,
  type Pool,
  type SendQuoteInput,
} from './schemas';

export const api = {
  status: () => request('/status', statusSchema),

  accounts: () => request('/accounts', z.array(accountSchema)),

  activity: (limit = 30) => request(`/activity?limit=${limit}`, z.array(activitySchema)),

  blocks: (options?: { limit?: number; before?: number }) => {
    const params = new URLSearchParams();
    params.set('limit', String(options?.limit ?? 20));
    if (options?.before !== undefined) params.set('before', String(options.before));
    return request(`/blocks?${params.toString()}`, blockPageSchema);
  },

  block: (id: string | number) => request(`/blocks/${id}`, blockSchema),

  transaction: (txid: string) => request(`/transactions/${txid}`, transactionSchema),

  address: (address: string) => request(`/addresses/${address}`, addressSchema),

  /** Resolves a 64-hex string the client cannot tell apart on shape alone. */
  search: (query: string) =>
    request(
      `/search?q=${encodeURIComponent(query)}`,
      z.looseObject({ type: z.enum(['block', 'transaction']) }),
    ),

  /** Exactly one of `to_account` or `to_address` is set. */
  send: (input: {
    from_account: number;
    to_account?: number;
    to_address?: string;
    source_pool: Pool;
    destination_pool: Pool;
    amount_zatoshi: bigint;
    memo?: string;
    idempotency_key: string;
  }) =>
    post('/send', activitySchema, {
      ...input,
      amount_zatoshi: Number(input.amount_zatoshi),
    }),

  /** Dry-run proposal: exact fee and max spendable; nothing is broadcast. */
  sendQuote: (input: SendQuoteInput) => post('/send/quote', sendQuoteSchema, input),

  parsePaymentUri: (uri: string) => post('/zip321/parse', paymentUriSchema, { uri }),

  faucet: (input: {
    account_id: number;
    pool: Pool;
    amount_zatoshi: bigint;
    idempotency_key: string;
  }) =>
    post('/faucet', activitySchema, {
      ...input,
      amount_zatoshi: Number(input.amount_zatoshi),
    }),

  startMining: (blocks: number, idempotency_key: string) =>
    post('/mining/jobs', miningJobSchema, { blocks, idempotency_key }),

  miningJob: () => request('/mining/jobs', miningJobResponseSchema),
};

import { z } from 'zod';
import { post, request } from './client';
import {
  accountSchema,
  activitySchema,
  addressSchema,
  blockPageSchema,
  blockSchema,
  mempoolSchema,
  seedSchema,
  statusSchema,
  transactionSchema,
  type Pool,
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

  mempool: () => request('/mempool', mempoolSchema),

  address: (address: string) => request(`/addresses/${address}`, addressSchema),

  /** Resolves a 64-hex string the client cannot tell apart on shape alone. */
  search: (query: string) =>
    request(
      `/search?q=${encodeURIComponent(query)}`,
      z.looseObject({ type: z.enum(['block', 'transaction']) }),
    ),

  send: (input: {
    from_account: number;
    to_account: number;
    source_pool: Pool;
    destination_pool: Pool;
    amount_zatoshi: bigint;
    idempotency_key: string;
    memo?: string;
  }) =>
    post('/send', activitySchema, {
      ...input,
      amount_zatoshi: Number(input.amount_zatoshi),
    }),

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

  mine: (blocks: number) =>
    post('/mine', z.object({ blocks: z.number(), hashes: z.array(z.string()) }), { blocks }),

  seed: () =>
    post('/dev/seed', seedSchema, {
      confirmation: 'I understand this seed is for regtest only',
    }),
};

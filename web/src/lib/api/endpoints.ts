import { z } from 'zod';
import { post, request, requestWithResponse } from './client';
import { ApiError } from './errors';
import {
  accountSchema,
  activitySchema,
  addressFaucetSchema,
  addressSchema,
  blockPageSchema,
  blockSchema,
  miningJobSchema,
  miningJobResponseSchema,
  paymentRequestSchema,
  paymentUriSchema,
  sendQuoteSchema,
  statusSchema,
  transactionSchema,
  type Pool,
  type SendQuoteInput,
  type AddressFaucet,
  type Activity,
} from './schemas';

export type ActivityListOptions = {
  limit?: number;
  account?: number;
  kind?: 'send' | 'faucet' | 'transaction';
  status?: string;
  before?: string;
};

function activityPath(options: number | ActivityListOptions = {}): string {
  const page = typeof options === 'number' ? { limit: options } : options;
  const params = new URLSearchParams();
  params.set('limit', String(page.limit ?? 30));
  if (page.account !== undefined) params.set('account', String(page.account));
  if (page.kind !== undefined) params.set('kind', page.kind);
  if (page.status !== undefined) params.set('status', page.status);
  if (page.before !== undefined) params.set('before', page.before);
  return `/activity?${params.toString()}`;
}

/** Adapt the address faucet's smaller status response to the shared payment UI. */
function addressFaucetActivity(payment: AddressFaucet, key: string): Activity {
  return {
    id: key,
    kind: 'faucet',
    from_account: null,
    to_account: null,
    to_address: payment.address,
    source_pool: 'ironwood',
    destination_pool: payment.address.startsWith('t') ? 'transparent' : 'ironwood',
    amount_zatoshi: payment.amount_zatoshi,
    txid: payment.txid,
    block_hash: payment.block_hash,
    status: payment.status,
    last_error: payment.last_error,
    created_at: '',
  };
}

export const api = {
  status: () => request('/status', statusSchema),

  accounts: () => request('/accounts', z.array(accountSchema)),

  activity: (options: number | ActivityListOptions = {}) =>
    request(activityPath(options), z.array(activitySchema)),

  activityPage: async (options: ActivityListOptions = {}) => {
    const { data, response } = await requestWithResponse(
      activityPath(options),
      z.array(activitySchema),
    );
    const header = response.headers.get('x-ths-has-more');
    let hasMore = header === 'true';
    // A running environment may predate the response header while Vite serves
    // the new dashboard. Probe one older row so UI-only iteration remains
    // accurate without forcing the developer to discard that environment.
    if (header === null && data.length === (options.limit ?? 30)) {
      const cursor = data.at(-1)?.id;
      if (cursor) {
        const older = await request(
          activityPath({ ...options, limit: 1, before: cursor }),
          z.array(activitySchema),
        );
        hasMore = older.length > 0;
      }
    }
    return {
      entries: data,
      hasMore,
    };
  },

  payment: async (key: string) => {
    const rows = await request(
      `/activity?idempotency_key=${encodeURIComponent(key)}`,
      z.array(activitySchema).max(1),
    );
    return rows[0] ?? null;
  },

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

  paymentRequest: (input: { account_id: number; amount_zatoshi?: bigint; memo?: string }) =>
    post('/zip321/request', paymentRequestSchema, {
      ...input,
      ...(input.amount_zatoshi === undefined
        ? {}
        : { amount_zatoshi: Number(input.amount_zatoshi) }),
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

  faucetAddress: async (input: {
    address: string;
    amount_zatoshi: bigint;
    idempotency_key: string;
  }) =>
    addressFaucetActivity(
      await post('/faucet/address', addressFaucetSchema, {
        ...input,
        amount_zatoshi: Number(input.amount_zatoshi),
      }),
      input.idempotency_key,
    ),

  faucetAddressStatus: async (key: string) => {
    try {
      const result = await request(
        `/faucet/address?idempotency_key=${encodeURIComponent(key)}`,
        addressFaucetSchema,
      );
      return addressFaucetActivity(result, key);
    } catch (error) {
      if (error instanceof ApiError && error.status === 404) return null;
      throw error;
    }
  },

  startMining: (blocks: number, idempotency_key: string) =>
    post('/mining/jobs', miningJobSchema, { blocks, idempotency_key }),

  miningJob: () => request('/mining/jobs', miningJobResponseSchema),
};

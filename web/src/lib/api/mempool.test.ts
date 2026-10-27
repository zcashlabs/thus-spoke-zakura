import { afterEach, describe, expect, it, vi } from 'vitest';
import { api } from './endpoints';

const TXID = 'ab'.repeat(32);
afterEach(() => vi.restoreAllMocks());

describe('mempool API', () => {
  it('reads pending transaction IDs without submitting an operation', async () => {
    const fetch = vi
      .spyOn(globalThis, 'fetch')
      .mockResolvedValue(new Response(JSON.stringify({ transactions: [TXID] })));
    expect(await api.mempool()).toEqual({ transactions: [TXID] });
    expect(fetch.mock.calls[0]?.[0]).toBe('/api/v1/mempool');
    expect(fetch.mock.calls[0]?.[1]?.method).toBeUndefined();
  });

  it.each([{}, { transactions: null }, { transactions: ['not-a-txid'] }])(
    'rejects malformed data instead of treating it as empty: %j',
    async (body) => {
      vi.spyOn(globalThis, 'fetch').mockResolvedValue(new Response(JSON.stringify(body)));
      await expect(api.mempool()).rejects.toThrow('Unexpected response shape');
    },
  );
});

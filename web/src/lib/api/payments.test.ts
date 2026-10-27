import { afterEach, describe, expect, it, vi } from 'vitest';
import { api } from './endpoints';

const paymentResponse = {
  id: 'operation',
  kind: 'send',
  from_account: 1,
  to_account: 2,
  source_pool: 'ironwood',
  destination_pool: 'ironwood',
  amount_zatoshi: 100_000_000,
  txid: '',
  block_hash: null,
  status: 'failed',
  last_error: 'stored reason',
  created_at: '2026-10-10 00:00:00',
};

afterEach(() => vi.restoreAllMocks());
describe('exact payment status API', () => {
  it('uses an encoded key for GET and accepts only zero or one stored outcome', async () => {
    const fetch = vi
      .spyOn(globalThis, 'fetch')
      .mockResolvedValueOnce(new Response(JSON.stringify([paymentResponse])))
      .mockResolvedValueOnce(new Response('[]'))
      .mockResolvedValueOnce(new Response(JSON.stringify([paymentResponse, paymentResponse])));
    expect(await api.payment('original/key')).toMatchObject({
      status: 'failed',
      txid: '',
      amount_zatoshi: 100_000_000n,
      last_error: 'stored reason',
    });
    expect(fetch.mock.calls[0]?.[0]).toBe('/api/v1/activity?idempotency_key=original%2Fkey');
    expect(fetch.mock.calls[0]?.[1]?.method).toBeUndefined();
    expect(await api.payment('original/key')).toBeNull();
    await expect(api.payment('original/key')).rejects.toThrow('Unexpected response shape');
  });

  it('reads address faucet status from its own endpoint and treats 404 as no record', async () => {
    const fetch = vi
      .spyOn(globalThis, 'fetch')
      .mockResolvedValueOnce(
        new Response(
          JSON.stringify({
            address: 'tmAg2wTARgKBHA9mFouqR727cU98MJmfjSq',
            amount_zatoshi: 100_000_000,
            txid: '',
            block_hash: null,
            status: 'broadcast',
            last_error: null,
          }),
        ),
      )
      .mockResolvedValueOnce(
        new Response(JSON.stringify({ error: { message: 'not found', status: 404 } }), {
          status: 404,
        }),
      );
    expect(await api.faucetAddressStatus('address/key')).toMatchObject({
      status: 'broadcast',
      to_account: null,
      amount_zatoshi: 100_000_000n,
    });
    expect(fetch.mock.calls[0]?.[0]).toBe('/api/v1/faucet/address?idempotency_key=address%2Fkey');
    expect(await api.faucetAddressStatus('address/key')).toBeNull();
  });
});

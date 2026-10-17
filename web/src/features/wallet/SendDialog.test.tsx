import { describe, expect, it, vi, beforeEach, afterEach } from 'vitest';
import { useState } from 'react';
import { act, fireEvent, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import {
  renderWithWalletActions as renderWithProviders,
  requestUrl,
  testAccounts,
} from '@/test/utils';
import { SendDialog } from './SendDialog';

/**
 * SendDialog moves real funds, so the behaviour worth pinning is what reaches
 * the network: the amount must arrive as an exact integer number of zatoshi,
 * and invalid input must never be submitted at all.
 */
interface SendBody {
  from_account: number;
  to_account?: number;
  to_address?: string;
  source_pool: 'ironwood' | 'transparent';
  destination_pool: 'ironwood' | 'transparent';
  amount_zatoshi: number;
  idempotency_key: string;
  memo?: string;
}

/** Reads the JSON body of a fetch call without leaning on `any`. */
function requestBody(init: RequestInit | undefined): SendBody {
  const raw = typeof init?.body === 'string' ? init.body : '{}';
  return JSON.parse(raw) as SendBody;
}

const QUOTE = {
  available_zatoshi: 500_000_000,
  fee_zatoshi: 10_000,
  max_zatoshi: 499_990_000,
};

const PENDING = {
  id: 'payment',
  kind: 'send',
  from_account: 1,
  to_account: 2,
  source_pool: 'ironwood',
  destination_pool: 'ironwood',
  amount_zatoshi: 100_000_000,
  txid: '',
  block_hash: null,
  status: 'preparing',
  last_error: null as string | null,
  created_at: '2026-10-10 00:00:00',
};

function sendImpl(quote: typeof QUOTE, payment?: typeof PENDING, parsed: unknown = null) {
  return (input: RequestInfo | URL, init?: RequestInit) => {
    if (requestUrl(input).endsWith('/zip321/parse')) {
      const raw = typeof init?.body === 'string' ? init.body : '{}';
      const uri = (JSON.parse(raw) as { uri: string }).uri;
      const address = uri.slice('zcash:'.length).split('?')[0] ?? '';
      return Promise.resolve(
        new Response(
          JSON.stringify(
            parsed ?? {
              address,
              destination_pool: address.startsWith('t') ? 'transparent' : 'ironwood',
              to_account: null,
              amount_zatoshi: null,
              memo: null,
            },
          ),
          {
            status: 200,
            headers: { 'content-type': 'application/json' },
          },
        ),
      );
    }
    if (requestUrl(input).includes('/activity?')) {
      return Promise.resolve(new Response(JSON.stringify(payment ? [payment] : [])));
    }
    if (requestUrl(input).endsWith('/send/quote')) {
      return Promise.resolve(
        new Response(JSON.stringify(quote), {
          status: 200,
          headers: { 'content-type': 'application/json' },
        }),
      );
    }
    return Promise.resolve(
      new Response(
        JSON.stringify(
          payment ?? {
            id: 'a',
            kind: 'send',
            from_account: requestBody(init).from_account,
            to_account: requestBody(init).to_account ?? null,
            to_address: requestBody(init).to_address ?? null,
            source_pool: requestBody(init).source_pool,
            destination_pool: requestBody(init).destination_pool,
            amount_zatoshi: requestBody(init).amount_zatoshi,
            txid: 'f'.repeat(64),
            block_hash: null,
            status: 'confirmed',
            created_at: '2026-09-18 10:00:00',
          },
        ),
        { status: 200, headers: { 'content-type': 'application/json' } },
      ),
    );
  };
}

function mockSend(quote = QUOTE, parsed: unknown = null) {
  return vi.spyOn(globalThis, 'fetch').mockImplementation(sendImpl(quote, undefined, parsed));
}

// Filters out the quote fetch the dialog makes on mount.
const sendCalls = (fetchMock: ReturnType<typeof mockSend>) =>
  fetchMock.mock.calls.filter(([input]) => requestUrl(input).endsWith('/send'));

const body = (fetchMock: ReturnType<typeof mockSend>): SendBody =>
  requestBody(sendCalls(fetchMock)[0]?.[1]);

async function selectTransferField(label: string, option: string) {
  Object.assign(Element.prototype, {
    hasPointerCapture: () => false,
    releasePointerCapture: () => undefined,
    scrollIntoView: () => undefined,
  });
  await userEvent.click(screen.getByLabelText(label));
  await userEvent.click(await screen.findByRole('option', { name: option }));
}

describe('SendDialog', () => {
  let fetchMock: ReturnType<typeof mockSend>;

  beforeEach(() => {
    sessionStorage.clear();
    localStorage.clear();
    fetchMock = mockSend();
  });
  afterEach(() => {
    vi.restoreAllMocks();
  });

  it('quotes and sends a transparent external address through the transparent pool', async () => {
    const address = 'tmAg2wTARgKBHA9mFouqR727cU98MJmfjSq';
    const transparentQuote = { ...QUOTE, fee_zatoshi: 20_000, max_zatoshi: 400_000_000 };
    fetchMock.mockImplementation((input, init) => {
      if (requestUrl(input).endsWith('/send/quote')) {
        const raw = typeof init?.body === 'string' ? init.body : '{}';
        const pool = (JSON.parse(raw) as { destination_pool: string }).destination_pool;
        return Promise.resolve(Response.json(pool === 'transparent' ? transparentQuote : QUOTE));
      }
      return sendImpl(QUOTE)(input, init);
    });
    renderWithProviders(<SendDialog open onOpenChange={vi.fn()} accounts={testAccounts} />);
    await selectTransferField('Destination', 'Other address');
    await userEvent.type(screen.getByLabelText('Destination address'), address);
    await waitFor(() =>
      expect(screen.getByText('Destination pool').parentElement).toHaveTextContent(
        'Transparent (public)',
      ),
    );
    await waitFor(() =>
      expect(
        screen.getByText(/4 ZEC spendable after a 0.0002 ZEC network fee/),
      ).toBeInTheDocument(),
    );
    await userEvent.click(screen.getByRole('button', { name: 'Max' }));
    expect(screen.getByLabelText('Amount (ZEC)')).toHaveValue('4');
    expect(screen.getByLabelText('Memo (optional)')).toBeDisabled();
    await userEvent.click(screen.getByRole('button', { name: /Send ZEC/i }));
    await waitFor(() => expect(sendCalls(fetchMock)).toHaveLength(1));
    expect(body(fetchMock)).toMatchObject({
      to_address: address,
      destination_pool: 'transparent',
      amount_zatoshi: 400_000_000,
    });
    expect(body(fetchMock)).not.toHaveProperty('memo');
  });

  it.each(['not-an-address', 't1wrongnetworkaddress'])(
    'rejects %s before sending',
    async (address) => {
      fetchMock.mockImplementation((input, init) =>
        requestUrl(input).endsWith('/zip321/parse')
          ? Promise.resolve(
              new Response(
                JSON.stringify({ error: { message: 'Not a valid Regtest address', status: 400 } }),
                { status: 400 },
              ),
            )
          : sendImpl(QUOTE)(input, init),
      );
      renderWithProviders(<SendDialog open onOpenChange={vi.fn()} accounts={testAccounts} />);
      await selectTransferField('Destination', 'Other address');
      await userEvent.type(screen.getByLabelText('Destination address'), address);
      await userEvent.click(screen.getByRole('button', { name: /Send ZEC/i }));
      expect(await screen.findByText('Not a valid Regtest address')).toBeInTheDocument();
      expect(sendCalls(fetchMock)).toHaveLength(0);
    },
  );

  async function submitAmount(amount: string) {
    renderWithProviders(<SendDialog open onOpenChange={vi.fn()} accounts={testAccounts} />);
    const field = screen.getByLabelText('Amount (ZEC)');
    await userEvent.clear(field);
    await userEvent.type(field, amount);
    await userEvent.click(screen.getByRole('button', { name: /Send ZEC/i }));
  }

  it.each([
    ['ironwood', 'transparent', 'Ironwood (shielded)', 'Transparent (public)'],
    ['transparent', 'ironwood', 'Transparent (public)', 'Ironwood (shielded)'],
  ] as const)(
    'submits same-account %s → %s',
    async (source, destination, sourceLabel, destLabel) => {
      const accounts = testAccounts.map((account) => ({
        ...account,
        transparent_zatoshi: account.id === 1 ? 500_000_000n : 0n,
      }));
      renderWithProviders(<SendDialog open onOpenChange={vi.fn()} accounts={accounts} />);
      await selectTransferField('Destination', 'Account 1');
      if (source === 'transparent') await selectTransferField('Source pool', sourceLabel);
      if (destination === 'transparent') await selectTransferField('Destination pool', destLabel);
      await userEvent.click(screen.getByRole('button', { name: /Send ZEC/i }));
      await waitFor(() => expect(sendCalls(fetchMock)).toHaveLength(1));
      expect(body(fetchMock)).toMatchObject({
        from_account: 1,
        to_account: 1,
        source_pool: source,
        destination_pool: destination,
        amount_zatoshi: 100_000_000,
      });
      expect(body(fetchMock).idempotency_key).toHaveLength(36);
    },
  );

  it('rejects the same account and pool before submission', async () => {
    renderWithProviders(<SendDialog open onOpenChange={vi.fn()} accounts={testAccounts} />);
    await selectTransferField('Destination', 'Account 1');
    await userEvent.click(screen.getByRole('button', { name: /Send ZEC/i }));
    expect(
      await screen.findByText('Choose a different account or a different destination pool.'),
    ).toBeInTheDocument();
    expect(sendCalls(fetchMock)).toHaveLength(0);
  });

  it('sends the amount as exact zatoshi', async () => {
    await submitAmount('1.5');
    await waitFor(() => expect(sendCalls(fetchMock)).toHaveLength(1));
    expect(body(fetchMock).amount_zatoshi).toBe(150_000_000);
  });

  it('parses a value that is not exactly representable as a float', async () => {
    // Number('2.675') * 1e8 is 267499999.99999997. Math.round would also
    // land on the right answer here, so this pins exactness rather than
    // catching the old implementation; see money.test.ts for the value that
    // genuinely separates the two.
    await submitAmount('2.675');
    await waitFor(() => expect(sendCalls(fetchMock)).toHaveLength(1));
    expect(body(fetchMock).amount_zatoshi).toBe(267_500_000);
  });

  it('sends a single zatoshi without rounding it away', async () => {
    await submitAmount('0.00000001');
    await waitFor(() => expect(sendCalls(fetchMock)).toHaveLength(1));
    expect(body(fetchMock).amount_zatoshi).toBe(1);
  });

  it('uses a new idempotency key after a successful payment', async () => {
    await submitAmount('1');
    await waitFor(() => expect(sendCalls(fetchMock)).toHaveLength(1));
    const first = body(fetchMock).idempotency_key;
    await waitFor(() => expect(screen.getByRole('button', { name: /Send ZEC/i })).toBeEnabled());
    await userEvent.click(screen.getByRole('button', { name: /Send ZEC/i }));
    await waitFor(() => expect(sendCalls(fetchMock)).toHaveLength(2));
    expect(first).toHaveLength(36);
    expect(first).not.toBe(requestBody(sendCalls(fetchMock)[1]?.[1]).idempotency_key);
  });

  it('restores a lost address payment and separates a different destination address', async () => {
    fetchMock.mockImplementation((input, init) =>
      requestUrl(input).endsWith('/send')
        ? Promise.reject(new TypeError('response lost'))
        : sendImpl(QUOTE)(input, init),
    );
    const renderForm = () =>
      renderWithProviders(<SendDialog open onOpenChange={vi.fn()} accounts={testAccounts} />);
    let view = renderForm();
    for (const [index, destination] of [
      'uregtest1first',
      'uregtest1first',
      'uregtest1second',
    ].entries()) {
      if (index < 2) await selectTransferField('Destination', 'Other address');
      const address = screen.getByLabelText('Destination address');
      await userEvent.clear(address);
      await userEvent.type(address, destination);
      await userEvent.click(screen.getByRole('button', { name: /Send ZEC|Resume payment/i }));
      await waitFor(() => expect(sendCalls(fetchMock)).toHaveLength(index + 1));
      expect(await screen.findByText(`1 ZEC to ${destination}. response lost`)).toBeInTheDocument();
      if (index === 0) {
        view.unmount();
        view = renderForm();
        await waitFor(() =>
          expect(screen.getByText('Payments').closest('button')).toHaveAccessibleName(
            /1 unresolved/,
          ),
        );
        expect(sendCalls(fetchMock)).toHaveLength(1);
      }
    }
    const keys = sendCalls(fetchMock).map(([, init]) => requestBody(init).idempotency_key);
    expect(keys[0]).toBe(keys[1]);
    expect(keys[2]).not.toBe(keys[1]);
  });

  it.each(['none', 'locks', 'storage'] as const)(
    'announces confirmation once and separates cleanup errors (cleanup failure: %s)',
    async (cleanupFailure) => {
      let posted = false;
      let restoreCleanup: (() => void) | undefined;
      const payment = { ...PENDING, status: 'confirmed', amount_zatoshi: 200_000_000 };
      fetchMock.mockImplementation((input, init) => {
        if (requestUrl(input).endsWith('/send')) {
          posted = true;
          if (cleanupFailure === 'locks') {
            const blocked = vi
              .spyOn(navigator.locks, 'request')
              .mockRejectedValue(new Error('Payment cleanup lock denied'));
            restoreCleanup = () => blocked.mockRestore();
          } else if (cleanupFailure === 'storage') {
            const blocked = vi.spyOn(Storage.prototype, 'removeItem').mockImplementation(() => {
              throw new DOMException('Browser storage is blocked', 'SecurityError');
            });
            restoreCleanup = () => blocked.mockRestore();
          }
        }
        return sendImpl(QUOTE, posted ? payment : undefined)(input, init);
      });
      const close = vi.fn();
      const first = renderWithProviders(
        <SendDialog open onOpenChange={close} accounts={testAccounts} />,
      );
      const field = screen.getByLabelText('Amount (ZEC)');
      await userEvent.clear(field);
      await userEvent.type(field, '2');
      await userEvent.click(screen.getByRole('button', { name: 'Send ZEC' }));
      expect(await screen.findAllByText('Sent to Account 2')).toHaveLength(1);
      await waitFor(() =>
        expect(screen.getByText('Payments').closest('button')).toHaveAttribute(
          'aria-label',
          expect.stringMatching(/1 confirmed/),
        ),
      );
      if (cleanupFailure === 'none') {
        await waitFor(() => expect(close).toHaveBeenCalledWith(false));
        expect(screen.getByLabelText('Amount (ZEC)')).toHaveValue('1');
      } else {
        expect(await screen.findByText(/Previous payment finished/)).toBeInTheDocument();
        await waitFor(() =>
          expect(screen.getByRole('button', { name: 'Send ZEC' })).toBeDisabled(),
        );
        expect(screen.queryByText('Payment not confirmed')).not.toBeInTheDocument();
        expect(close).not.toHaveBeenCalled();
        expect(screen.getByLabelText('Amount (ZEC)')).toHaveValue('2');
        const record = `ths:send:retained:${body(fetchMock).idempotency_key}`;
        expect(localStorage.getItem(record)).toBeTruthy();
        restoreCleanup!();
        first.unmount();
        renderWithProviders(<SendDialog open onOpenChange={close} accounts={testAccounts} />);
        await waitFor(() =>
          expect(screen.getByText('Payments').closest('button')).toHaveAccessibleName(
            /1 confirmed/,
          ),
        );
        await waitFor(() => expect(localStorage.getItem(record)).toBeNull());
        expect(sendCalls(fetchMock)).toHaveLength(1);
      }
      expect(screen.queryAllByText('Sent to Account 2')).toHaveLength(
        cleanupFailure === 'none' ? 1 : 0,
      );
    },
  );

  it.each(['preparing', 'confirmed'])(
    'shows submission without Resume and keeps edits when its response is %s',
    async (status) => {
      let payment: typeof PENDING | undefined;
      let confirm!: (response: Response) => void;
      const response = new Promise<Response>((resolve) => {
        confirm = resolve;
      });
      fetchMock.mockImplementation((input, init) =>
        requestUrl(input).endsWith('/send') ? response : sendImpl(QUOTE, payment)(input, init),
      );
      const close = vi.fn();
      renderWithProviders(<SendDialog open onOpenChange={close} accounts={testAccounts} />);
      await userEvent.click(screen.getByRole('button', { name: 'Send ZEC' }));
      expect(await screen.findAllByText('Submitting payment…')).toHaveLength(1);
      expect(screen.queryByText(/Payment outcome unknown/)).not.toBeInTheDocument();
      expect(
        screen.queryByRole('button', { name: 'Resume payment', hidden: true }),
      ).not.toBeInTheDocument();
      await userEvent.clear(screen.getByLabelText('Amount (ZEC)'));
      await userEvent.type(screen.getByLabelText('Amount (ZEC)'), '2');
      await userEvent.type(screen.getByLabelText('Memo (optional)'), 'next payment draft');
      await act(async () => {
        payment = { ...PENDING, status };
        confirm(await sendImpl(QUOTE, payment)('/api/v1/send', sendCalls(fetchMock)[0]?.[1]));
      });
      await waitFor(() =>
        expect(screen.getByText('Payments').closest('button')).toHaveAccessibleName(
          status === 'confirmed' ? /1 confirmed/ : /1 unresolved/,
        ),
      );
      expect(await screen.findByText(/Your draft is unchanged/)).toBeInTheDocument();
      expect(close).not.toHaveBeenCalled();
      expect(screen.getByLabelText('Amount (ZEC)')).toHaveValue('2');
      expect(screen.getByLabelText('Memo (optional)')).toHaveValue('next payment draft');
      expect(screen.queryAllByText('Sent to Account 2')).toHaveLength(
        status === 'confirmed' ? 1 : 0,
      );
    },
  );

  it.each(['absent', 'retained', 'unavailable'] as const)(
    'keeps a rejected request until the user acts (lookup: %s)',
    async (lookup) => {
      fetchMock.mockImplementation((input, init) => {
        if (requestUrl(input).endsWith('/send')) {
          return Promise.resolve(
            new Response(
              JSON.stringify({ error: { message: 'Insufficient funds', status: 422 } }),
              {
                status: 422,
              },
            ),
          );
        }
        if (requestUrl(input).includes('/activity?') && lookup === 'unavailable') {
          return Promise.reject(new TypeError('status offline'));
        }
        return sendImpl(QUOTE, lookup === 'retained' ? PENDING : undefined)(input, init);
      });
      renderWithProviders(<SendDialog open onOpenChange={vi.fn()} accounts={testAccounts} />);
      await userEvent.click(screen.getByRole('button', { name: 'Send ZEC' }));
      expect(await screen.findByText('Payment rejected')).toBeInTheDocument();
      const first = body(fetchMock).idempotency_key;
      const name = 'ths:send:[1,2,"ironwood","ironwood","100000000",null]';
      expect(localStorage.getItem(name)).toBe(first);
      expect(screen.getByRole('button', { name: 'View payment' })).toBeInTheDocument();
      fetchMock.mockImplementation(sendImpl(QUOTE));
      await userEvent.click(screen.getByRole('button', { name: 'Resume payment' }));
      await waitFor(() => expect(sendCalls(fetchMock)).toHaveLength(2));
      const next = requestBody(sendCalls(fetchMock)[1]?.[1]).idempotency_key;
      expect(next).toBe(first);
    },
  );

  it('keeps a conflicting payment retained for an explicit retry', async () => {
    Object.assign(Element.prototype, { scrollIntoView: vi.fn() });
    fetchMock.mockImplementation((input, init) =>
      requestUrl(input).endsWith('/send')
        ? Promise.resolve(
            new Response(JSON.stringify({ error: { message: 'Key conflict', status: 409 } }), {
              status: 409,
            }),
          )
        : sendImpl(QUOTE)(input, init),
    );
    renderWithProviders(<SendDialog open onOpenChange={vi.fn()} accounts={testAccounts} />);
    await userEvent.click(screen.getByRole('button', { name: 'Send ZEC' }));
    expect(await screen.findByText('Payment not confirmed')).toBeInTheDocument();
    expect(localStorage.getItem('ths:send:[1,2,"ironwood","ironwood","100000000",null]')).toBe(
      body(fetchMock).idempotency_key,
    );
    await waitFor(() =>
      expect(screen.getByText('Payments').closest('button')).toHaveAttribute(
        'aria-label',
        expect.stringMatching(/1 unresolved/),
      ),
    );
    expect(screen.getByRole('button', { name: 'Resume payment' })).toBeInTheDocument();
    await userEvent.type(screen.getByLabelText('Memo (optional)'), 'next draft');
    const form = screen.getByRole('dialog', { name: 'Send ZEC' });
    const opener = within(form).getByRole('button', { name: 'View payment' });
    await userEvent.click(opener);
    const panel = screen.getByRole('dialog', { name: 'Payments' });
    await waitFor(() => expect(within(panel).getByRole('heading', { level: 3 })).toHaveFocus());
    expect(sendCalls(fetchMock)).toHaveLength(1);
    await userEvent.click(within(panel).getByRole('button', { name: 'Close dialog' }));
    expect(screen.getByLabelText('Memo (optional)')).toHaveValue('next draft');
    await waitFor(() => expect(opener).toHaveFocus());
  });

  it.each([
    [false, false],
    [false, true],
    [true, false],
    [true, true],
  ])(
    'keeps a lost-response key across remounts (same account: %s, durable only: %s)',
    async (sameAccount, durableOnly) => {
      let lostSends = 2;
      fetchMock.mockImplementation((input, init) => {
        if (requestUrl(input).endsWith('/send') && lostSends-- > 0) {
          return Promise.reject(new TypeError('response lost'));
        }
        return sendImpl(QUOTE)(input, init);
      });
      const accounts = testAccounts.map((account) => ({
        ...account,
        transparent_zatoshi: account.id === 1 ? 500_000_000n : 0n,
      }));
      async function chooseRoute() {
        if (sameAccount) {
          await selectTransferField('Destination', 'Account 1');
          await selectTransferField('Source pool', 'Transparent (public)');
        }
      }
      const first = renderWithProviders(
        <SendDialog open onOpenChange={vi.fn()} accounts={accounts} />,
      );
      await chooseRoute();
      await userEvent.type(screen.getByLabelText('Memo (optional)'), 'rent');
      await userEvent.click(screen.getByRole('button', { name: /Send ZEC/i }));
      await waitFor(() => expect(sendCalls(fetchMock)).toHaveLength(1));
      first.unmount();
      sessionStorage.clear();
      if (durableOnly) {
        const record = `ths:send:retained:${body(fetchMock).idempotency_key}`;
        const name = localStorage.getItem(record)!;
        expect(name).toBeTruthy();
        localStorage.removeItem(name);
      }
      renderWithProviders(<SendDialog open onOpenChange={vi.fn()} accounts={accounts} />);
      await chooseRoute();
      const memo = screen.getByLabelText('Memo (optional)');
      await userEvent.type(memo, 'rent');
      await userEvent.click(screen.getByRole('button', { name: /^Resume payment$/ }));
      await waitFor(() => expect(sendCalls(fetchMock)).toHaveLength(2));
      await userEvent.clear(memo);
      await userEvent.type(memo, 'gift');
      await userEvent.click(screen.getByRole('button', { name: /Send ZEC/i }));
      await waitFor(() => expect(sendCalls(fetchMock)).toHaveLength(3));
      const keys = sendCalls(fetchMock).map(([, init]) => requestBody(init).idempotency_key);
      expect(keys[0]).toBe(keys[1]);
      expect(keys[2]).not.toBe(keys[1]);
    },
  );

  it('retains acceptance across restart, replays spent funds, and confirms through GET', async () => {
    let payment = PENDING;
    fetchMock.mockImplementation((input, init) => sendImpl(QUOTE, payment)(input, init));
    const close = vi.fn();
    const first = renderWithProviders(
      <SendDialog open onOpenChange={close} accounts={testAccounts} />,
    );
    await userEvent.click(screen.getByRole('button', { name: 'Send ZEC' }));
    await waitFor(() =>
      expect(screen.getByText('Payments').closest('button')).toHaveAttribute(
        'aria-label',
        expect.stringMatching(/1 unresolved/),
      ),
    );
    expect(close).toHaveBeenCalledWith(false);
    close.mockClear();
    const originalKey = body(fetchMock).idempotency_key;
    first.unmount();
    sessionStorage.clear();
    payment = { ...PENDING, status: 'prepared', txid: 'f'.repeat(64) };
    const second = renderWithProviders(
      <SendDialog
        open
        onOpenChange={close}
        accounts={testAccounts.map((account) => ({ ...account, ironwood_zatoshi: 0n }))}
      />,
    );
    expect(
      await screen.findByText(/saved payment has these details \(prepared\)/),
    ).toBeInTheDocument();
    expect(fetchMock.mock.calls.map(([input]) => requestUrl(input))).toContain(
      `/api/v1/activity?idempotency_key=${originalKey}`,
    );
    expect(sendCalls(fetchMock)).toHaveLength(1);
    await userEvent.click(screen.getByRole('button', { name: 'Resume payment' }));
    await waitFor(() => expect(sendCalls(fetchMock)).toHaveLength(2));
    expect(requestBody(sendCalls(fetchMock)[1]?.[1]).idempotency_key).toBe(originalKey);
    expect(requestBody(sendCalls(fetchMock)[1]?.[1])).toMatchObject({
      amount_zatoshi: 100_000_000,
    });
    expect(requestBody(sendCalls(fetchMock)[1]?.[1]).memo).toBeUndefined();
    close.mockClear();
    await userEvent.clear(screen.getByLabelText('Amount (ZEC)'));
    await userEvent.type(screen.getByLabelText('Amount (ZEC)'), '2');
    await userEvent.type(screen.getByLabelText('Memo (optional)'), 'next payment draft');
    payment = { ...PENDING, status: 'confirmed' };
    await waitFor(
      () =>
        expect(screen.getByText('Payments').closest('button')).toHaveAttribute(
          'aria-label',
          expect.stringMatching(/1 confirmed/),
        ),
      { timeout: 3_000 },
    );
    expect(close).not.toHaveBeenCalled();
    expect(screen.getByLabelText('Amount (ZEC)')).toHaveValue('2');
    expect(screen.getByLabelText('Memo (optional)')).toHaveValue('next payment draft');
    await waitFor(() =>
      expect(
        localStorage.getItem('ths:send:[1,2,"ironwood","ironwood","100000000",null]'),
      ).toBeNull(),
    );
    second.unmount();
    renderWithProviders(<SendDialog open onOpenChange={close} accounts={testAccounts} />);
    expect(screen.getByRole('dialog')).toBeInTheDocument();
    expect(close).not.toHaveBeenCalled();
    expect(sendCalls(fetchMock)).toHaveLength(2);
  });

  it('keeps read errors distinct from execution failure and requires a deliberate new payment', async () => {
    Object.assign(Element.prototype, {
      scrollIntoView: vi.fn(),
      hasPointerCapture: () => false,
      releasePointerCapture: () => undefined,
    });
    let payment: typeof PENDING = {
      ...PENDING,
      status: 'broadcast',
      last_error: 'lightwalletd unavailable',
    };
    let offline = true;
    fetchMock.mockImplementation((input, init) =>
      offline && requestUrl(input).includes('/activity?')
        ? Promise.reject(new TypeError('status offline'))
        : sendImpl(QUOTE, payment)(input, init),
    );
    function Sender() {
      const [open, setOpen] = useState(true);
      return <SendDialog open={open} onOpenChange={setOpen} accounts={testAccounts} />;
    }
    renderWithProviders(<Sender />);
    await userEvent.click(screen.getByRole('button', { name: 'Send ZEC' }));
    await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument());
    await userEvent.click(screen.getByRole('button', { name: /View payments/ }));
    expect(
      await screen.findByText(/Latest execution error: lightwalletd unavailable/),
    ).toBeInTheDocument();
    expect(await screen.findByText(/Couldn’t check status: status offline/)).toBeInTheDocument();
    expect(screen.queryByText(/Payment failed\./)).not.toBeInTheDocument();
    const originalKey = body(fetchMock).idempotency_key;
    offline = false;
    payment = { ...PENDING, status: 'failed', last_error: 'Insufficient funds' };
    await userEvent.click(screen.getByRole('button', { name: 'Resume' }));
    expect(await screen.findByText('Insufficient funds')).toBeInTheDocument();
    expect(screen.getByText('1 ZEC to Account 2. Insufficient funds')).toBeInTheDocument();
    await userEvent.click(screen.getByRole('button', { name: 'Unresolved 0' }));
    expect(
      within(screen.getByRole('region', { name: 'Payment updates' })).getByText(
        'No unresolved payments.',
      ),
    ).toBeInTheDocument();
    await userEvent.click(screen.getByRole('button', { name: 'Close dialog' }));
    await userEvent.click(screen.getByRole('button', { name: 'View payment' }));
    const updates = screen.getByRole('region', { name: 'Payment updates' });
    await waitFor(() => expect(within(updates).getByRole('heading')).toHaveFocus());
    expect(within(updates).getByText('Failed')).toBeInTheDocument();
    expect(sendCalls(fetchMock)).toHaveLength(2);
    expect(requestBody(sendCalls(fetchMock)[1]?.[1]).idempotency_key).toBe(originalKey);
    await userEvent.click(screen.getByRole('button', { name: 'New Send' }));
    await waitFor(() => expect(screen.getByRole('button', { name: 'Send ZEC' })).toBeEnabled());
    payment = PENDING;
    await userEvent.click(screen.getByRole('button', { name: 'Send ZEC' }));
    await waitFor(() => expect(sendCalls(fetchMock)).toHaveLength(3));
    expect(requestBody(sendCalls(fetchMock)[2]?.[1]).idempotency_key).not.toBe(originalKey);
  });

  it('migrates non-default session details and resumes an empty-memo request from the form', async () => {
    const name = 'ths:send:[3,4,"transparent","ironwood","267500001",""]';
    sessionStorage.setItem(name, 'original-legacy-key');
    let rejectResume = true;
    const recovered = sendImpl(QUOTE, {
      ...PENDING,
      from_account: 3,
      to_account: 4,
      source_pool: 'transparent',
      amount_zatoshi: 267_500_001,
    });
    fetchMock.mockImplementation((input, init) =>
      requestUrl(input).endsWith('/send') && rejectResume
        ? Promise.reject(new TypeError('Resume connection lost'))
        : recovered(input, init),
    );
    const first = renderWithProviders(<></>);
    await waitFor(() =>
      expect(screen.getByText('Payments').closest('button')).toHaveAttribute(
        'aria-label',
        expect.stringMatching(/1 unresolved/),
      ),
    );
    expect(sendCalls(fetchMock)).toHaveLength(0);
    expect(localStorage.getItem('ths:send:retained:original-legacy-key')).toBe(name);
    first.unmount();
    sessionStorage.clear();
    localStorage.removeItem(name);
    const close = vi.fn();
    renderWithProviders(
      <SendDialog open onOpenChange={close} accounts={testAccounts} defaultAccountId={3} />,
    );
    await selectTransferField('Destination', 'Account 4');
    await selectTransferField('Source pool', 'Transparent (public)');
    await userEvent.clear(screen.getByLabelText('Amount (ZEC)'));
    await userEvent.type(screen.getByLabelText('Amount (ZEC)'), '2.67500001');
    expect(screen.getByLabelText('Memo (optional)')).toHaveValue('');
    expect(sendCalls(fetchMock)).toHaveLength(0);
    await userEvent.click(await screen.findByRole('button', { name: 'Resume payment' }));
    await waitFor(() => expect(sendCalls(fetchMock)).toHaveLength(1));
    expect(await screen.findByText('Payment not confirmed')).toBeInTheDocument();
    expect(await screen.findByText('Resume connection lost')).toBeInTheDocument();
    expect(
      screen.getByText('2.67500001 ZEC to Account 4. Resume connection lost'),
    ).toBeInTheDocument();
    expect(localStorage.getItem('ths:send:retained:original-legacy-key')).toBe(name);
    rejectResume = false;
    await userEvent.click(await screen.findByRole('button', { name: 'Resume payment' }));
    await waitFor(() => expect(sendCalls(fetchMock)).toHaveLength(2));
    expect(requestBody(sendCalls(fetchMock)[1]?.[1])).toEqual(body(fetchMock));
    expect(body(fetchMock)).toEqual({
      from_account: 3,
      to_account: 4,
      source_pool: 'transparent',
      destination_pool: 'ironwood',
      amount_zatoshi: 267_500_001,
      memo: '',
      idempotency_key: 'original-legacy-key',
    });
    await waitFor(() => expect(close).toHaveBeenCalledWith(false));
  });

  it('posts the memo with the send', async () => {
    renderWithProviders(<SendDialog open onOpenChange={vi.fn()} accounts={testAccounts} />);
    await userEvent.type(screen.getByLabelText('Memo (optional)'), 'rent for October');
    await userEvent.click(screen.getByRole('button', { name: /Send ZEC/i }));
    await waitFor(() => expect(fetchMock).toHaveBeenCalled());
    expect(body(fetchMock).memo).toBe('rent for October');
  });

  it('omits the memo when none is entered', async () => {
    await submitAmount('1');
    await waitFor(() => expect(fetchMock).toHaveBeenCalled());
    expect(body(fetchMock)).not.toHaveProperty('memo');
  });

  it('refuses a memo over 512 bytes without calling the API', async () => {
    renderWithProviders(<SendDialog open onOpenChange={vi.fn()} accounts={testAccounts} />);
    const field = screen.getByLabelText('Memo (optional)');
    await userEvent.click(field);
    await userEvent.paste('a'.repeat(513));
    await userEvent.click(screen.getByRole('button', { name: /Send ZEC/i }));
    expect(await screen.findByRole('alert')).toHaveTextContent('512 bytes');
    expect(sendCalls(fetchMock)).toHaveLength(0);
  });

  it('disables and clears the memo when the destination is transparent', async () => {
    // Radix Select relies on pointer-capture and scrolling APIs jsdom lacks.
    Object.assign(Element.prototype, {
      hasPointerCapture: () => false,
      releasePointerCapture: () => undefined,
      scrollIntoView: () => undefined,
    });

    renderWithProviders(<SendDialog open onOpenChange={vi.fn()} accounts={testAccounts} />);
    const memo = screen.getByLabelText('Memo (optional)');
    expect(memo).toBeEnabled();
    await userEvent.type(memo, 'ironwood only');

    await userEvent.click(screen.getByLabelText('Destination pool'));
    await userEvent.click(await screen.findByRole('option', { name: 'Transparent (public)' }));

    expect(memo).toBeDisabled();
    expect(memo).toHaveValue('');
    expect(screen.getByText(/only available for ironwood destinations/i)).toBeInTheDocument();

    await userEvent.click(screen.getByRole('button', { name: /Send ZEC/i }));
    await waitFor(() => expect(fetchMock).toHaveBeenCalled());
    expect(body(fetchMock)).not.toHaveProperty('memo');
  });

  it('refuses a zero amount without calling the API', async () => {
    await submitAmount('0');
    expect(await screen.findByRole('alert')).toHaveTextContent('greater than zero');
    expect(sendCalls(fetchMock)).toHaveLength(0);
  });

  it('refuses more than eight decimal places without calling the API', async () => {
    await submitAmount('0.000000001');
    expect(await screen.findByRole('alert')).toHaveTextContent('8 decimal places');
    expect(sendCalls(fetchMock)).toHaveLength(0);
  });

  it('names the accounts and pools being moved between', () => {
    renderWithProviders(<SendDialog open onOpenChange={vi.fn()} accounts={testAccounts} />);
    expect(screen.getByLabelText('From account')).toBeInTheDocument();
    expect(screen.getByLabelText('Destination')).toBeInTheDocument();
    expect(screen.getByLabelText('Source pool')).toBeInTheDocument();
    expect(screen.getByLabelText('Destination pool')).toBeInTheDocument();
  });

  it('defaults the sender to the account the send action was opened from', () => {
    renderWithProviders(
      <SendDialog open onOpenChange={vi.fn()} accounts={testAccounts} defaultAccountId={3} />,
    );
    expect(screen.getByLabelText('From account')).toHaveTextContent('Account 3');
  });

  it('defaults the destination to an account other than the source', () => {
    renderWithProviders(
      <SendDialog open onOpenChange={vi.fn()} accounts={testAccounts} defaultAccountId={2} />,
    );

    // Opening Send from Account 2 used to preselect Account 2 on both sides,
    // which is a self-send that costs a fee and moves nothing.
    expect(screen.getByLabelText('From account')).toHaveTextContent('Account 2');
    expect(screen.getByLabelText('Destination')).not.toHaveTextContent('Account 2');
  });

  it('refuses an amount the source account cannot cover', async () => {
    // Account 2 holds nothing, so any amount is unaffordable.
    renderWithProviders(
      <SendDialog open onOpenChange={vi.fn()} accounts={testAccounts} defaultAccountId={2} />,
    );
    const field = screen.getByLabelText('Amount (ZEC)');
    await userEvent.clear(field);
    await userEvent.type(field, '1');
    await userEvent.click(screen.getByRole('button', { name: /Send ZEC/i }));

    expect(await screen.findByText(/holds 0 ZEC in the ironwood pool/i)).toBeInTheDocument();
    expect(sendCalls(fetchMock)).toHaveLength(0);
  });

  it('refuses to spend more than the quoted maximum, naming fee and cap', async () => {
    await submitAmount('5');

    expect(await screen.findByText(/0.0001 ZEC network fee/i)).toBeInTheDocument();
    expect(await screen.findByText(/at most 4.9999 ZEC spendable/i)).toBeInTheDocument();
    expect(sendCalls(fetchMock)).toHaveLength(0);
  });

  it('accepts a send that fits the quoted maximum', async () => {
    await submitAmount('4.9999');

    await waitFor(() => expect(sendCalls(fetchMock)).toHaveLength(1));
    expect(body(fetchMock).amount_zatoshi).toBe(499_990_000);
  });

  it('fills the amount with the quoted maximum when Max is clicked', async () => {
    renderWithProviders(<SendDialog open onOpenChange={vi.fn()} accounts={testAccounts} />);

    const max = await screen.findByRole('button', { name: 'Max' });
    await waitFor(() => expect(max).toBeEnabled());
    await userEvent.click(max);

    expect(screen.getByLabelText('Amount (ZEC)')).toHaveValue('4.9999');
    await userEvent.click(screen.getByRole('button', { name: /Send ZEC/i }));
    await waitFor(() => expect(sendCalls(fetchMock)).toHaveLength(1));
    expect(body(fetchMock).amount_zatoshi).toBe(499_990_000);
  });

  it('fills a maximum of 1,000 ZEC without separators and sends it exactly', async () => {
    fetchMock.mockImplementation(
      sendImpl({
        available_zatoshi: 100_000_010_000,
        fee_zatoshi: 10_000,
        max_zatoshi: 100_000_000_000,
      }),
    );
    const accounts = testAccounts.map((account) => ({
      ...account,
      ironwood_zatoshi: account.id === 1 ? 100_000_010_000n : 0n,
    }));
    renderWithProviders(<SendDialog open onOpenChange={vi.fn()} accounts={accounts} />);

    const max = await screen.findByRole('button', { name: 'Max' });
    await waitFor(() => expect(max).toBeEnabled());
    expect(
      screen.getByText('1,000 ZEC spendable after a 0.0001 ZEC network fee.'),
    ).toBeInTheDocument();
    await userEvent.click(max);

    expect(screen.getByLabelText('Amount (ZEC)')).toHaveValue('1000');
    await userEvent.click(screen.getByRole('button', { name: /Send ZEC/i }));
    await waitFor(() => expect(sendCalls(fetchMock)).toHaveLength(1));
    expect(body(fetchMock).amount_zatoshi).toBe(100_000_000_000);
  });

  it('disables Max when nothing is spendable after the fee', async () => {
    fetchMock.mockImplementation(sendImpl({ ...QUOTE, available_zatoshi: 10_000, max_zatoshi: 0 }));
    renderWithProviders(<SendDialog open onOpenChange={vi.fn()} accounts={testAccounts} />);

    expect(await screen.findByRole('button', { name: 'Max' })).toBeDisabled();
  });

  it('quotes the fee without the destination account', async () => {
    renderWithProviders(<SendDialog open onOpenChange={vi.fn()} accounts={testAccounts} />);

    const call = await waitFor(() => {
      const found = fetchMock.mock.calls.find(([input]) =>
        requestUrl(input).endsWith('/send/quote'),
      );
      expect(found).toBeDefined();
      return found as [RequestInfo | URL, RequestInit | undefined];
    });
    const body = JSON.parse(typeof call[1]?.body === 'string' ? call[1].body : '{}') as Record<
      string,
      unknown
    >;
    expect(body).toEqual({
      from_account: 1,
      source_pool: 'ironwood',
      destination_pool: 'ironwood',
    });
  });

  it('shows what the selected source can actually spend', () => {
    renderWithProviders(
      <SendDialog open onOpenChange={vi.fn()} accounts={testAccounts} defaultAccountId={1} />,
    );
    expect(screen.getByText(/5 ZEC available in the ironwood pool/i)).toBeInTheDocument();
  });

  async function applyPaymentUri(uri: string) {
    renderWithProviders(<SendDialog open onOpenChange={vi.fn()} accounts={testAccounts} />);
    await userEvent.type(screen.getByLabelText('Payment request (optional)'), uri);
    await userEvent.click(screen.getByRole('button', { name: 'Apply' }));
  }

  it.each(['button', 'form'] as const)(
    'blocks %s submission until the current nonempty URI is successfully applied',
    async (submission) => {
      const uriA = 'zcash:uregtest1requestA?amount=0.25';
      const uriB = 'zcash:uregtest1requestB?amount=0.75';
      const parsedA = {
        address: 'uregtest1requestA',
        destination_pool: 'ironwood',
        to_account: null,
        amount_zatoshi: 25_000_000,
        memo: 'request A',
      };
      const parsedB = {
        ...parsedA,
        address: 'uregtest1requestB',
        amount_zatoshi: 75_000_000,
        memo: 'request B',
      };
      fetchMock.mockImplementation(sendImpl(QUOTE, undefined, parsedA));
      renderWithProviders(<SendDialog open onOpenChange={vi.fn()} accounts={testAccounts} />);
      const uriField = screen.getByLabelText('Payment request (optional)');
      const sendButton = screen.getByRole('button', { name: /Send ZEC/i });
      const attemptSend = async () => {
        expect(sendButton).toBeDisabled();
        if (submission === 'button') await userEvent.click(sendButton);
        else await act(() => fireEvent.submit(sendButton.closest('form')!));
        expect(sendCalls(fetchMock)).toHaveLength(0);
      };
      await userEvent.type(uriField, uriA);
      await attemptSend();
      await userEvent.click(screen.getByRole('button', { name: 'Apply' }));
      await waitFor(() => expect(sendButton).toBeEnabled());
      expect(screen.getByLabelText('Destination address')).toHaveValue(parsedA.address);
      await userEvent.clear(uriField);
      await userEvent.type(uriField, uriB);
      await attemptSend();
      fetchMock.mockImplementation((input, init) =>
        requestUrl(input).endsWith('/zip321/parse')
          ? Promise.resolve(
              new Response(
                JSON.stringify({ error: { message: 'Current URI error', status: 400 } }),
                { status: 400, headers: { 'content-type': 'application/json' } },
              ),
            )
          : sendImpl(QUOTE)(input, init),
      );
      await userEvent.click(screen.getByRole('button', { name: 'Apply' }));
      expect(await screen.findByText('Current URI error')).toBeInTheDocument();
      await attemptSend();
      expect(screen.getByLabelText('Destination address')).toHaveValue(parsedA.address);
      fetchMock.mockImplementation(sendImpl(QUOTE, undefined, parsedB));
      await userEvent.click(screen.getByRole('button', { name: 'Apply' }));
      await waitFor(() => expect(sendButton).toBeEnabled());
      await userEvent.click(sendButton);
      await waitFor(() => expect(sendCalls(fetchMock)).toHaveLength(1));
      expect(body(fetchMock)).toMatchObject({
        to_address: parsedB.address,
        amount_zatoshi: 75_000_000,
        memo: parsedB.memo,
      });
      expect(body(fetchMock)).not.toHaveProperty('to_account');
    },
  );

  it('allows an intentional manual send after clearing an unapplied URI', async () => {
    renderWithProviders(<SendDialog open onOpenChange={vi.fn()} accounts={testAccounts} />);
    const uriField = screen.getByLabelText('Payment request (optional)');
    await userEvent.type(uriField, 'zcash:uregtest1requestA?amount=0.25');
    expect(screen.getByRole('button', { name: /Send ZEC/i })).toBeDisabled();
    await userEvent.clear(uriField);
    await userEvent.click(screen.getByRole('button', { name: /Send ZEC/i }));
    await waitFor(() => expect(sendCalls(fetchMock)).toHaveLength(1));
    expect(body(fetchMock)).toMatchObject({ to_account: 2, amount_zatoshi: 100_000_000 });
  });

  it.each([
    ['changed', 'success'],
    ['cleared', 'success'],
    ['restored', 'success'],
    ['changed', 'error'],
    ['cleared', 'error'],
    ['restored', 'error'],
  ] as const)('ignores a stale parse after the URI is %s (%s response)', async (edit, outcome) => {
    const uriA = 'zcash:uregtest1requestA?amount=0.25';
    const uriB = 'zcash:uregtest1requestB?amount=0.75';
    const parsedB = {
      address: 'uregtest1requestB',
      destination_pool: 'ironwood',
      to_account: null,
      amount_zatoshi: 75_000_000,
      memo: 'request B',
    };
    let finishParsing!: (response: Response) => void;
    const parsing = new Promise<Response>((resolve) => {
      finishParsing = resolve;
    });
    fetchMock.mockImplementation((input, init) => {
      if (requestUrl(input).endsWith('/zip321/parse')) {
        const raw = typeof init?.body === 'string' ? init.body : '{}';
        const { uri } = JSON.parse(raw) as { uri: string };
        if (uri === uriA) return parsing;
      }
      return sendImpl(QUOTE, undefined, parsedB)(input, init);
    });
    await applyPaymentUri(uriA);
    await waitFor(() =>
      expect(screen.getByRole('button', { name: 'Apply' })).toHaveAttribute('aria-busy', 'true'),
    );

    const uriField = screen.getByLabelText('Payment request (optional)');
    await userEvent.clear(uriField);
    if (edit !== 'cleared') await userEvent.type(uriField, uriB);
    if (edit === 'restored') {
      await userEvent.clear(uriField);
      await userEvent.type(uriField, uriA);
    }
    const sendButton = screen.getByRole('button', { name: /Send ZEC/i });
    if (edit === 'cleared') expect(sendButton).toBeEnabled();
    else {
      expect(sendButton).toBeDisabled();
      await userEvent.click(sendButton);
      await act(() => fireEvent.submit(sendButton.closest('form')!));
      expect(sendCalls(fetchMock)).toHaveLength(0);
    }
    expect(screen.getByRole('button', { name: 'Apply' })).not.toHaveAttribute('aria-busy', 'true');

    await act(async () => {
      finishParsing(
        new Response(
          JSON.stringify(
            outcome === 'success'
              ? {
                  address: 'uregtest1requestA',
                  destination_pool: 'ironwood',
                  to_account: null,
                  amount_zatoshi: 25_000_000,
                  memo: 'request A',
                }
              : { error: { message: 'Obsolete URI error', status: 400 } },
          ),
          {
            status: outcome === 'success' ? 200 : 400,
            headers: { 'content-type': 'application/json' },
          },
        ),
      );
      await parsing;
    });

    if (edit === 'cleared') expect(sendButton).toBeEnabled();
    else {
      expect(sendButton).toBeDisabled();
      await act(() => fireEvent.submit(sendButton.closest('form')!));
    }
    expect(screen.getByLabelText('Destination')).toHaveTextContent('Account 2');
    expect(screen.queryByLabelText('Destination address')).not.toBeInTheDocument();
    expect(screen.getByLabelText('Amount (ZEC)')).toHaveValue('1');
    expect(screen.getByLabelText('Memo (optional)')).toHaveValue('');
    expect(screen.queryByText('Obsolete URI error')).not.toBeInTheDocument();
    expect(sendCalls(fetchMock)).toHaveLength(0);

    await userEvent.clear(uriField);
    await userEvent.type(uriField, uriB);
    await userEvent.click(screen.getByRole('button', { name: 'Apply' }));
    expect(await screen.findByLabelText('Destination address')).toHaveValue('uregtest1requestB');
    await userEvent.click(screen.getByRole('button', { name: /Send ZEC/i }));
    await waitFor(() => expect(sendCalls(fetchMock)).toHaveLength(1));
    expect(body(fetchMock)).toMatchObject({
      from_account: 1,
      to_address: 'uregtest1requestB',
      source_pool: 'ironwood',
      destination_pool: 'ironwood',
      amount_zatoshi: 75_000_000,
      memo: 'request B',
    });
    expect(body(fetchMock)).not.toHaveProperty('to_account');
  });

  it.each(['button', 'form'] as const)(
    'blocks %s submission while a payment URI is parsing',
    async (submission) => {
      const parsed = {
        address: 'uregtest1external',
        destination_pool: 'ironwood',
        to_account: null,
        amount_zatoshi: 25_000_000,
        memo: 'coffee',
      };
      let finishParsing!: (response: Response) => void;
      const parsing = new Promise<Response>((resolve) => {
        finishParsing = resolve;
      });
      fetchMock.mockImplementation((input, init) =>
        requestUrl(input).endsWith('/zip321/parse') ? parsing : sendImpl(QUOTE)(input, init),
      );
      await applyPaymentUri('zcash:uregtest1external?amount=0.25');
      await waitFor(() =>
        expect(screen.getByRole('button', { name: 'Apply' })).toHaveAttribute('aria-busy', 'true'),
      );
      const sendButton = screen.getByRole('button', { name: /Send ZEC/i });
      expect(screen.getByLabelText('Destination')).toHaveTextContent('Account 2');
      expect(screen.getByLabelText('Amount (ZEC)')).toHaveValue('1');

      if (submission === 'button') {
        expect(sendButton).toBeDisabled();
        await userEvent.click(sendButton);
      } else {
        // A disabled button alone cannot protect keyboard/programmatic form submission.
        await act(async () => {
          fireEvent.submit(sendButton.closest('form')!);
          await Promise.resolve();
        });
      }
      expect(sendCalls(fetchMock)).toHaveLength(0);

      await act(async () => {
        finishParsing(
          new Response(JSON.stringify(parsed), {
            status: 200,
            headers: { 'content-type': 'application/json' },
          }),
        );
        await parsing;
      });
      expect(await screen.findByLabelText('Destination address')).toHaveValue(parsed.address);
      expect(screen.getByLabelText('Amount (ZEC)')).toHaveValue('0.25');
      await waitFor(() => expect(sendButton).toBeEnabled());
      await userEvent.click(sendButton);
      await waitFor(() => expect(sendCalls(fetchMock)).toHaveLength(1));
      expect(body(fetchMock)).toMatchObject({
        to_address: parsed.address,
        amount_zatoshi: parsed.amount_zatoshi,
        memo: parsed.memo,
      });
      expect(body(fetchMock)).not.toHaveProperty('to_account');
    },
  );

  it.each([
    ['older first', 'success', 'success'],
    ['newer first', 'success', 'success'],
    ['older first', 'error', 'success'],
    ['newer first', 'error', 'success'],
    ['older first', 'success', 'error'],
    ['newer first', 'success', 'error'],
  ] as const)(
    'keeps the current URI when responses finish %s (%s old, %s current)',
    async (order, outcome, currentOutcome) => {
      const uriA = 'zcash:uregtest1requestA?amount=0.25';
      const uriB = 'zcash:uregtest1requestB?amount=0.75';
      const parsedB = {
        address: 'uregtest1requestB',
        destination_pool: 'ironwood',
        to_account: null,
        amount_zatoshi: 75_000_000,
        memo: 'request B',
      };
      let finishA!: (response: Response) => void;
      let finishB!: (response: Response) => void;
      const parsingA = new Promise<Response>((resolve) => {
        finishA = resolve;
      });
      const parsingB = new Promise<Response>((resolve) => {
        finishB = resolve;
      });
      fetchMock.mockImplementation((input, init) => {
        if (requestUrl(input).endsWith('/zip321/parse')) {
          const raw = typeof init?.body === 'string' ? init.body : '{}';
          const { uri } = JSON.parse(raw) as { uri: string };
          return uri === uriA ? parsingA : parsingB;
        }
        return sendImpl(QUOTE)(input, init);
      });
      await applyPaymentUri(uriA);
      await waitFor(() =>
        expect(screen.getByRole('button', { name: 'Apply' })).toHaveAttribute('aria-busy', 'true'),
      );
      const uriField = screen.getByLabelText('Payment request (optional)');
      await userEvent.clear(uriField);
      await userEvent.type(uriField, uriB);
      await userEvent.keyboard('{Enter}');
      const parseCalls = () =>
        fetchMock.mock.calls.filter(([input]) => requestUrl(input).endsWith('/zip321/parse'));
      await waitFor(() => expect(parseCalls()).toHaveLength(2));
      const sendButton = screen.getByRole('button', { name: /Send ZEC/i });
      expect(sendButton).toBeDisabled();

      const resolveA = async () => {
        await act(async () => {
          finishA(
            new Response(
              JSON.stringify(
                outcome === 'success'
                  ? {
                      address: 'uregtest1requestA',
                      destination_pool: 'ironwood',
                      to_account: null,
                      amount_zatoshi: 25_000_000,
                      memo: 'request A',
                    }
                  : { error: { message: 'Obsolete URI error', status: 400 } },
              ),
              {
                status: outcome === 'success' ? 200 : 400,
                headers: { 'content-type': 'application/json' },
              },
            ),
          );
          await parsingA;
        });
      };
      if (order === 'older first') {
        await resolveA();
        expect(sendButton).toBeDisabled();
        expect(screen.getByLabelText('Destination')).toHaveTextContent('Account 2');
        expect(screen.queryByText('Obsolete URI error')).not.toBeInTheDocument();
      }
      await act(async () => {
        finishB(
          new Response(
            JSON.stringify(
              currentOutcome === 'success'
                ? parsedB
                : { error: { message: 'Current URI error', status: 400 } },
            ),
            {
              status: currentOutcome === 'success' ? 200 : 400,
              headers: { 'content-type': 'application/json' },
            },
          ),
        );
        await parsingB;
      });
      await waitFor(() => {
        if (currentOutcome === 'success') expect(sendButton).toBeEnabled();
        else expect(sendButton).toBeDisabled();
      });
      if (order === 'newer first') await resolveA();
      if (currentOutcome === 'error') {
        expect(screen.getByText('Current URI error')).toBeInTheDocument();
        expect(screen.getByLabelText('Destination')).toHaveTextContent('Account 2');
        expect(screen.getByLabelText('Amount (ZEC)')).toHaveValue('1');
        expect(screen.getByLabelText('Memo (optional)')).toHaveValue('');
        expect(sendCalls(fetchMock)).toHaveLength(0);
        fetchMock.mockImplementation(sendImpl(QUOTE, undefined, parsedB));
        await userEvent.click(screen.getByRole('button', { name: 'Apply' }));
        expect(await screen.findByLabelText('Destination address')).toHaveValue(
          'uregtest1requestB',
        );
      }
      expect(screen.getByLabelText('Destination address')).toHaveValue('uregtest1requestB');
      expect(screen.getByLabelText('Amount (ZEC)')).toHaveValue('0.75');
      expect(screen.getByLabelText('Memo (optional)')).toHaveValue('request B');
      expect(screen.queryByText('Obsolete URI error')).not.toBeInTheDocument();
      await userEvent.click(sendButton);
      await waitFor(() => expect(sendCalls(fetchMock)).toHaveLength(1));
      expect(body(fetchMock)).toMatchObject({
        to_address: 'uregtest1requestB',
        amount_zatoshi: 75_000_000,
        memo: 'request B',
      });
    },
  );

  it('does not apply a pending URI to a reopened dialog', async () => {
    let finishParsing!: (response: Response) => void;
    const parsing = new Promise<Response>((resolve) => {
      finishParsing = resolve;
    });
    fetchMock.mockImplementation((input, init) =>
      requestUrl(input).endsWith('/zip321/parse') ? parsing : sendImpl(QUOTE)(input, init),
    );
    // Closing in WalletActionsProvider unmounts SendDialog but retains the query provider.
    function ReopenableDialog() {
      const [open, setOpen] = useState(true);
      return open ? (
        <SendDialog open onOpenChange={setOpen} accounts={testAccounts} />
      ) : (
        <button onClick={() => setOpen(true)}>Reopen Send</button>
      );
    }
    renderWithProviders(<ReopenableDialog />);
    await userEvent.type(
      screen.getByLabelText('Payment request (optional)'),
      'zcash:uregtest1requestA?amount=0.25',
    );
    await userEvent.click(screen.getByRole('button', { name: 'Apply' }));
    await waitFor(() =>
      expect(screen.getByRole('button', { name: 'Apply' })).toHaveAttribute('aria-busy', 'true'),
    );
    await userEvent.click(screen.getByRole('button', { name: 'Close dialog' }));
    await userEvent.click(screen.getByRole('button', { name: 'Reopen Send' }));
    await act(async () => {
      finishParsing(
        new Response(
          JSON.stringify({
            address: 'uregtest1requestA',
            destination_pool: 'ironwood',
            to_account: null,
            amount_zatoshi: 25_000_000,
            memo: 'request A',
          }),
          { status: 200, headers: { 'content-type': 'application/json' } },
        ),
      );
      await parsing;
    });
    expect(screen.getByLabelText('Payment request (optional)')).toHaveValue('');
    expect(screen.getByLabelText('Destination')).toHaveTextContent('Account 2');
    expect(screen.getByLabelText('Amount (ZEC)')).toHaveValue('1');
    expect(screen.getByLabelText('Memo (optional)')).toHaveValue('');
    expect(screen.queryByLabelText('Destination address')).not.toBeInTheDocument();
    expect(sendCalls(fetchMock)).toHaveLength(0);
  });

  it('fills the form from a pasted zcash: URI and sends to its address with the memo', async () => {
    fetchMock.mockRestore();
    fetchMock = mockSend(QUOTE, {
      address: 'uregtest1external',
      destination_pool: 'ironwood',
      to_account: null,
      amount_zatoshi: 25_000_000,
      memo: 'coffee',
    });
    await applyPaymentUri('zcash:uregtest1external?amount=0.25');

    expect(await screen.findByLabelText('Destination address')).toHaveValue('uregtest1external');
    expect(screen.getByLabelText('Amount (ZEC)')).toHaveValue('0.25');
    expect(screen.getByLabelText('Memo (optional)')).toHaveValue('coffee');

    await userEvent.click(screen.getByRole('button', { name: /Send ZEC/i }));
    await waitFor(() => expect(sendCalls(fetchMock)).toHaveLength(1));
    expect(body(fetchMock)).toMatchObject({
      to_address: 'uregtest1external',
      amount_zatoshi: 25_000_000,
      destination_pool: 'ironwood',
      memo: 'coffee',
    });
    expect(body(fetchMock).to_account).toBeUndefined();
  });

  it('cancels a waiting send when the payment URI is edited without applying it', async () => {
    let finishQuote!: (response: Response) => void;
    const quoting = new Promise<Response>((resolve) => {
      finishQuote = resolve;
    });
    const parsed = {
      address: 'uregtest1requestB',
      destination_pool: 'ironwood',
      to_account: null,
      amount_zatoshi: 75_000_000,
      memo: 'request B',
    };
    fetchMock.mockImplementation((input, init) =>
      requestUrl(input).endsWith('/send/quote')
        ? quoting
        : sendImpl(QUOTE, undefined, parsed)(input, init),
    );
    renderWithProviders(<SendDialog open onOpenChange={vi.fn()} accounts={testAccounts} />);
    await userEvent.click(screen.getByRole('button', { name: /Send ZEC/i }));
    await userEvent.type(
      screen.getByLabelText('Payment request (optional)'),
      'zcash:uregtest1requestB?amount=0.75',
    );
    await act(async () => {
      finishQuote(
        new Response(JSON.stringify(QUOTE), {
          status: 200,
          headers: { 'content-type': 'application/json' },
        }),
      );
      await quoting;
    });
    await waitFor(() => expect(screen.getByRole('button', { name: 'Max' })).toBeEnabled());
    expect(sendCalls(fetchMock)).toHaveLength(0);

    await userEvent.click(screen.getByRole('button', { name: 'Apply' }));
    expect(await screen.findByLabelText('Destination address')).toHaveValue('uregtest1requestB');
    await userEvent.click(screen.getByRole('button', { name: /Send ZEC/i }));
    await waitFor(() => expect(sendCalls(fetchMock)).toHaveLength(1));
    expect(body(fetchMock)).toMatchObject({
      to_address: 'uregtest1requestB',
      amount_zatoshi: 75_000_000,
      memo: 'request B',
    });
    expect(body(fetchMock)).not.toHaveProperty('to_account');
  });

  it('does not send a stale draft after the fee quote resolves', async () => {
    let finishQuote!: (response: Response) => void;
    const quoting = new Promise<Response>((resolve) => {
      finishQuote = resolve;
    });
    fetchMock.mockImplementation((input, init) =>
      requestUrl(input).endsWith('/send/quote') ? quoting : sendImpl(QUOTE)(input, init),
    );
    renderWithProviders(<SendDialog open onOpenChange={vi.fn()} accounts={testAccounts} />);

    await userEvent.click(screen.getByRole('button', { name: /Send ZEC/i }));
    await waitFor(() =>
      expect(screen.getByRole('button', { name: 'Checking fee…' })).toBeDisabled(),
    );
    fireEvent.change(screen.getByLabelText('Amount (ZEC)'), { target: { value: '2' } });

    await act(async () => {
      finishQuote(
        new Response(JSON.stringify(QUOTE), {
          status: 200,
          headers: { 'content-type': 'application/json' },
        }),
      );
      await quoting;
    });

    expect(sendCalls(fetchMock)).toHaveLength(0);
    expect(screen.getByLabelText('Amount (ZEC)')).toHaveValue('2');
    expect(screen.getByRole('alert')).toHaveTextContent(
      'The send details changed while checking the fee.',
    );

    await userEvent.click(screen.getByRole('button', { name: /Send ZEC/i }));
    await waitFor(() => expect(sendCalls(fetchMock)).toHaveLength(1));
    expect(body(fetchMock).amount_zatoshi).toBe(200_000_000);
  });

  it('allows only one send submission while the initial fee quote is pending', async () => {
    let finishQuote!: (response: Response) => void;
    const quoting = new Promise<Response>((resolve) => {
      finishQuote = resolve;
    });
    fetchMock.mockImplementation((input, init) =>
      requestUrl(input).endsWith('/send/quote') ? quoting : sendImpl(QUOTE)(input, init),
    );
    renderWithProviders(<SendDialog open onOpenChange={vi.fn()} accounts={testAccounts} />);
    const form = screen.getByRole('button', { name: /Send ZEC/i }).closest('form');
    expect(form).not.toBeNull();

    act(() => {
      fireEvent.submit(form!);
      fireEvent.submit(form!);
    });
    await waitFor(() =>
      expect(screen.getByRole('button', { name: 'Checking fee…' })).toBeDisabled(),
    );

    await act(async () => {
      finishQuote(
        new Response(JSON.stringify(QUOTE), {
          status: 200,
          headers: { 'content-type': 'application/json' },
        }),
      );
      await quoting;
    });

    await waitFor(() => expect(sendCalls(fetchMock)).toHaveLength(1));
    expect(body(fetchMock).amount_zatoshi).toBe(100_000_000);
  });

  it.each([false, true])(
    'cancels a waiting send when URI parsing starts (parsing finished: %s)',
    async (parsingFinished) => {
      let finishQuote!: (response: Response) => void;
      let finishParsing!: (response: Response) => void;
      const quoting = new Promise<Response>((resolve) => {
        finishQuote = resolve;
      });
      const parsing = new Promise<Response>((resolve) => {
        finishParsing = resolve;
      });
      const parsed = {
        address: 'uregtest1external',
        destination_pool: 'ironwood',
        to_account: null,
        amount_zatoshi: 25_000_000,
        memo: null,
      };
      fetchMock.mockImplementation((input, init) => {
        if (requestUrl(input).endsWith('/send/quote')) return quoting;
        if (requestUrl(input).endsWith('/zip321/parse')) return parsing;
        return sendImpl(QUOTE)(input, init);
      });
      renderWithProviders(<SendDialog open onOpenChange={vi.fn()} accounts={testAccounts} />);
      await userEvent.click(screen.getByRole('button', { name: /Send ZEC/i }));
      await userEvent.type(
        screen.getByLabelText('Payment request (optional)'),
        'zcash:uregtest1external?amount=0.25',
      );
      expect(sendCalls(fetchMock)).toHaveLength(0);
      await userEvent.click(screen.getByRole('button', { name: 'Apply' }));
      await waitFor(() =>
        expect(screen.getByRole('button', { name: 'Apply' })).toHaveAttribute('aria-busy', 'true'),
      );
      const resolveParsing = async () => {
        await act(async () => {
          finishParsing(
            new Response(JSON.stringify(parsed), {
              status: 200,
              headers: { 'content-type': 'application/json' },
            }),
          );
          await parsing;
        });
      };
      if (parsingFinished) await resolveParsing();
      await act(async () => {
        finishQuote(
          new Response(JSON.stringify(QUOTE), {
            status: 200,
            headers: { 'content-type': 'application/json' },
          }),
        );
        await quoting;
      });
      expect(sendCalls(fetchMock)).toHaveLength(0);
      if (!parsingFinished) await resolveParsing();
      expect(await screen.findByLabelText('Destination address')).toHaveValue(parsed.address);
      await userEvent.click(screen.getByRole('button', { name: /Send ZEC/i }));
      await waitFor(() => expect(sendCalls(fetchMock)).toHaveLength(1));
      expect(body(fetchMock)).toMatchObject({
        to_address: parsed.address,
        amount_zatoshi: parsed.amount_zatoshi,
      });
      expect(body(fetchMock)).not.toHaveProperty('to_account');
    },
  );

  it('clears the amount for a URI without one and requires the sender to enter it', async () => {
    fetchMock.mockRestore();
    fetchMock = mockSend(QUOTE, {
      address: 'tmAccount3',
      destination_pool: 'transparent',
      to_account: 3,
      amount_zatoshi: null,
      memo: null,
    });
    await applyPaymentUri('zcash:tmAccount3');

    await waitFor(() =>
      expect(screen.getByLabelText('Destination')).toHaveTextContent('Account 3'),
    );
    // The dialog's default of 1 ZEC must not carry over into a request that named no amount.
    expect(screen.getByLabelText('Amount (ZEC)')).toHaveValue('');

    await userEvent.click(screen.getByRole('button', { name: /Send ZEC/i }));
    expect(await screen.findByText('Enter an amount.')).toBeInTheDocument();
    expect(sendCalls(fetchMock)).toHaveLength(0);
  });

  it('shows why a URI was rejected without changing the form', async () => {
    fetchMock.mockRestore();
    fetchMock = vi
      .spyOn(globalThis, 'fetch')
      .mockImplementation((input, init) =>
        requestUrl(input).endsWith('/zip321/parse')
          ? Promise.resolve(
              new Response(
                JSON.stringify({ error: { message: 'Invalid payment URI', status: 400 } }),
                { status: 400, headers: { 'content-type': 'application/json' } },
              ),
            )
          : sendImpl(QUOTE)(input, init),
      );
    await applyPaymentUri('zcash:?address=a');

    expect(await screen.findByText('Invalid payment URI')).toBeInTheDocument();
    expect(screen.getByLabelText('Destination')).toHaveTextContent('Account 2');
    expect(screen.getByLabelText('Amount (ZEC)')).toHaveValue('1');
  });
});

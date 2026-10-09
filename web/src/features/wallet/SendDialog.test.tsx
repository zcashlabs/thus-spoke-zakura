import { describe, expect, it, vi, beforeEach, afterEach } from 'vitest';
import { screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { renderWithProviders, requestUrl, testAccounts } from '@/test/utils';
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

/** `parsed` answers `/zip321/parse`; sends echo their recipient back as an activity. */
function sendImpl(quote: typeof QUOTE, parsed: unknown = null) {
  return (input: RequestInfo | URL, init?: RequestInit) => {
    if (requestUrl(input).endsWith('/zip321/parse')) {
      return Promise.resolve(
        new Response(JSON.stringify(parsed), {
          status: 200,
          headers: { 'content-type': 'application/json' },
        }),
      );
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
        JSON.stringify({
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
        }),
        { status: 200, headers: { 'content-type': 'application/json' } },
      ),
    );
  };
}

function mockSend(quote = QUOTE, parsed: unknown = null) {
  return vi.spyOn(globalThis, 'fetch').mockImplementation(sendImpl(quote, parsed));
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
    fetchMock = mockSend();
  });
  afterEach(() => {
    fetchMock.mockRestore();
  });

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

  it('uses a different idempotency key for a different destination address', async () => {
    // Every send's response is lost, so each key stays stored for a retry.
    fetchMock.mockImplementation((input, init) =>
      requestUrl(input).endsWith('/send')
        ? Promise.reject(new TypeError('response lost'))
        : sendImpl(QUOTE)(input, init),
    );
    renderWithProviders(<SendDialog open onOpenChange={vi.fn()} accounts={testAccounts} />);
    await selectTransferField('Destination', 'Other address');
    const address = screen.getByLabelText('Destination address');
    for (const destination of ['uregtest1first', 'uregtest1first', 'uregtest1second']) {
      await userEvent.clear(address);
      await userEvent.type(address, destination);
      await userEvent.click(screen.getByRole('button', { name: /Send ZEC/i }));
    }
    await waitFor(() => expect(sendCalls(fetchMock)).toHaveLength(3));
    const keys = sendCalls(fetchMock).map(([, init]) => requestBody(init).idempotency_key);
    expect(keys[0]).toBe(keys[1]);
    expect(keys[2]).not.toBe(keys[1]);
  });

  it.each([false, true])(
    'keeps a lost-response key across remounts (same account: %s)',
    async (sameAccount) => {
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
      renderWithProviders(<SendDialog open onOpenChange={vi.fn()} accounts={accounts} />);
      await chooseRoute();
      const memo = screen.getByLabelText('Memo (optional)');
      await userEvent.type(memo, 'rent');
      await userEvent.click(screen.getByRole('button', { name: /Send ZEC/i }));
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

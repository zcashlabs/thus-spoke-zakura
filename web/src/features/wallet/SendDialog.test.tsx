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
  amount_zatoshi: number;
  idempotency_key: string;
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

function sendImpl(quote: typeof QUOTE) {
  return (input: RequestInfo | URL, init?: RequestInit) => {
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
          from_account: 1,
          to_account: 2,
          source_pool: 'orchard',
          destination_pool: 'orchard',
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

function mockSend(quote = QUOTE) {
  return vi.spyOn(globalThis, 'fetch').mockImplementation(sendImpl(quote));
}

// Filters out the quote fetch the dialog makes on mount.
const sendCalls = (fetchMock: ReturnType<typeof mockSend>) =>
  fetchMock.mock.calls.filter(([input]) => requestUrl(input).endsWith('/send'));

const body = (fetchMock: ReturnType<typeof mockSend>): SendBody =>
  requestBody(sendCalls(fetchMock)[0]?.[1]);

describe('SendDialog', () => {
  let fetchMock: ReturnType<typeof mockSend>;

  beforeEach(() => {
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

  it('carries an idempotency key so a retry cannot double-spend', async () => {
    await submitAmount('1');
    await waitFor(() => expect(sendCalls(fetchMock)).toHaveLength(1));
    expect(body(fetchMock).idempotency_key).toHaveLength(36);
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
    expect(screen.getByLabelText('Destination account')).toBeInTheDocument();
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
    expect(screen.getByLabelText('Destination account')).not.toHaveTextContent('Account 2');
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

    expect(await screen.findByText(/holds 0 ZEC in the orchard pool/i)).toBeInTheDocument();
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
      source_pool: 'orchard',
      destination_pool: 'orchard',
    });
  });

  it('shows what the selected source can actually spend', () => {
    renderWithProviders(
      <SendDialog open onOpenChange={vi.fn()} accounts={testAccounts} defaultAccountId={1} />,
    );
    expect(screen.getByText(/5 ZEC available in the orchard pool/i)).toBeInTheDocument();
  });
});

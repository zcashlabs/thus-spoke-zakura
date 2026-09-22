import { describe, expect, it, vi, beforeEach, afterEach } from 'vitest';
import { screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { renderWithProviders, testAccounts } from '@/test/utils';
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

function mockSend() {
  const fetchMock = vi.spyOn(globalThis, 'fetch').mockImplementation((_input, init) =>
    Promise.resolve(
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
    ),
  );
  return fetchMock;
}

const body = (fetchMock: ReturnType<typeof mockSend>): SendBody =>
  requestBody(fetchMock.mock.calls[0]?.[1]);

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

  it('sends the amount as exact zatoshi', async () => {
    await submitAmount('1.5');
    await waitFor(() => expect(fetchMock).toHaveBeenCalled());
    expect(body(fetchMock).amount_zatoshi).toBe(150_000_000);
  });

  it('parses a value that is not exactly representable as a float', async () => {
    // Number('2.675') * 1e8 is 267499999.99999997. Math.round would also
    // land on the right answer here, so this pins exactness rather than
    // catching the old implementation; see money.test.ts for the value that
    // genuinely separates the two.
    await submitAmount('2.675');
    await waitFor(() => expect(fetchMock).toHaveBeenCalled());
    expect(body(fetchMock).amount_zatoshi).toBe(267_500_000);
  });

  it('sends a single zatoshi without rounding it away', async () => {
    await submitAmount('0.00000001');
    await waitFor(() => expect(fetchMock).toHaveBeenCalled());
    expect(body(fetchMock).amount_zatoshi).toBe(1);
  });

  it('uses a new idempotency key after a successful payment', async () => {
    await submitAmount('1');
    await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(1));
    const first = body(fetchMock).idempotency_key;
    await waitFor(() => expect(screen.getByRole('button', { name: /Send ZEC/i })).toBeEnabled());
    await userEvent.click(screen.getByRole('button', { name: /Send ZEC/i }));
    await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(2));

    expect(first).toHaveLength(36);
    expect(first).not.toBe(requestBody(fetchMock.mock.calls[1]?.[1]).idempotency_key);
  });

  it('reuses the idempotency key when the same submission is retried', async () => {
    fetchMock.mockRejectedValueOnce(new TypeError('response lost'));
    renderWithProviders(<SendDialog open onOpenChange={vi.fn()} accounts={testAccounts} />);
    const amount = screen.getByLabelText('Amount (ZEC)');
    const submit = screen.getByRole('button', { name: /Send ZEC/i });
    await userEvent.clear(amount);
    await userEvent.type(amount, '1');

    await userEvent.click(submit);
    await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(1));
    await waitFor(() => expect(submit).toBeEnabled());
    await userEvent.click(submit);
    await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(2));

    expect(requestBody(fetchMock.mock.calls[0]?.[1]).idempotency_key).toBe(
      requestBody(fetchMock.mock.calls[1]?.[1]).idempotency_key,
    );
  });

  it('reuses the idempotency key after the dialog is remounted', async () => {
    fetchMock.mockRejectedValueOnce(new TypeError('response lost'));
    const first = renderWithProviders(
      <SendDialog open onOpenChange={vi.fn()} accounts={testAccounts} />,
    );
    let amount = screen.getByLabelText('Amount (ZEC)');
    await userEvent.clear(amount);
    await userEvent.type(amount, '1');
    await userEvent.click(screen.getByRole('button', { name: /Send ZEC/i }));
    await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(1));

    first.unmount();
    renderWithProviders(<SendDialog open onOpenChange={vi.fn()} accounts={testAccounts} />);
    amount = screen.getByLabelText('Amount (ZEC)');
    await userEvent.clear(amount);
    await userEvent.type(amount, '1');
    await userEvent.click(screen.getByRole('button', { name: /Send ZEC/i }));
    await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(2));

    expect(requestBody(fetchMock.mock.calls[0]?.[1]).idempotency_key).toBe(
      requestBody(fetchMock.mock.calls[1]?.[1]).idempotency_key,
    );
  });

  it('uses a new idempotency key after the payment parameters change', async () => {
    fetchMock.mockRejectedValueOnce(new TypeError('response lost'));
    renderWithProviders(<SendDialog open onOpenChange={vi.fn()} accounts={testAccounts} />);
    const amount = screen.getByLabelText('Amount (ZEC)');
    const submit = screen.getByRole('button', { name: /Send ZEC/i });
    await userEvent.clear(amount);
    await userEvent.type(amount, '1');
    await userEvent.click(submit);
    await waitFor(() => expect(submit).toBeEnabled());

    await userEvent.clear(amount);
    await userEvent.type(amount, '2');
    await userEvent.click(submit);
    await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(2));

    expect(requestBody(fetchMock.mock.calls[0]?.[1]).idempotency_key).not.toBe(
      requestBody(fetchMock.mock.calls[1]?.[1]).idempotency_key,
    );
  });

  it('refuses a zero amount without calling the API', async () => {
    await submitAmount('0');
    expect(await screen.findByRole('alert')).toHaveTextContent('greater than zero');
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it('refuses more than eight decimal places without calling the API', async () => {
    await submitAmount('0.000000001');
    expect(await screen.findByRole('alert')).toHaveTextContent('8 decimal places');
    expect(fetchMock).not.toHaveBeenCalled();
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
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it('shows what the selected source can actually spend', () => {
    renderWithProviders(
      <SendDialog open onOpenChange={vi.fn()} accounts={testAccounts} defaultAccountId={1} />,
    );
    expect(screen.getByText(/5 ZEC available in the orchard pool/i)).toBeInTheDocument();
  });
});

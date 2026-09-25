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
  memo?: string;
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

  it('carries an idempotency key so a retry cannot double-spend', async () => {
    await submitAmount('1');
    await waitFor(() => expect(fetchMock).toHaveBeenCalled());
    expect(body(fetchMock).idempotency_key).toHaveLength(36);
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
    expect(fetchMock).not.toHaveBeenCalled();
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
    await userEvent.type(memo, 'orchard only');

    await userEvent.click(screen.getByLabelText('Destination pool'));
    await userEvent.click(await screen.findByRole('option', { name: 'Transparent (public)' }));

    expect(memo).toBeDisabled();
    expect(memo).toHaveValue('');
    expect(screen.getByText(/only available for orchard destinations/i)).toBeInTheDocument();

    await userEvent.click(screen.getByRole('button', { name: /Send ZEC/i }));
    await waitFor(() => expect(fetchMock).toHaveBeenCalled());
    expect(body(fetchMock)).not.toHaveProperty('memo');
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

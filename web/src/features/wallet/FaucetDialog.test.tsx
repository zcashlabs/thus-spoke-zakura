import { describe, expect, it, vi } from 'vitest';
import { screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { renderWithProviders, testAccounts } from '@/test/utils';
import { FaucetDialog } from './FaucetDialog';

describe('FaucetDialog', () => {
  it('exposes modal semantics the previous hand-rolled dialog lacked', () => {
    renderWithProviders(<FaucetDialog open onOpenChange={vi.fn()} accounts={testAccounts} />);

    const dialog = screen.getByRole('dialog');
    // The dialog is named and described by real elements, so screen readers
    // announce it. The previous implementation was an unlabelled <div>.
    const labelId = dialog.getAttribute('aria-labelledby');
    const descriptionId = dialog.getAttribute('aria-describedby');
    expect(labelId).toBeTruthy();
    expect(document.getElementById(labelId!)).toHaveTextContent('Fund an account');
    expect(descriptionId).toBeTruthy();
    expect(document.getElementById(descriptionId!)).toBeInTheDocument();
    expect(screen.getByRole('heading', { name: 'Fund an account' })).toBeInTheDocument();
  });

  it('moves focus into the dialog on open', () => {
    renderWithProviders(<FaucetDialog open onOpenChange={vi.fn()} accounts={testAccounts} />);
    expect(screen.getByRole('dialog').contains(document.activeElement)).toBe(true);
  });

  it('closes on Escape', async () => {
    const onOpenChange = vi.fn();
    renderWithProviders(<FaucetDialog open onOpenChange={onOpenChange} accounts={testAccounts} />);

    await userEvent.keyboard('{Escape}');
    await waitFor(() => expect(onOpenChange).toHaveBeenCalledWith(false));
  });

  it('defaults to 1 ZEC rather than the failure-prone 5 ZEC ceiling', () => {
    renderWithProviders(<FaucetDialog open onOpenChange={vi.fn()} accounts={testAccounts} />);
    expect(screen.getByLabelText('Amount (ZEC)')).toHaveValue('1');
  });

  it('reuses the idempotency key after the dialog is remounted', async () => {
    sessionStorage.clear();
    const fetchSpy = vi
      .spyOn(globalThis, 'fetch')
      .mockRejectedValueOnce(new TypeError('response lost'))
      .mockResolvedValue(
        new Response(
          JSON.stringify({
            id: 'a',
            kind: 'faucet',
            from_account: null,
            to_account: 1,
            source_pool: 'orchard',
            destination_pool: 'orchard',
            amount_zatoshi: 100_000_000,
            txid: 'f'.repeat(64),
            block_hash: null,
            status: 'confirmed',
            created_at: '2026-09-22 10:00:00',
          }),
          { status: 200, headers: { 'content-type': 'application/json' } },
        ),
      );
    const first = renderWithProviders(
      <FaucetDialog open onOpenChange={vi.fn()} accounts={testAccounts} />,
    );

    await userEvent.click(screen.getByRole('button', { name: 'Add funds' }));
    await waitFor(() => expect(fetchSpy).toHaveBeenCalledTimes(1));

    first.unmount();
    renderWithProviders(<FaucetDialog open onOpenChange={vi.fn()} accounts={testAccounts} />);
    await userEvent.click(screen.getByRole('button', { name: 'Add funds' }));
    await waitFor(() => expect(fetchSpy).toHaveBeenCalledTimes(2));

    const key = (call: number) => {
      const raw = fetchSpy.mock.calls[call]?.[1]?.body;
      return (JSON.parse(typeof raw === 'string' ? raw : '{}') as { idempotency_key: string })
        .idempotency_key;
    };
    expect(key(0)).toBe(key(1));
    fetchSpy.mockRestore();
  });

  it('shows the faucet limit while editing and prevents submission', async () => {
    const fetchSpy = vi.spyOn(globalThis, 'fetch');
    renderWithProviders(<FaucetDialog open onOpenChange={vi.fn()} accounts={testAccounts} />);

    const amount = screen.getByLabelText('Amount (ZEC)');
    await userEvent.clear(amount);
    await userEvent.type(amount, '6');

    expect(await screen.findByRole('alert')).toHaveTextContent(
      'The faucet is limited to 5 ZEC per request.',
    );
    expect(screen.getByRole('button', { name: 'Add funds' })).toBeDisabled();

    await userEvent.keyboard('{Enter}');
    expect(fetchSpy).not.toHaveBeenCalled();
    fetchSpy.mockRestore();
  });

  it('rejects non-numeric input', async () => {
    renderWithProviders(<FaucetDialog open onOpenChange={vi.fn()} accounts={testAccounts} />);

    const amount = screen.getByLabelText('Amount (ZEC)');
    await userEvent.clear(amount);
    await userEvent.type(amount, 'abc');
    await userEvent.click(screen.getByRole('button', { name: 'Add funds' }));

    expect(await screen.findByRole('alert')).toHaveTextContent('up to 8 decimal places');
  });
});

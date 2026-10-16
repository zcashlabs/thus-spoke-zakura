import { describe, expect, it } from 'vitest';
import { screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { MemoryRouter, useLocation } from 'react-router-dom';
import { renderWithProviders } from '@/test/utils';
import { ApiError } from '@/lib/api';
import { ActivityList } from './ActivityList';

function renderError(error: unknown) {
  renderWithProviders(
    <MemoryRouter>
      <ActivityList activity={{ isPending: false, isError: true, error, data: undefined }} />
    </MemoryRouter>,
  );
}

describe('ActivityList same-account transfers', () => {
  it.each([
    ['ironwood', 'transparent'],
    ['transparent', 'ironwood'],
  ] as const)('renders %s → %s once', (source, destination) => {
    const txid = 'ab'.repeat(32);
    renderWithProviders(
      <MemoryRouter>
        <ActivityList
          activity={{
            isPending: false,
            isError: false,
            error: null,
            data: [
              {
                id: 'self-transfer',
                kind: 'send',
                from_account: 1,
                to_account: 1,
                to_address: null,
                source_pool: source,
                destination_pool: destination,
                amount_zatoshi: 1_000_000n,
                txid,
                block_hash: 'cd'.repeat(32),
                status: 'confirmed',
                last_error: null,
                created_at: '2026-10-02 12:00:00',
              },
            ],
          }}
        />
      </MemoryRouter>,
    );
    const link = screen.getByRole('link', { name: 'Account 1 → Account 1' });
    expect(link).toHaveAttribute('href', `/explorer/tx/${txid}`);
    const row = link.closest('tr');
    expect(row).not.toBeNull();
    expect(within(row!).getByText('confirmed')).toBeInTheDocument();
    expect(within(row!).getByText(source)).toBeInTheDocument();
    expect(within(row!).getByText(destination)).toBeInTheDocument();
    expect(screen.getByText('1 event')).toBeInTheDocument();
  });
});

function CurrentRoute() {
  return <output aria-label="Current route">{useLocation().pathname}</output>;
}

describe('ActivityList error row', () => {
  const txid = 'ab'.repeat(32);

  function renderFailedSend() {
    renderWithProviders(
      <MemoryRouter initialEntries={['/wallet']}>
        <CurrentRoute />
        <ActivityList
          activity={{
            isPending: false,
            isError: false,
            error: null,
            data: [
              {
                id: 'failed-send',
                kind: 'send',
                from_account: 1,
                to_account: 2,
                to_address: null,
                source_pool: 'ironwood',
                destination_pool: 'ironwood',
                amount_zatoshi: 1_000_000n,
                txid,
                block_hash: null,
                status: 'failed',
                last_error: 'transaction expired',
                created_at: '2026-10-02 12:00:00',
              },
            ],
          }}
        />
      </MemoryRouter>,
    );
  }

  it('opens the transaction on a plain click', async () => {
    renderFailedSend();
    await userEvent.click(screen.getByText('Failed: transaction expired'));
    expect(screen.getByLabelText('Current route')).toHaveTextContent(`/explorer/tx/${txid}`);
  });

  it('stays on the current page on a modifier click', async () => {
    const user = userEvent.setup();
    renderFailedSend();
    await user.keyboard('{Meta>}');
    await user.click(screen.getByText('Failed: transaction expired'));
    expect(screen.getByLabelText('Current route')).toHaveTextContent('/wallet');
  });
});

describe('ActivityList error state', () => {
  it("preserves an ApiError's own message", () => {
    renderError(new ApiError(404, 'activity not found'));
    expect(screen.getByText('activity not found')).toBeInTheDocument();
  });

  it('falls back to the shared message for an unrecognised error', () => {
    renderError('boom');
    expect(screen.getByText('Something went wrong.')).toBeInTheDocument();
  });
});

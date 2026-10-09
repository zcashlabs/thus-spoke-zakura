import { describe, expect, it } from 'vitest';
import { screen, within } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import { renderWithProviders } from '@/test/utils';
import { ActivityList } from './ActivityList';

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

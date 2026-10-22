import { describe, expect, it, vi } from 'vitest';
import { act, screen, within } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
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

describe('ActivityList incoming transactions', () => {
  it('renders an external payment to its affected development account', () => {
    const txid = 'ef'.repeat(32);
    renderWithProviders(
      <MemoryRouter>
        <ActivityList
          activity={{
            isPending: false,
            isError: false,
            error: null,
            data: [
              {
                id: 'wallet-incoming-2',
                kind: 'transaction',
                direction: 'incoming',
                from_account: null,
                to_account: 2,
                to_address: null,
                source_pool: null,
                destination_pool: 'ironwood',
                amount_zatoshi: 100_000_000n,
                txid,
                block_hash: 'cd'.repeat(32),
                mined_height: 121,
                status: 'confirmed',
                last_error: null,
                created_at: null,
              },
            ],
          }}
        />
      </MemoryRouter>,
    );

    const link = screen.getByRole('link', { name: 'External → Account 2' });
    expect(link).toHaveAttribute('href', `/explorer/tx/${txid}`);
    const row = link.closest('tr');
    expect(row).not.toBeNull();
    expect(within(row!).getByText('unknown')).toBeInTheDocument();
    expect(within(row!).getByText('ironwood')).toBeInTheDocument();
    expect(within(row!).getByText('Block 121')).toBeInTheDocument();
  });

  it('renders every affected development account', () => {
    const txid = 'ac'.repeat(32);
    renderWithProviders(
      <MemoryRouter>
        <ActivityList
          activity={{
            isPending: false,
            isError: false,
            error: null,
            data: [
              {
                id: 'wallet-incoming-multi',
                kind: 'transaction',
                direction: 'incoming',
                from_account: null,
                to_account: null,
                from_accounts: [],
                to_accounts: [2, 3],
                to_address: null,
                source_pool: null,
                destination_pool: 'ironwood',
                amount_zatoshi: 300_000_000n,
                txid,
                block_hash: 'cd'.repeat(32),
                mined_height: 121,
                status: 'confirmed',
                last_error: null,
                created_at: null,
              },
            ],
          }}
        />
      </MemoryRouter>,
    );

    expect(screen.getByRole('link', { name: 'External → Accounts 2, 3' })).toHaveAttribute(
      'href',
      `/explorer/tx/${txid}`,
    );
  });
});

describe('ActivityList multi-pool transactions', () => {
  it('uses the known pool arrays when no singular pool is available', () => {
    const txid = 'bd'.repeat(32);
    renderWithProviders(
      <MemoryRouter>
        <ActivityList
          activity={{
            isPending: false,
            isError: false,
            error: null,
            data: [
              {
                id: 'wallet-multi-pool',
                kind: 'transaction',
                direction: 'internal',
                from_account: 1,
                to_account: 2,
                to_address: null,
                source_pool: null,
                destination_pool: null,
                source_pools: ['ironwood'],
                destination_pools: ['ironwood', 'transparent'],
                amount_zatoshi: 100_000_000n,
                txid,
                block_hash: 'cd'.repeat(32),
                mined_height: 121,
                status: 'confirmed',
                last_error: null,
                created_at: null,
              },
            ],
          }}
        />
      </MemoryRouter>,
    );

    const row = screen.getByRole('link', { name: 'Account 1 → Account 2' }).closest('tr');
    expect(row).not.toBeNull();
    expect(within(row!).getByText('ironwood')).toBeInTheDocument();
    expect(within(row!).getByText('ironwood, transparent')).toBeInTheDocument();
    expect(within(row!).queryByText('unknown')).not.toBeInTheDocument();
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

describe('ActivityList clock', () => {
  it('updates idle ages beside unknown timestamps without fetching and clears its timer on unmount', async () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date('2026-10-15T12:00:00Z'));
    const fetch = vi.spyOn(globalThis, 'fetch');
    try {
      const { container, unmount } = renderWithProviders(
        <MemoryRouter>
          <ActivityList
            activity={{
              isPending: false,
              isError: false,
              error: null,
              data: [
                {
                  id: 'idle',
                  kind: 'send',
                  from_account: 1,
                  to_account: 2,
                  to_address: null,
                  source_pool: 'ironwood',
                  destination_pool: 'ironwood',
                  amount_zatoshi: 100_000n,
                  txid: '',
                  block_hash: null,
                  status: 'preparing',
                  last_error: null,
                  created_at: '2026-10-15 12:00:00',
                },
                ...[121, null].map((height) => ({
                  id: `unknown-${height}`,
                  kind: 'transaction' as const,
                  direction: 'incoming' as const,
                  from_account: null,
                  to_account: 2,
                  to_address: null,
                  source_pool: null,
                  destination_pool: 'ironwood' as const,
                  amount_zatoshi: 100_000n,
                  txid: '',
                  block_hash: null,
                  mined_height: height,
                  status: 'confirmed' as const,
                  last_error: null,
                  created_at: null,
                })),
              ],
            }}
          />
        </MemoryRouter>,
      );
      const time = container.querySelector('time')!;
      const title = time.title;
      expect(time).toHaveTextContent('just now');
      expect(screen.getByText('Block 121')).toBeInTheDocument();
      expect(screen.getByLabelText('Time unknown')).toBeInTheDocument();
      expect(container.querySelectorAll('time')).toHaveLength(1);
      await act(() => vi.advanceTimersByTime(10_000));
      expect(time).toHaveTextContent('10 seconds ago');
      await act(() => vi.advanceTimersByTime(60_000));
      expect(time).toHaveTextContent('1 minute ago');
      expect(time).toHaveAttribute('datetime', '2026-10-15 12:00:00');
      expect(time.title).toBe(title);
      expect(screen.getByText('Block 121')).toBeInTheDocument();
      expect(screen.getByLabelText('Time unknown')).toBeInTheDocument();
      expect(container.querySelectorAll('time')).toHaveLength(1);
      expect(fetch).not.toHaveBeenCalled();
      unmount();
      // Flush unrelated provider cleanup before checking for a recurring timer.
      vi.runOnlyPendingTimers();
      expect(vi.getTimerCount()).toBe(0);
    } finally {
      vi.useRealTimers();
      fetch.mockRestore();
    }
  });
});

describe('ActivityList memos', () => {
  it('shows the text memos a payment carried', () => {
    renderWithProviders(
      <MemoryRouter>
        <ActivityList
          activity={{
            isPending: false,
            isError: false,
            error: null,
            data: [
              {
                id: 'memo-send',
                kind: 'send',
                from_account: 1,
                to_account: 2,
                to_address: null,
                source_pool: 'ironwood',
                destination_pool: 'ironwood',
                amount_zatoshi: 10_000_000n,
                txid: '0d'.repeat(32),
                block_hash: 'cd'.repeat(32),
                status: 'confirmed',
                last_error: null,
                created_at: '2026-10-20 12:00:00',
                memos: ['order 42', ''],
              },
              {
                id: 'plain-send',
                kind: 'send',
                from_account: 1,
                to_account: 3,
                to_address: null,
                source_pool: 'ironwood',
                destination_pool: 'ironwood',
                amount_zatoshi: 10_000_000n,
                txid: '0e'.repeat(32),
                block_hash: 'cd'.repeat(32),
                status: 'confirmed',
                last_error: null,
                created_at: '2026-10-20 12:01:00',
                memos: [],
              },
            ],
          }}
        />
      </MemoryRouter>,
    );

    const memoRow = screen.getByRole('link', { name: 'Account 1 → Account 2' }).closest('tr');
    expect(within(memoRow!).getByText('Memo: order 42 · (empty text)')).toBeInTheDocument();
    const plainRow = screen.getByRole('link', { name: 'Account 1 → Account 3' }).closest('tr');
    expect(within(plainRow!).queryByText(/Memo:/)).not.toBeInTheDocument();
  });
});

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
  it('updates idle ages without fetching and clears its timer on unmount', () => {
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
              ],
            }}
          />
        </MemoryRouter>,
      );
      const time = container.querySelector('time')!;
      const title = time.title;
      expect(time).toHaveTextContent('just now');
      act(() => vi.advanceTimersByTime(10_000));
      expect(time).toHaveTextContent('10 seconds ago');
      act(() => vi.advanceTimersByTime(60_000));
      expect(time).toHaveTextContent('1 minute ago');
      expect(time).toHaveAttribute('datetime', '2026-10-15 12:00:00');
      expect(time.title).toBe(title);
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

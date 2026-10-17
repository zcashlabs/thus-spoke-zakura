import { afterEach, describe, expect, it, vi } from 'vitest';
import { screen, within } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import { renderWithProviders, requestUrl } from '@/test/utils';
import { Shell } from './Shell';

afterEach(() => vi.restoreAllMocks());

describe('Shell status', () => {
  it('does not present an unsettled environment as healthy', async () => {
    vi.spyOn(globalThis, 'fetch').mockImplementation((input) => {
      const url = requestUrl(input);
      if (!url.includes('/status')) return Promise.reject(new Error(`unexpected request: ${url}`));
      return Promise.resolve(
        new Response(
          JSON.stringify({
            instance: 'default',
            node: { chain: 'test', blocks: 104, bestblockhash: 'ab'.repeat(32) },
            account_count: 5,
            auto_mine: true,
            network: 'Regtest',
            settled: false,
            settlement_blocker: 'wallet_behind',
            wallet_sync: {
              state: 'ready',
              fully_scanned_height: 100,
              observed_height: 104,
              last_success_at: 1700000000,
              error: null,
            },
          }),
          { status: 200, headers: { 'content-type': 'application/json' } },
        ),
      );
    });

    renderWithProviders(
      <MemoryRouter>
        <Shell>Content</Shell>
      </MemoryRouter>,
    );

    const sidebar = within(screen.getByRole('complementary'));
    expect(await sidebar.findByText('Wallet is catching up')).toBeInTheDocument();
    expect(sidebar.queryByText('All systems operational')).not.toBeInTheDocument();
  });
});

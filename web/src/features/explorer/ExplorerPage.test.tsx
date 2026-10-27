import { describe, expect, it, vi, afterEach } from 'vitest';
import { screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { MemoryRouter, Route, Routes } from 'react-router-dom';
import { renderWithProviders, requestUrl } from '@/test/utils';
import { WalletActionsContext } from '@/features/wallet/wallet-actions-context';
import { api, ApiError } from '@/lib/api';
import { ExplorerPage } from './ExplorerPage';

afterEach(() => {
  vi.restoreAllMocks();
});

const TXID = 'ab'.repeat(32);

function renderAt(path: string, transactions: string[] | Error = []) {
  vi.spyOn(globalThis, 'fetch').mockImplementation((input) =>
    Promise.resolve(
      new Response(
        JSON.stringify(
          requestUrl(input).includes('/mempool')
            ? transactions instanceof Error
              ? { error: { message: transactions.message, status: 500 } }
              : { transactions }
            : { blocks: [], next_before: null },
        ),
        {
          status:
            requestUrl(input).includes('/mempool') && transactions instanceof Error ? 500 : 200,
          headers: { 'content-type': 'application/json' },
        },
      ),
    ),
  );

  return renderWithProviders(
    <WalletActionsContext
      value={{ openMine: vi.fn(), openSend: vi.fn(), openFaucet: vi.fn(), openRequest: vi.fn() }}
    >
      <MemoryRouter initialEntries={[path]}>
        <Routes>
          <Route path="/explorer/*" element={<ExplorerPage />} />
        </Routes>
      </MemoryRouter>
    </WalletActionsContext>,
  );
}

describe('ExplorerPage', () => {
  it('shows pending transaction links before they enter a block', async () => {
    renderAt('/explorer', [TXID]);
    expect(await screen.findByRole('link', { name: TXID })).toHaveAttribute(
      'href',
      `/explorer/tx/${TXID}`,
    );
  });

  it('leaves out the pending panel when nothing is pending', async () => {
    renderAt('/explorer');
    const fetchSpy = vi.mocked(globalThis.fetch);
    expect(await screen.findByLabelText('Search the chain')).toBeInTheDocument();
    await waitFor(() =>
      expect(fetchSpy.mock.calls.some(([input]) => requestUrl(input).includes('/mempool'))).toBe(
        true,
      ),
    );
    expect(screen.queryByRole('heading', { name: 'Pending transactions' })).not.toBeInTheDocument();
  });

  it('reports a failed mempool read instead of claiming it is empty', async () => {
    renderAt('/explorer', new Error('Zakura unavailable'));
    expect(await screen.findByRole('alert')).toHaveTextContent(/unexpected error/i);
  });

  it('explains an unknown explorer path instead of rendering nothing', async () => {
    // `/explorer/*` swallows the app-level catch-all, so this page needs its
    // own fallback or the route is a silent dead end.
    renderAt('/explorer/does-not-exist');

    expect(await screen.findByRole('alert')).toHaveTextContent(/not part of the explorer/i);
    expect(screen.getByRole('link', { name: /back to blocks/i })).toBeInTheDocument();
  });

  it('still shows the block list at the explorer index', async () => {
    renderAt('/explorer');
    expect(await screen.findByLabelText('Search the chain')).toBeInTheDocument();
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
  });

  it('labels an older page with the first block shown', async () => {
    vi.spyOn(api, 'blocks').mockImplementation((options) =>
      Promise.resolve({
        blocks: (options?.before === undefined ? [60, 59] : [39, 38]).map((height) => ({
          hash: `${height}`.padStart(64, '0'),
          height,
          time: 1_760_000_000,
          size: 1000,
          nTx: 1,
          confirmations: 1,
          tx: [],
          valuePools: [],
        })),
        next_before: options?.before === undefined ? 39 : null,
      }),
    );
    renderAt('/explorer');

    expect(await screen.findByRole('link', { name: '60' })).toBeInTheDocument();
    await userEvent.click(screen.getByRole('button', { name: 'Older blocks' }));

    expect(await screen.findByText('First block #39')).toBeInTheDocument();
    expect(screen.getByRole('link', { name: '39' })).toBeInTheDocument();
  });

  it('does not claim a hash is absent when the server fails', async () => {
    vi.spyOn(api, 'search').mockRejectedValue(new ApiError(500, 'Zakura unavailable'));
    renderAt('/explorer');

    await userEvent.type(screen.getByLabelText('Search the chain'), 'f'.repeat(64));
    await userEvent.click(screen.getByRole('button', { name: 'Search' }));

    expect(await screen.findByRole('alert')).toHaveTextContent(/unexpected error/i);
    expect(screen.getByRole('alert')).not.toHaveTextContent(/no block or transaction/i);
  });

  it('shows a missing-hash message for a 404', async () => {
    vi.spyOn(api, 'search').mockRejectedValue(new ApiError(404, 'Not found'));
    renderAt('/explorer');

    await userEvent.type(screen.getByLabelText('Search the chain'), 'f'.repeat(64));
    await userEvent.click(screen.getByRole('button', { name: 'Search' }));

    expect(await screen.findByRole('alert')).toHaveTextContent(
      'No block or transaction on this chain has that hash.',
    );
  });

  it('reports a connection failure without claiming the hash is absent', async () => {
    vi.spyOn(api, 'search').mockRejectedValue(new TypeError('Failed to fetch'));
    renderAt('/explorer');

    await userEvent.type(screen.getByLabelText('Search the chain'), 'f'.repeat(64));
    await userEvent.click(screen.getByRole('button', { name: 'Search' }));

    expect(await screen.findByRole('alert')).toHaveTextContent(/check your connection/i);
    expect(screen.getByRole('alert')).not.toHaveTextContent(/no block or transaction/i);
  });

  it('does not label a valid P2SH address as P2PKH', async () => {
    const address = 't26YoyZ1iPgiMEWL4zGUm74eVWfhyDMXzY2';
    vi.spyOn(api, 'address').mockResolvedValue({ address, balance: { balance: 0, received: 0 } });
    renderAt(`/explorer/address/${address}`);

    expect(await screen.findByText(address)).toBeInTheDocument();
    expect(screen.getByText('Type').nextElementSibling).toHaveTextContent('Transparent');
    expect(screen.getByText('Type').nextElementSibling).not.toHaveTextContent('P2PKH');
  });
});

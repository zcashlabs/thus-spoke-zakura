import { afterEach, describe, expect, it, vi } from 'vitest';
import { act, cleanup, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { MemoryRouter } from 'react-router-dom';
import { WalletActionsContext } from '@/features/wallet/wallet-actions-context';
import { queryKeys } from '@/hooks/queries';
import { api, type MiningJob } from '@/lib/api';
import { renderWithWalletActions } from '@/test/utils';
import { MempoolPanel } from './MempoolPanel';

const TXID = 'cd'.repeat(32);

function renderPanel() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const openMine = vi.fn();
  const result = render(
    <QueryClientProvider client={client}>
      <WalletActionsContext
        value={{ openMine, openSend: vi.fn(), openFaucet: vi.fn(), openRequest: vi.fn() }}
      >
        <MemoryRouter>
          <MempoolPanel />
        </MemoryRouter>
      </WalletActionsContext>
    </QueryClientProvider>,
  );
  return { ...result, client, openMine };
}

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  vi.useRealTimers();
  sessionStorage.clear();
});

describe('MempoolPanel', () => {
  it('renders nothing while loading or when nothing is pending', async () => {
    let resolve!: (body: { transactions: string[] }) => void;
    const read = vi.spyOn(api, 'mempool').mockImplementation(
      () =>
        new Promise((done) => {
          resolve = done;
        }),
    );
    const { container } = renderPanel();
    expect(container).toBeEmptyDOMElement();
    await act(async () => {
      resolve({ transactions: [] });
      await Promise.resolve();
    });
    expect(read).toHaveBeenCalled();
    expect(container).toBeEmptyDOMElement();
  });

  it('polls an empty mempool for external broadcasts and stops after unmount', async () => {
    vi.useFakeTimers({ toFake: ['setInterval', 'clearInterval'] });
    let transactions: string[] = [];
    const read = vi
      .spyOn(api, 'mempool')
      .mockImplementation(() => Promise.resolve({ transactions }));
    const view = renderPanel();
    await waitFor(() => expect(read).toHaveBeenCalled());
    expect(view.container).toBeEmptyDOMElement();

    transactions = [TXID];
    await act(async () => {
      await vi.advanceTimersByTimeAsync(2_000);
    });
    expect(await screen.findByRole('link', { name: TXID })).toHaveAttribute(
      'href',
      `/explorer/tx/${TXID}`,
    );
    expect(screen.getByText('1 pending')).toBeInTheDocument();

    transactions = [];
    await act(async () => {
      await vi.advanceTimersByTimeAsync(2_000);
    });
    await waitFor(() => expect(view.container).toBeEmptyDOMElement());

    view.unmount();
    const reads = read.mock.calls.length;
    await vi.advanceTimersByTimeAsync(10_000);
    expect(read).toHaveBeenCalledTimes(reads);
  });

  it('hides stale results after a failed refresh and can retry', async () => {
    const read = vi.spyOn(api, 'mempool').mockResolvedValue({ transactions: [TXID] });
    const { client, container } = renderPanel();
    await screen.findByRole('link', { name: TXID });

    read.mockRejectedValue(new TypeError('Network unavailable'));
    await act(async () => {
      await client.invalidateQueries({ queryKey: queryKeys.mempool });
    });
    expect(await screen.findByRole('alert')).toHaveTextContent(
      'Unable to refresh pending transactions',
    );
    expect(screen.queryByRole('link', { name: TXID })).not.toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Mine 1 block' })).toBeDisabled();

    read.mockResolvedValue({ transactions: [] });
    await userEvent.click(screen.getByRole('button', { name: 'Retry' }));
    await waitFor(() => expect(container).toBeEmptyDOMElement());
  });

  it('uses the existing mining dialog with one block selected and submits only after confirmation', async () => {
    vi.spyOn(api, 'mempool').mockResolvedValue({ transactions: [TXID] });
    let job: MiningJob | null = null;
    vi.spyOn(api, 'miningJob').mockImplementation(() => Promise.resolve({ job }));
    const start = vi.spyOn(api, 'startMining').mockImplementation(() => {
      job = {
        id: '588e9911-20b6-4c4a-a9ae-5f6c1abae953',
        requested_blocks: 1,
        completed_blocks: 1,
        state: 'completed',
        error: null,
        progress_uncertain: false,
      };
      return Promise.resolve(job);
    });
    renderWithWalletActions(<MempoolPanel />);
    await screen.findByRole('link', { name: TXID });
    expect(start).not.toHaveBeenCalled();
    await userEvent.click(screen.getByRole('button', { name: 'Mine 1 block' }));
    const dialog = await screen.findByRole('dialog');
    expect(within(dialog).getByLabelText('Number of blocks')).toHaveValue('1');
    expect(start).not.toHaveBeenCalled();
    await userEvent.click(within(dialog).getByRole('button', { name: 'Mine blocks' }));
    expect(
      await within(dialog).findByText('Mining completed. Wallet synchronized.'),
    ).toBeInTheDocument();
    expect(start).toHaveBeenCalledExactlyOnceWith(1, expect.any(String));
  });
});

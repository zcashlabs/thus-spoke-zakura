import { afterEach, describe, expect, it, vi } from 'vitest';
import { act, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { createMemoryRouter, RouterProvider } from 'react-router-dom';
import { api, type Block } from '@/lib/api';
import { renderWithProviders } from '@/test/utils';
import { ExplorerPage } from './ExplorerPage';

afterEach(() => {
  vi.restoreAllMocks();
});

function block(height: number): Block {
  return {
    hash: `hash-${height}`,
    height,
    time: 1_296_688_602,
    size: 100,
    nTx: 1,
    confirmations: 105 - height,
    tx: [],
    valuePools: [],
  };
}

function renderExplorer(path = '/explorer') {
  const router = createMemoryRouter([{ path: '/explorer/*', element: <ExplorerPage /> }], {
    initialEntries: [path],
  });
  const view = renderWithProviders(<RouterProvider router={router} />);
  return { router, ...view };
}

function mockBlocks() {
  vi.spyOn(api, 'block').mockImplementation((id) => Promise.resolve(block(Number(id))));
  vi.spyOn(api, 'status').mockRejectedValue(new Error('status not needed for pagination'));
  return vi.spyOn(api, 'blocks').mockImplementation((options) => {
    const end = Math.min(options?.before ?? 104, 104);
    const start = Math.max(0, end - 19);
    return Promise.resolve({
      blocks: Array.from({ length: end - start + 1 }, (_, index) => block(end - index)),
      next_before: start === 0 ? null : start - 1,
    });
  });
}

async function expectFirstBlock(height: number) {
  await waitFor(() => {
    const firstRow = screen.getAllByRole('row')[1];
    if (!firstRow) throw new Error('No block rows');
    expect(within(firstRow).getByRole('link')).toHaveTextContent(new RegExp(`^${height}$`));
  });
}

describe('BlockList pagination', () => {
  it('restores the older page after one Back from a hash-cell navigation', async () => {
    mockBlocks();
    const user = userEvent.setup();
    const { router } = renderExplorer();
    await expectFirstBlock(104);
    await user.click(screen.getByRole('button', { name: 'Older blocks' }));
    await expectFirstBlock(84);
    await user.click(screen.getByTitle('hash-84'));
    expect(await screen.findByRole('heading', { name: 'Block #84' })).toBeInTheDocument();
    await act(() => router.navigate(-1));
    await expectFirstBlock(84);
    expect(router.state.location.search).toBe('?before=84');
    expect(screen.getByRole('button', { name: 'Back to tip' })).toBeInTheDocument();
  });

  it('retains a copied cursor across a fresh mount and clears only that cursor at the tip', async () => {
    const requests = mockBlocks();
    const user = userEvent.setup();
    const first = renderExplorer('/explorer?filter=kept');
    await expectFirstBlock(104);
    await user.click(screen.getByRole('button', { name: 'Older blocks' }));
    await expectFirstBlock(84);
    await user.click(screen.getByRole('button', { name: 'Older blocks' }));
    await expectFirstBlock(64);
    const copiedUrl = first.router.state.location.pathname + first.router.state.location.search;
    expect(copiedUrl).toBe('/explorer?filter=kept&before=64');
    first.unmount();

    const { router } = renderExplorer(copiedUrl);
    await expectFirstBlock(64);
    expect(requests).toHaveBeenLastCalledWith({ before: 64 });
    await user.click(screen.getByRole('button', { name: 'Back to tip' }));
    await expectFirstBlock(104);
    expect(router.state.location.search).toBe('?filter=kept');
    expect(requests).toHaveBeenLastCalledWith({});
    expect(screen.queryByRole('button', { name: 'Back to tip' })).not.toBeInTheDocument();
    await act(() => router.navigate(-1));
    await expectFirstBlock(64);
    await act(() => router.navigate(1));
    await expectFirstBlock(104);
  });

  it('tracks successive pages through Back and Forward', async () => {
    mockBlocks();
    const user = userEvent.setup();
    const { router } = renderExplorer();
    await expectFirstBlock(104);
    await user.click(screen.getByRole('button', { name: 'Older blocks' }));
    await expectFirstBlock(84);
    await user.click(screen.getByRole('button', { name: 'Older blocks' }));
    await expectFirstBlock(64);
    await act(() => router.navigate(-1));
    await expectFirstBlock(84);
    await act(() => router.navigate(-1));
    await expectFirstBlock(104);
    await act(() => router.navigate(1));
    await expectFirstBlock(84);
  });

  it('accepts the genesis cursor and disables paging beyond it', async () => {
    const requests = mockBlocks();
    renderExplorer('/explorer?before=0');
    await expectFirstBlock(0);
    expect(requests).toHaveBeenLastCalledWith({ before: 0 });
    expect(screen.getByRole('button', { name: 'Older blocks' })).toBeDisabled();
    expect(screen.getByRole('button', { name: 'Back to tip' })).toBeInTheDocument();
  });

  it.each(['84', '00084', '9007199254740991'])('accepts the decimal cursor %j', async (cursor) => {
    const requests = mockBlocks();
    renderExplorer(`/explorer?before=${cursor}`);
    await expectFirstBlock(Math.min(Number(cursor), 104));
    expect(requests).toHaveBeenLastCalledWith({ before: Number(cursor) });
  });

  it.each(['', '-1', '1.5', '84abc', '1e2', '0x54', 'Infinity', 'NaN', ' 84', '9007199254740992'])(
    'does not send the invalid cursor %j to the API',
    async (cursor) => {
      const requests = mockBlocks();
      renderExplorer(`/explorer?before=${encodeURIComponent(cursor)}`);
      await expectFirstBlock(104);
      expect(requests).toHaveBeenLastCalledWith({});
      expect(screen.queryByRole('button', { name: 'Back to tip' })).not.toBeInTheDocument();
    },
  );
});

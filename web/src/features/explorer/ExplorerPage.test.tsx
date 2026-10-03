import { describe, expect, it, vi, afterEach } from 'vitest';
import { screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { MemoryRouter, Route, Routes } from 'react-router-dom';
import { renderWithProviders } from '@/test/utils';
import { api, ApiError } from '@/lib/api';
import { ExplorerPage } from './ExplorerPage';

afterEach(() => {
  vi.restoreAllMocks();
});

function renderAt(path: string) {
  vi.spyOn(globalThis, 'fetch').mockImplementation(() =>
    Promise.resolve(
      new Response(JSON.stringify({ blocks: [], next_before: null }), {
        status: 200,
        headers: { 'content-type': 'application/json' },
      }),
    ),
  );

  return renderWithProviders(
    <MemoryRouter initialEntries={[path]}>
      <Routes>
        <Route path="/explorer/*" element={<ExplorerPage />} />
      </Routes>
    </MemoryRouter>,
  );
}

describe('ExplorerPage', () => {
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

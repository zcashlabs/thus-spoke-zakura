import { afterEach, describe, expect, it, vi } from 'vitest';
import { act, screen, waitFor } from '@testing-library/react';
import { renderWithProviders, testAccounts } from '@/test/utils';
import userEvent from '@testing-library/user-event';
import { Link, MemoryRouter, Route, Routes, useLocation } from 'react-router-dom';
import { api, type Account } from '@/lib/api';
import { SearchBar } from './SearchBar';

afterEach(() => vi.restoreAllMocks());

function Location() {
  return <output aria-label="Current page">{useLocation().pathname}</output>;
}

function renderSearch() {
  renderWithProviders(
    <MemoryRouter initialEntries={['/explorer']}>
      <Location />
      <Link to="/network">Network</Link>
      <Routes>
        <Route path="/explorer/*" element={<SearchBar />} />
        <Route path="/network" element={<p>Network page</p>} />
      </Routes>
    </MemoryRouter>,
  );
}

describe('Explorer search navigation', () => {
  it('waits for the initial accounts query before resolving a known unified address', async () => {
    let resolve!: (value: Account[]) => void;
    const lookup = vi.spyOn(api, 'accounts').mockReturnValue(
      new Promise((done) => {
        resolve = done;
      }),
    );
    const publicSearch = vi.spyOn(api, 'search');
    renderSearch();
    await userEvent.type(
      screen.getByLabelText('Search the chain'),
      testAccounts[1]!.unified_address,
    );
    await userEvent.click(screen.getByRole('button', { name: 'Search' }));
    expect(screen.getByRole('button', { name: 'Searching…' })).toBeInTheDocument();
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
    await act(async () => {
      resolve(testAccounts);
      await Promise.resolve();
    });
    await waitFor(() =>
      expect(screen.getByRole('status', { name: 'Current page' })).toHaveTextContent(
        '/wallet/accounts/2',
      ),
    );
    expect(lookup).toHaveBeenCalledTimes(1);
    expect(publicSearch).not.toHaveBeenCalled();
  });

  it.each([
    'uregtest1unknownaccount',
    'uregtest1account2extra',
    'UREGTEST1ACCOUNT2',
    'uregtest1treasury',
  ])('does not invent public history or an account match for %s', async (address) => {
    vi.spyOn(api, 'accounts').mockResolvedValue([
      ...testAccounts,
      { ...testAccounts[0]!, id: 6, unified_address: 'uregtest1treasury' },
    ]);
    const publicSearch = vi.spyOn(api, 'search');
    const publicAddress = vi.spyOn(api, 'address');
    renderSearch();
    await userEvent.type(screen.getByLabelText('Search the chain'), address);
    await userEvent.click(screen.getByRole('button', { name: 'Search' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('no public chain history');
    expect(publicSearch).not.toHaveBeenCalled();
    expect(publicAddress).not.toHaveBeenCalled();
  });

  it('distinguishes a failed account lookup from an unknown address and allows another search', async () => {
    const lookup = vi.spyOn(api, 'accounts').mockRejectedValue(new TypeError('Offline'));
    renderSearch();
    await userEvent.type(
      screen.getByLabelText('Search the chain'),
      testAccounts[1]!.unified_address,
    );
    await userEvent.click(screen.getByRole('button', { name: 'Search' }));
    expect(await screen.findByRole('alert')).toHaveTextContent(
      'Could not load development accounts',
    );
    expect(screen.getByRole('alert')).not.toHaveTextContent('no public chain history');
    lookup.mockResolvedValue(testAccounts);
    await userEvent.click(screen.getByRole('button', { name: 'Search' }));
    await waitFor(() =>
      expect(screen.getByRole('status', { name: 'Current page' })).toHaveTextContent(
        '/wallet/accounts/2',
      ),
    );
  });

  it.each(['away', 'transparent', 'hash', 'invalid'])(
    'ignores a late unified-address lookup after %s',
    async (next) => {
      let resolve!: (value: Account[]) => void;
      vi.spyOn(api, 'accounts').mockReturnValue(
        new Promise((done) => {
          resolve = done;
        }),
      );
      vi.spyOn(api, 'search').mockResolvedValue({ type: 'transaction' });
      renderSearch();
      const input = screen.getByLabelText('Search the chain');
      await userEvent.type(input, testAccounts[1]!.unified_address);
      await userEvent.click(screen.getByRole('button', { name: 'Search' }));
      if (next === 'away') await userEvent.click(screen.getByRole('link', { name: 'Network' }));
      else {
        await userEvent.clear(input);
        await userEvent.type(
          input,
          next === 'transparent'
            ? 'tmUnknownTransparentAddress00000000'
            : next === 'hash'
              ? 'a'.repeat(64)
              : 'invalid',
        );
        await userEvent.click(screen.getByRole('button', { name: /Search/ }));
      }
      await act(async () => {
        resolve(testAccounts);
        await Promise.resolve();
      });
      expect(screen.getByRole('status', { name: 'Current page' })).not.toHaveTextContent(
        '/wallet/accounts/2',
      );
      if (next === 'invalid')
        expect(screen.getByRole('alert')).toHaveTextContent('Not a recognised');
    },
  );
  it('does not leave a newer page when a search resolves late', async () => {
    let resolve!: (value: { type: 'block' }) => void;
    vi.spyOn(api, 'search').mockReturnValue(new Promise((done) => (resolve = done)));
    renderSearch();
    await userEvent.type(screen.getByLabelText('Search the chain'), 'a'.repeat(64));
    await userEvent.click(screen.getByRole('button', { name: 'Search' }));
    await userEvent.click(screen.getByRole('link', { name: 'Network' }));
    resolve({ type: 'block' });
    await waitFor(() => expect(screen.getByText('Network page')).toBeInTheDocument());
    expect(screen.getByRole('status', { name: 'Current page' })).toHaveTextContent('/network');
  });

  it('uses the newest submitted hash when responses arrive out of order', async () => {
    let resolveFirst!: (value: { type: 'block' }) => void;
    let resolveSecond!: (value: { type: 'transaction' }) => void;
    vi.spyOn(api, 'search')
      .mockReturnValueOnce(new Promise((done) => (resolveFirst = done)))
      .mockReturnValueOnce(new Promise((done) => (resolveSecond = done)));
    renderSearch();
    const input = screen.getByLabelText('Search the chain');
    await userEvent.type(input, 'a'.repeat(64));
    await userEvent.click(screen.getByRole('button', { name: 'Search' }));
    await userEvent.clear(input);
    await userEvent.type(input, 'b'.repeat(64));
    await userEvent.click(screen.getByRole('button', { name: /Search/ }));
    resolveFirst({ type: 'block' });
    resolveSecond({ type: 'transaction' });
    await waitFor(() =>
      expect(screen.getByRole('status', { name: 'Current page' })).toHaveTextContent(
        `/explorer/tx/${'b'.repeat(64)}`,
      ),
    );
  });
});

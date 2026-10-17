import { afterEach, describe, expect, it, vi } from 'vitest';
import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { Link, MemoryRouter, Route, Routes, useLocation } from 'react-router-dom';
import { api } from '@/lib/api';
import { SearchBar } from './SearchBar';

afterEach(() => vi.restoreAllMocks());

function Location() {
  return <output aria-label="Current page">{useLocation().pathname}</output>;
}

function renderSearch() {
  render(
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

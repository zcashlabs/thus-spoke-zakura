import { afterEach, describe, expect, it, vi } from 'vitest';
import { act, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { MemoryRouter, Route, Routes, useLocation } from 'react-router-dom';
import { renderWithProviders, testAccounts } from '@/test/utils';
import { api, ApiError, type Account } from '@/lib/api';
import { SearchBar } from './SearchBar';

afterEach(() => vi.restoreAllMocks());

const HASH = 'a'.repeat(64);

function Location() {
  return <output aria-label="Current page">{useLocation().pathname}</output>;
}

function renderSearch() {
  renderWithProviders(
    <MemoryRouter initialEntries={['/explorer']}>
      <Location />
      <Routes>
        <Route path="/explorer/*" element={<SearchBar />} />
      </Routes>
    </MemoryRouter>,
  );
}

describe('Explorer search errors', () => {
  it('describes the input with the error and clears it when the query changes', async () => {
    renderSearch();
    const user = userEvent.setup();
    const input = screen.getByLabelText('Search the chain');

    await user.type(input, 'not a search');
    await user.click(screen.getByRole('button', { name: 'Search' }));

    const alert = await screen.findByRole('alert');
    expect(input).toHaveAttribute('aria-invalid', 'true');
    expect(input).toHaveAccessibleDescription(alert.textContent ?? '');

    await user.type(input, 'x');

    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
    expect(input).toHaveAttribute('aria-invalid', 'false');
    expect(input).not.toHaveAccessibleDescription();
  });

  it('ignores a pending search once the query has changed', async () => {
    let reject!: (error: unknown) => void;
    vi.spyOn(api, 'search').mockReturnValue(
      new Promise((_, fail) => {
        reject = fail;
      }),
    );
    renderSearch();
    const user = userEvent.setup();
    const input = screen.getByLabelText('Search the chain');

    await user.type(input, HASH);
    await user.click(screen.getByRole('button', { name: 'Search' }));
    expect(screen.getByRole('button', { name: 'Searching…' })).toBeInTheDocument();
    await user.type(input, 'b');
    await act(async () => {
      reject(new ApiError(404, 'not found'));
      await Promise.resolve();
    });

    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
    expect(input).toHaveAttribute('aria-invalid', 'false');
    expect(screen.getByRole('button', { name: 'Search' })).toBeInTheDocument();
    expect(screen.getByLabelText('Current page')).toHaveTextContent(/^\/explorer$/);
  });

  it('ignores a pending search result that would navigate for the old query', async () => {
    let resolve!: (value: { type: 'block' | 'transaction' }) => void;
    vi.spyOn(api, 'search').mockReturnValue(
      new Promise((done) => {
        resolve = done;
      }),
    );
    renderSearch();
    const user = userEvent.setup();
    const input = screen.getByLabelText('Search the chain');

    await user.type(input, HASH);
    await user.click(screen.getByRole('button', { name: 'Search' }));
    await user.clear(input);
    await act(async () => {
      resolve({ type: 'block' });
      await Promise.resolve();
    });

    expect(screen.getByLabelText('Current page')).toHaveTextContent(/^\/explorer$/);
  });

  it('ignores a pending account lookup once the query has changed', async () => {
    let resolve!: (value: Account[]) => void;
    vi.spyOn(api, 'accounts').mockReturnValue(
      new Promise((done) => {
        resolve = done;
      }),
    );
    renderSearch();
    const user = userEvent.setup();
    const input = screen.getByLabelText('Search the chain');

    await user.type(input, testAccounts[1]!.unified_address);
    await user.click(screen.getByRole('button', { name: 'Search' }));
    expect(screen.getByRole('button', { name: 'Searching…' })).toBeInTheDocument();
    await user.type(input, 'x');
    await act(async () => {
      resolve(testAccounts);
      await Promise.resolve();
    });

    expect(screen.getByLabelText('Current page')).toHaveTextContent(/^\/explorer$/);
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Search' })).toBeInTheDocument();
  });
});

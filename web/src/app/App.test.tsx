import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { act, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { StrictMode } from 'react';
import { MemoryRouter, useLocation } from 'react-router-dom';
import { renderWithProviders, testAccounts } from '@/test/utils';
import { WalletActionsProvider } from '@/features/wallet/WalletActions';
import { api, type Block } from '@/lib/api';
import { App } from './App';

type SearchResult = Awaited<ReturnType<typeof api.search>>;

const FIRST = 'f'.repeat(64);
const SECOND = 'e'.repeat(64);
const block: Block = {
  hash: 'b'.repeat(64),
  height: 106,
  time: 1_296_688_602,
  size: 9_500,
  nTx: 0,
  confirmations: 1,
  tx: [],
  valuePools: [],
};

class FakeEventSource extends EventTarget {
  close() {}
}

function LocationProbe() {
  return <output aria-label="Current route">{useLocation().pathname}</output>;
}

const route = () => screen.getByLabelText('Current route');

function renderApp() {
  renderWithProviders(
    <StrictMode>
      <MemoryRouter initialEntries={['/explorer']}>
        <WalletActionsProvider>
          <LocationProbe />
          <App />
        </WalletActionsProvider>
      </MemoryRouter>
    </StrictMode>,
  );
}

/** Delays the next search response until the test resolves it. */
function deferSearch() {
  let resolve!: (result: SearchResult) => void;
  const response = new Promise<SearchResult>((res) => {
    resolve = res;
  });
  vi.mocked(api.search).mockReturnValueOnce(response);
  return (result: SearchResult) =>
    act(async () => {
      resolve(result);
      await Promise.resolve();
    });
}

async function search(query: string) {
  await userEvent.type(screen.getByLabelText('Search the chain'), query);
  await userEvent.click(screen.getByRole('button', { name: 'Search' }));
}

const clickLink = (name: string) => userEvent.click(screen.getByRole('link', { name }));

beforeEach(() => {
  localStorage.clear();
  sessionStorage.clear();
  vi.stubGlobal('EventSource', FakeEventSource);
  vi.spyOn(api, 'status').mockResolvedValue({
    instance: 'default',
    node: null,
    account_count: 5,
    auto_mine: true,
    network: 'Regtest',
  });
  vi.spyOn(api, 'accounts').mockResolvedValue(testAccounts);
  vi.spyOn(api, 'activity').mockResolvedValue([]);
  vi.spyOn(api, 'miningJob').mockResolvedValue({ job: null });
  vi.spyOn(api, 'blocks').mockResolvedValue({ blocks: [block], next_before: null });
  vi.spyOn(api, 'block').mockResolvedValue(block);
  vi.spyOn(api, 'search');
});

afterEach(() => {
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

describe('Explorer search navigation', () => {
  it('opens the result of the current search', async () => {
    const resolveSearch = deferSearch();
    renderApp();
    await search(FIRST);
    expect(screen.getByRole('button', { name: 'Searching…' })).toBeDisabled();
    await resolveSearch({ type: 'block' });
    expect(route()).toHaveTextContent(`/explorer/block/${FIRST}`);
  });

  it('ignores a result that arrives after leaving Explorer', async () => {
    const resolveSearch = deferSearch();
    renderApp();
    await search(FIRST);
    await clickLink('Wallet');
    await resolveSearch({ type: 'block' });
    expect(route()).toHaveTextContent('/wallet');
  });

  it('ignores a result that arrives after returning to Explorer', async () => {
    const resolveSearch = deferSearch();
    renderApp();
    await search(FIRST);
    await clickLink('Wallet');
    await clickLink('Explorer');
    await resolveSearch({ type: 'block' });
    expect(route()).toHaveTextContent(/^\/explorer$/);
  });

  it('keeps a block opened while the search was pending', async () => {
    const resolveSearch = deferSearch();
    renderApp();
    await search(FIRST);
    await clickLink('106');
    await resolveSearch({ type: 'block' });
    expect(route()).toHaveTextContent('/explorer/block/106');
  });

  it('lets a newer search replace a pending one', async () => {
    const resolveFirst = deferSearch();
    const resolveSecond = deferSearch();
    renderApp();
    await search(FIRST);
    // Reopening Explorer from the nav stays on the same page but frees the search bar.
    await clickLink('Explorer');
    await search(SECOND);
    await resolveFirst({ type: 'block' });
    expect(route()).toHaveTextContent(/^\/explorer$/);
    await resolveSecond({ type: 'transaction' });
    expect(route()).toHaveTextContent(`/explorer/tx/${SECOND}`);
  });
});

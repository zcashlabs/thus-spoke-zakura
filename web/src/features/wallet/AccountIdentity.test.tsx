import { afterEach, describe, expect, it, vi } from 'vitest';
import { act, render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { MemoryRouter } from 'react-router-dom';
import { App } from '@/app/App';
import { ToastProvider } from '@/components/ui/Toast';
import { api, type Account } from '@/lib/api';
import { queryKeys } from '@/hooks/queries';
import { testAccounts } from '@/test/utils';

vi.mock('@/hooks/useServerEvents', () => ({ useServerEvents: () => undefined }));
vi.mock('./wallet-actions-context', () => ({
  useWalletActions: () => ({
    openFaucet: vi.fn(),
    openMine: vi.fn(),
    openSend: vi.fn(),
    openRequest: vi.fn(),
  }),
}));
vi.mock('./PaymentUpdates', () => ({ PaymentUpdates: () => null }));

const accounts: Account[] = testAccounts.map((account) => ({
  ...account,
  transparent_address: `tm${'a'.repeat(28)}${account.id}`,
  unified_full_viewing_key: `uviewtest1completeviewingkey${account.id}`,
}));
const treasury: Account = {
  ...accounts[0]!,
  id: 6,
  name: 'Treasury',
  unified_address: 'uregtest1treasury',
  transparent_address: 'tmTreasury000000000000000000000000',
};
const TXID = 'ab'.repeat(32);

function renderAt(path: string, payload = accounts, load?: () => Promise<Account[]>) {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false, staleTime: Infinity } },
  });
  vi.spyOn(api, 'accounts').mockImplementation(load ?? (() => Promise.resolve(payload)));
  vi.spyOn(api, 'status').mockImplementation(() => new Promise(() => {}));
  vi.spyOn(api, 'activity').mockResolvedValue([]);
  vi.spyOn(api, 'blocks').mockResolvedValue({ blocks: [], next_before: null });
  const view = render(
    <QueryClientProvider client={client}>
      <ToastProvider>
        <MemoryRouter initialEntries={[path]}>
          <App />
        </MemoryRouter>
      </ToastProvider>
    </QueryClientProvider>,
  );
  return { ...view, client };
}

afterEach(() => vi.restoreAllMocks());

describe('Development account identity', () => {
  it('opens the same stable account destination from the Wallet card and copies full identities', async () => {
    const user = userEvent.setup();
    const write = vi.spyOn(navigator.clipboard, 'writeText').mockResolvedValue();
    renderAt('/wallet');
    await user.click(await screen.findByRole('link', { name: 'View Account 2 details' }));
    expect(await screen.findByRole('heading', { level: 1, name: 'Account 2' })).toBeInTheDocument();
    expect(screen.getAllByRole('heading', { level: 1 })).toHaveLength(1);
    for (const [label, value] of [
      ['unified address', accounts[1]!.unified_address],
      ['transparent address', accounts[1]!.transparent_address],
      ['unified full viewing key', accounts[1]!.unified_full_viewing_key!],
    ] as const) {
      expect(screen.getByText(value, { exact: true })).toBeInTheDocument();
      await user.click(screen.getByRole('button', { name: `Copy Account 2 ${label}` }));
      expect(write).toHaveBeenLastCalledWith(value);
    }
  });

  it('resolves a known unified address to account detail instead of public address history', async () => {
    const address = vi.spyOn(api, 'address');
    const search = vi.spyOn(api, 'search');
    renderAt('/explorer');
    await userEvent.type(
      screen.getByLabelText('Search the chain'),
      `  ${accounts[1]!.unified_address}  `,
    );
    await userEvent.click(screen.getByRole('button', { name: 'Search' }));
    expect(await screen.findByRole('heading', { name: 'Account 2', level: 1 })).toBeInTheDocument();
    expect(address).not.toHaveBeenCalled();
    expect(search).not.toHaveBeenCalled();
  });

  it('links a known transparent address to its account without exposing a key in Explorer', async () => {
    vi.spyOn(api, 'address').mockResolvedValue({
      address: accounts[1]!.transparent_address,
      balance: { balance: 100, received: 100 },
    });
    renderAt(`/explorer/address/${accounts[1]!.transparent_address}`);
    const link = await screen.findByRole('link', { name: 'Account 2' });
    expect(link).toHaveAttribute('href', '/wallet/accounts/2');
    expect(document.body).not.toHaveTextContent(accounts[1]!.unified_full_viewing_key!);
    await userEvent.click(link);
    expect(await screen.findByRole('heading', { level: 1, name: 'Account 2' })).toBeInTheDocument();
  });

  it('annotates known transaction inputs and outputs while preserving public-address links', async () => {
    vi.spyOn(api, 'transaction').mockResolvedValue({
      txid: TXID,
      vShieldedSpend: [],
      vShieldedOutput: [],
      vin: [
        {
          txid: 'cd'.repeat(32),
          vout: 0,
          valueZat: 200,
          scriptPubKey: { addresses: [accounts[0]!.transparent_address] },
        },
      ],
      vout: [
        { n: 0, valueZat: 100, scriptPubKey: { addresses: [accounts[1]!.transparent_address] } },
      ],
    });
    renderAt(`/explorer/tx/${TXID}`);
    expect(await screen.findByRole('link', { name: 'Account 1' })).toHaveAttribute(
      'href',
      '/wallet/accounts/1',
    );
    expect(screen.getByRole('link', { name: 'Account 2' })).toHaveAttribute(
      'href',
      '/wallet/accounts/2',
    );
    expect(screen.getByRole('link', { name: accounts[1]!.transparent_address })).toHaveAttribute(
      'href',
      `/explorer/address/${accounts[1]!.transparent_address}`,
    );
    expect(document.body).not.toHaveTextContent(accounts[1]!.unified_full_viewing_key!);
    await userEvent.click(screen.getByRole('link', { name: 'Account 2' }));
    expect(await screen.findByRole('heading', { name: 'Account 2', level: 1 })).toBeInTheDocument();
  });

  it('omits a missing viewing key supplied by an older server', async () => {
    const older = accounts.map((account) => {
      const older = { ...account };
      delete older.unified_full_viewing_key;
      return older;
    });
    renderAt('/wallet/accounts/2', older);
    expect(
      await screen.findByText(accounts[1]!.transparent_address, { exact: true }),
    ).toBeInTheDocument();
    expect(screen.queryByText('Unified full viewing key')).not.toBeInTheDocument();
    expect(screen.queryByRole('button', { name: /viewing key/i })).not.toBeInTheDocument();
  });

  it('keeps treasury out of Wallet cards and totals even if the response contains it', async () => {
    renderAt('/wallet', [...accounts, { ...treasury, ironwood_zatoshi: 900_000_000n }]);
    expect(await screen.findByText('Account 1')).toBeInTheDocument();
    expect(screen.queryByText('Treasury')).not.toBeInTheDocument();
    expect(screen.queryByText('14 ZEC')).not.toBeInTheDocument();
    expect(document.body).not.toHaveTextContent(treasury.unified_address);
  });

  it.each(['6', '01', '1junk', '99'])('rejects a non-user account route %s', async (id) => {
    renderAt(`/wallet/accounts/${id}`, [...accounts, treasury]);
    expect(await screen.findByRole('alert')).toHaveTextContent('Account not found');
    expect(document.body).not.toHaveTextContent(treasury.unified_address);
  });

  it('derives account details from the shared query after a refresh', async () => {
    const { client } = renderAt('/wallet/accounts/2');
    await screen.findByText(accounts[1]!.unified_address, { exact: true });
    await act(() =>
      client.setQueryData(
        queryKeys.accounts,
        accounts.map((account) =>
          account.id === 2 ? { ...account, unified_address: 'uregtest1updatedaccount2' } : account,
        ),
      ),
    );
    expect(
      await screen.findByText('uregtest1updatedaccount2', { exact: true }),
    ).toBeInTheDocument();
    expect(
      screen.queryByText(accounts[1]!.unified_address, { exact: true }),
    ).not.toBeInTheDocument();
  });

  it('shows loading until account lookup succeeds, not a premature not-found error', async () => {
    let resolve!: (value: Account[]) => void;
    renderAt(
      '/wallet/accounts/2',
      accounts,
      () =>
        new Promise((done) => {
          resolve = done;
        }),
    );
    expect(screen.getByRole('status')).toHaveTextContent('Loading account');
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
    await act(async () => {
      resolve(accounts);
      await Promise.resolve();
    });
    expect(
      await screen.findByText(accounts[1]!.transparent_address, { exact: true }),
    ).toBeInTheDocument();
  });

  it('reports initial account failures and retries without claiming the account is missing', async () => {
    renderAt('/wallet/accounts/2', accounts, () => Promise.reject(new TypeError('Offline')));
    expect(await screen.findByRole('alert')).not.toHaveTextContent('Account not found');
    vi.mocked(api.accounts).mockResolvedValue(accounts);
    await userEvent.click(screen.getByRole('button', { name: 'Retry' }));
    expect(
      await screen.findByText(accounts[1]!.transparent_address, { exact: true }),
    ).toBeInTheDocument();
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
  });

  it('keeps cached identity with a warning after refresh failure, then recovers', async () => {
    const { client } = renderAt('/wallet/accounts/2');
    await screen.findByText(accounts[1]!.transparent_address, { exact: true });
    vi.mocked(api.accounts).mockRejectedValue(new TypeError('Offline'));
    await act(async () => {
      await client.invalidateQueries({ queryKey: queryKeys.accounts });
    });
    expect(await screen.findByRole('alert')).toHaveTextContent('Showing the last loaded values');
    expect(screen.getByText(accounts[1]!.transparent_address, { exact: true })).toBeInTheDocument();
    vi.mocked(api.accounts).mockResolvedValue(accounts);
    await userEvent.click(screen.getByRole('button', { name: 'Retry' }));
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
  });

  it('reports a successful lookup without that account as not found', async () => {
    renderAt('/wallet/accounts/2', [accounts[0]!]);
    expect(await screen.findByRole('alert')).toHaveTextContent('Account not found');
  });

  it('keeps public transaction details usable when account enrichment fails', async () => {
    vi.spyOn(api, 'transaction').mockResolvedValue({
      txid: TXID,
      vin: [],
      vout: [
        { n: 0, valueZat: 100, scriptPubKey: { addresses: [accounts[1]!.transparent_address] } },
      ],
      vShieldedSpend: [],
      vShieldedOutput: [],
    });
    renderAt(`/explorer/tx/${TXID}`, accounts, () => Promise.reject(new TypeError('Offline')));
    expect(
      await screen.findByRole('link', { name: accounts[1]!.transparent_address }),
    ).toHaveAttribute('href', `/explorer/address/${accounts[1]!.transparent_address}`);
    expect(screen.queryByRole('link', { name: 'Account 2' })).not.toBeInTheDocument();
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
  });

  it.each([
    accounts[1]!.transparent_address,
    treasury.transparent_address,
    'tmUnknownTransparentAddress00000000',
  ])('preserves raw address navigation for %s', async (address) => {
    vi.spyOn(api, 'address').mockResolvedValue({ address, balance: { balance: 0, received: 0 } });
    renderAt(`/explorer/address/${address}`, [...accounts, treasury]);
    expect(await screen.findByText(address, { exact: true })).toBeInTheDocument();
    expect(screen.queryByRole('link', { name: 'Account 6' })).not.toBeInTheDocument();
    if (address !== accounts[1]!.transparent_address)
      expect(screen.queryByRole('link', { name: /^Account \d$/ })).not.toBeInTheDocument();
  });
});

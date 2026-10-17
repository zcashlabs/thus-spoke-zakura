import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { act, render, screen } from '@testing-library/react';
import type { ReactNode } from 'react';
import { MemoryRouter, Route, Routes } from 'react-router-dom';
import { AddressDetail } from '@/features/explorer/AddressDetail';
import { BlockDetail } from '@/features/explorer/BlockDetail';
import { TransactionDetail } from '@/features/explorer/TransactionDetail';
import { AppProviders } from '@/app/providers';
import { renderWithProviders, requestUrl } from '@/test/utils';
import { useAccounts } from './queries';
import { useServerEvents } from './useServerEvents';

const TXID = 'd9'.repeat(32);
const ADDRESS = 'tmAg2wTARgKBHA9mFouqR727cU98MJmfjSq';

type Listener = (event: MessageEvent<string>) => void;

class FakeEventSource {
  static current: FakeEventSource | undefined;
  onopen: ((event: Event) => void) | null = null;
  private listeners = new Set<Listener>();

  constructor() {
    FakeEventSource.current = this;
  }

  addEventListener(_type: string, listener: Listener) {
    this.listeners.add(listener);
  }

  removeEventListener(_type: string, listener: Listener) {
    this.listeners.delete(listener);
  }

  close() {}

  emit(topic: string) {
    for (const listener of this.listeners) {
      listener(new MessageEvent('update', { data: topic }));
    }
  }
}

function json(body: unknown): Promise<Response> {
  return Promise.resolve(
    new Response(JSON.stringify(body), {
      status: 200,
      headers: { 'content-type': 'application/json' },
    }),
  );
}

let routes: Record<string, () => unknown>;

beforeEach(() => {
  vi.stubGlobal('EventSource', FakeEventSource);
  vi.spyOn(globalThis, 'fetch').mockImplementation((input) => {
    const url = requestUrl(input);
    for (const [path, body] of Object.entries(routes)) {
      if (url.includes(path)) return json(body());
    }
    return Promise.reject(new Error(`unexpected request ${url}`));
  });
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

function Live({ children }: { children: ReactNode }) {
  useServerEvents();
  return children;
}

function renderPage(path: string, pattern: string, page: ReactNode) {
  renderWithProviders(
    <Live>
      <MemoryRouter initialEntries={[path]}>
        <Routes>
          <Route path={pattern} element={page} />
        </Routes>
      </MemoryRouter>
    </Live>,
  );
}

function requestsTo(path: string): number {
  return vi.mocked(fetch).mock.calls.filter(([input]) => requestUrl(input).includes(path)).length;
}

describe('useServerEvents', () => {
  it('refreshes a pending initial balance after SSE opens', async () => {
    const account = {
      id: 1,
      name: 'Account 1',
      unified_address: 'uregtest1example',
      transparent_address: ADDRESS,
      transparent_zatoshi: 0,
      ironwood_zatoshi: 100_000_000,
    };
    const oldResponse = await json([account]);
    let releaseOldResponse!: (response: Response) => void;
    const pendingResponse = new Promise<Response>((resolve) => {
      releaseOldResponse = resolve;
    });
    routes = { '/accounts': () => [{ ...account, ironwood_zatoshi: 200_000_000 }] };
    vi.mocked(fetch).mockImplementationOnce(() => pendingResponse);

    function Balance() {
      const accounts = useAccounts();
      return <p>Balance: {accounts.data?.[0]?.ironwood_zatoshi.toString()}</p>;
    }
    render(
      <AppProviders>
        <Live>
          <Balance />
        </Live>
      </AppProviders>,
    );
    expect(requestsTo('/accounts')).toBe(1);

    act(() => FakeEventSource.current?.onopen?.(new Event('open')));
    // The held snapshot predates subscription and must not satisfy the fresh read.
    await act(async () => {
      releaseOldResponse(oldResponse);
      await pendingResponse;
    });

    expect(await screen.findByText('Balance: 200000000')).toBeInTheDocument();
    expect(requestsTo('/accounts')).toBe(2);
  });

  it.each(['resync', 'unknown-topic'])('invalidates all mapped queries on %s', (topic) => {
    const client = new QueryClient();
    const invalidate = vi.spyOn(client, 'invalidateQueries');
    render(
      <QueryClientProvider client={client}>
        <Live>{null}</Live>
      </QueryClientProvider>,
    );
    FakeEventSource.current?.emit(topic);
    expect(invalidate.mock.calls).toEqual([
      [{ queryKey: ['accounts'] }],
      [{ queryKey: ['activity'] }],
      [{ queryKey: ['send-quote'] }],
      [{ queryKey: ['status'] }],
      [{ queryKey: ['blocks'] }],
      [{ queryKey: ['block'] }],
      [{ queryKey: ['transaction'] }],
      [{ queryKey: ['address'] }],
      [{ queryKey: ['status'] }],
      [{ queryKey: ['mining'] }],
    ]);
  });

  it('invalidates only mining on a mining update', () => {
    const client = new QueryClient();
    const invalidate = vi.spyOn(client, 'invalidateQueries');
    render(
      <QueryClientProvider client={client}>
        <Live>{null}</Live>
      </QueryClientProvider>,
    );
    FakeEventSource.current?.emit('mining');
    expect(invalidate.mock.calls).toEqual([[{ queryKey: ['mining'] }]]);
  });

  it('refreshes an open transaction page on a new block', async () => {
    let confirmations = 1;
    routes = { [`/transactions/${TXID}`]: () => ({ txid: TXID, height: 106, confirmations }) };
    renderPage(`/explorer/tx/${TXID}`, '/explorer/tx/:txid', <TransactionDetail />);
    expect(await screen.findByText('1 conf')).toBeInTheDocument();

    confirmations = 4;
    FakeEventSource.current?.emit('chain');

    expect(screen.queryByText('Loading transaction…')).not.toBeInTheDocument();
    expect(await screen.findByText('4 conf')).toBeInTheDocument();
  });

  it('refreshes an open address page on a new block', async () => {
    let balance = 0;
    routes = {
      [`/addresses/${ADDRESS}`]: () => ({
        address: ADDRESS,
        balance: { balance, received: balance },
      }),
    };
    renderPage(`/explorer/address/${ADDRESS}`, '/explorer/address/:address', <AddressDetail />);
    expect(await screen.findAllByText('0 ZEC')).not.toHaveLength(0);

    balance = 150_000_000;
    FakeEventSource.current?.emit('chain');

    expect(await screen.findAllByText('1.5 ZEC')).not.toHaveLength(0);
  });

  it('refreshes an open block page on a new block', async () => {
    let confirmations = 1;
    routes = {
      '/status': () => ({
        instance: 'test',
        account_count: 5,
        auto_mine: true,
        network: 'regtest',
        node: { blocks: 106 + confirmations - 1, bestblockhash: 'ab'.repeat(32) },
      }),
      '/blocks/106': () => ({
        hash: 'cd'.repeat(32),
        height: 106,
        time: 1_700_000_000,
        size: 1_000,
        nTx: 0,
        confirmations,
      }),
    };
    renderPage('/explorer/block/106', '/explorer/block/:id', <BlockDetail />);
    expect(await screen.findByText('1 conf')).toBeInTheDocument();

    confirmations = 4;
    FakeEventSource.current?.emit('chain');

    expect(await screen.findByText('4 conf')).toBeInTheDocument();
  });

  it('leaves detail pages alone on wallet and sync updates', async () => {
    routes = { [`/transactions/${TXID}`]: () => ({ txid: TXID, height: 106, confirmations: 1 }) };
    renderPage(`/explorer/tx/${TXID}`, '/explorer/tx/:txid', <TransactionDetail />);
    await screen.findByText('1 conf');

    FakeEventSource.current?.emit('wallet');
    FakeEventSource.current?.emit('sync');
    await new Promise((resolve) => setTimeout(resolve, 50));

    expect(requestsTo(`/transactions/${TXID}`)).toBe(1);
  });
});

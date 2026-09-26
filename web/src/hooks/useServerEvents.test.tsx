import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { screen } from '@testing-library/react';
import type { ReactNode } from 'react';
import { MemoryRouter, Route, Routes } from 'react-router-dom';
import { AddressDetail } from '@/features/explorer/AddressDetail';
import { BlockDetail } from '@/features/explorer/BlockDetail';
import { TransactionDetail } from '@/features/explorer/TransactionDetail';
import { renderWithProviders, requestUrl } from '@/test/utils';
import { useServerEvents } from './useServerEvents';

const TXID = 'd9'.repeat(32);
const ADDRESS = 'tmAg2wTARgKBHA9mFouqR727cU98MJmfjSq';

type Listener = (event: MessageEvent<string>) => void;

class FakeEventSource {
  static current: FakeEventSource | undefined;
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

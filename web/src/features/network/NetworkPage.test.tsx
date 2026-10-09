import { describe, expect, it, vi, afterEach } from 'vitest';
import { render, screen } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import type { ReactNode } from 'react';
import { MemoryRouter } from 'react-router-dom';
import { renderWithProviders, requestUrl } from '@/test/utils';
import { useServerEvents } from '@/hooks/useServerEvents';
import { NetworkPage } from './NetworkPage';

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

function Live({ children }: { children: ReactNode }) {
  useServerEvents();
  return children;
}

const ENDPOINTS = {
  dashboard: 'http://127.0.0.1:8080',
  zakura_rpc: 'http://127.0.0.1:18232',
  lightwalletd: 'http://127.0.0.1:9067',
  p2p: '127.0.0.1:18233',
};

function json(body: unknown): Promise<Response> {
  return Promise.resolve(
    new Response(JSON.stringify(body), {
      status: 200,
      headers: { 'content-type': 'application/json' },
    }),
  );
}

function renderNetwork(status: Record<string, unknown>) {
  vi.spyOn(globalThis, 'fetch').mockImplementation((input) => {
    const url = requestUrl(input);
    if (url.includes('/status')) return json(status);
    return Promise.reject(new Error(`unexpected request: ${url}`));
  });

  return renderWithProviders(
    <MemoryRouter>
      <NetworkPage />
    </MemoryRouter>,
  );
}

const BASE = {
  instance: 'dash',
  node: { chain: 'test', blocks: 104, bestblockhash: 'ab'.repeat(32), verificationprogress: 1 },
  account_count: 5,
  auto_mine: true,
  network: 'Regtest',
};

afterEach(() => {
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

describe('NetworkPage', () => {
  it('lists every runtime endpoint the server publishes', async () => {
    renderNetwork({ ...BASE, endpoints: ENDPOINTS });

    expect(await screen.findByText('Runtime endpoints')).toBeInTheDocument();
    for (const value of Object.values(ENDPOINTS)) {
      expect(screen.getByText(value)).toBeInTheDocument();
    }
  });

  it('gives each endpoint its own copy control', async () => {
    renderNetwork({ ...BASE, endpoints: ENDPOINTS });

    expect(await screen.findByLabelText('Copy Zakura RPC endpoint')).toBeInTheDocument();
    expect(screen.getByLabelText('Copy lightwalletd endpoint')).toBeInTheDocument();
    expect(screen.getByLabelText('Copy P2P endpoint')).toBeInTheDocument();
    expect(screen.getByLabelText('Copy Dashboard endpoint')).toBeInTheDocument();
  });

  it('still renders against a server too old to send endpoints', async () => {
    renderNetwork(BASE);

    // The rest of the page must survive the field being absent entirely.
    expect(await screen.findByText('Node details')).toBeInTheDocument();
    expect(screen.queryByText('Runtime endpoints')).not.toBeInTheDocument();
  });

  it('claims all systems operational only when the node is reachable and the wallet is ready', async () => {
    renderNetwork({
      ...BASE,
      wallet_sync: {
        state: 'ready',
        fully_scanned_height: 104,
        observed_height: 104,
        last_success_at: 1700000000,
        error: null,
      },
    });

    expect(await screen.findByText('All systems operational')).toBeInTheDocument();
    expect(screen.getByText('Online')).toBeInTheDocument();
    expect(screen.getByText('Wallet ready')).toBeInTheDocument();
  });

  it('does not claim all systems operational when a reachable node has a syncing wallet', async () => {
    renderNetwork({
      ...BASE,
      wallet_sync: {
        state: 'syncing',
        fully_scanned_height: 50,
        observed_height: 104,
        last_success_at: null,
        error: null,
      },
    });

    expect(await screen.findByText('Wallet is syncing')).toBeInTheDocument();
    expect(screen.getByText('Online')).toBeInTheDocument();
    expect(screen.getByText('Wallet syncing')).toBeInTheDocument();
    expect(screen.queryByText('All systems operational')).not.toBeInTheDocument();
  });

  it('does not claim all systems operational when a reachable node has a failed wallet', async () => {
    renderNetwork({
      ...BASE,
      wallet_sync: {
        state: 'error',
        fully_scanned_height: null,
        observed_height: null,
        last_success_at: 1700000000,
        error: 'connection reset',
      },
    });

    expect(await screen.findByText('Wallet synchronization failed')).toBeInTheDocument();
    expect(screen.getByText('Online')).toBeInTheDocument();
    expect(screen.getByText('Wallet error')).toBeInTheDocument();
    expect(screen.queryByText('All systems operational')).not.toBeInTheDocument();
  });

  it('shows a neutral fallback when the server omits wallet_sync entirely', async () => {
    renderNetwork(BASE);

    expect(await screen.findByText('Wallet status unavailable')).toBeInTheDocument();
    expect(screen.getByText('Node connected')).toBeInTheDocument();
    expect(screen.queryByText('All systems operational')).not.toBeInTheDocument();
  });

  it('keeps the node offline even if the last known wallet state was ready', async () => {
    renderNetwork({
      ...BASE,
      node: null,
      wallet_sync: {
        state: 'ready',
        fully_scanned_height: 104,
        observed_height: 104,
        last_success_at: 1700000000,
        error: null,
      },
    });

    expect(await screen.findByText('Waiting for Zakura')).toBeInTheDocument();
    expect(screen.getByText('Offline')).toBeInTheDocument();
    expect(screen.getByText('Wallet ready')).toBeInTheDocument();
  });

  it('reflects a wallet transition from syncing to ready after a sync event refetch', async () => {
    let state: 'syncing' | 'ready' = 'syncing';
    vi.spyOn(globalThis, 'fetch').mockImplementation((input) => {
      const url = requestUrl(input);
      if (!url.includes('/status')) return Promise.reject(new Error(`unexpected request: ${url}`));
      return json({
        ...BASE,
        wallet_sync: {
          state,
          fully_scanned_height: state === 'ready' ? 104 : 50,
          observed_height: 104,
          last_success_at: state === 'ready' ? 1700000000 : null,
          error: null,
        },
      });
    });

    const client = new QueryClient({
      defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
    });
    vi.stubGlobal('EventSource', FakeEventSource);
    render(
      <QueryClientProvider client={client}>
        <Live>
          <MemoryRouter>
            <NetworkPage />
          </MemoryRouter>
        </Live>
      </QueryClientProvider>,
    );

    expect(await screen.findByText('Wallet is syncing')).toBeInTheDocument();

    state = 'ready';
    FakeEventSource.current?.emit('sync');

    expect(await screen.findByText('All systems operational')).toBeInTheDocument();
  });
});

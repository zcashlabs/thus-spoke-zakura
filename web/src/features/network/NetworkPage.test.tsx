import { describe, expect, it, vi, afterEach } from 'vitest';
import { screen } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import { renderWithProviders, requestUrl } from '@/test/utils';
import { NetworkPage } from './NetworkPage';

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

  it('identifies a locally built node', async () => {
    renderNetwork({ ...BASE, node_mode: 'local_binary' });
    expect(await screen.findByText('Local Zakura executable')).toBeInTheDocument();
  });

  it('explains local-only attachment and omits an unknown P2P endpoint', async () => {
    renderNetwork({ ...BASE, node_mode: 'external_rpc', endpoints: { ...ENDPOINTS, p2p: '' } });
    expect(await screen.findByText('Self-managed local Zakura')).toBeInTheDocument();
    expect(screen.getByText(/Internet and LAN nodes are not supported/)).toBeInTheDocument();
    expect(screen.getByText(/preserved when you detach/)).toBeInTheDocument();
    expect(screen.queryByLabelText('Copy P2P endpoint')).not.toBeInTheDocument();
    expect(screen.queryByText(/starts from block 0/)).not.toBeInTheDocument();
  });
});

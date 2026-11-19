import { afterEach, describe, expect, it, vi } from 'vitest';
import { screen } from '@testing-library/react';
import { MemoryRouter, Route, Routes } from 'react-router-dom';
import { renderWithProviders, requestUrl } from '@/test/utils';
import { AddressDetail } from './AddressDetail';

function renderAddress(balance: Record<string, number>) {
  vi.spyOn(globalThis, 'fetch').mockImplementation((input) => {
    const url = requestUrl(input);
    const body = url.includes('/addresses/') ? { address: 'tmAddress', balance } : [];
    return Promise.resolve(
      new Response(JSON.stringify(body), {
        status: 200,
        headers: { 'content-type': 'application/json' },
      }),
    );
  });

  renderWithProviders(
    <MemoryRouter initialEntries={['/explorer/address/tmAddress']}>
      <Routes>
        <Route path="/explorer/address/:address" element={<AddressDetail />} />
      </Routes>
    </MemoryRouter>,
  );
}

afterEach(() => {
  vi.restoreAllMocks();
});

describe('AddressDetail', () => {
  it('formats bigint totals from the API schema', async () => {
    renderAddress({ balance: 100_000_000, received: 250_000_000 });

    expect(await screen.findByText('1 ZEC')).toBeInTheDocument();
    expect(screen.getByText('2.5 ZEC')).toBeInTheDocument();
    expect(screen.getByText('1.5 ZEC')).toBeInTheDocument();
  });

  it('floors total sent at zero when the address received more than it spent', async () => {
    renderAddress({ balance: 5, received: 3 });

    expect(await screen.findByText('0.00000005 ZEC')).toBeInTheDocument();
    expect(screen.getByText('Total sent').parentElement).toHaveTextContent('0 ZEC');
  });
});

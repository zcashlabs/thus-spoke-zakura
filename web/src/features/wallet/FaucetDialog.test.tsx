import { beforeEach, afterEach, describe, expect, it, vi, type MockInstance } from 'vitest';
import { useState } from 'react';
import { QueryClient } from '@tanstack/react-query';
import { act, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import {
  renderWithWalletActions as renderWithProviders,
  requestUrl,
  testAccounts,
} from '@/test/utils';
import { queryKeys, useAccounts, useActivity } from '@/hooks/queries';
import { api } from '@/lib/api';
import { FaucetDialog } from './FaucetDialog';

const activity = {
  id: 'payment',
  kind: 'faucet',
  from_account: null,
  to_account: 1,
  source_pool: 'ironwood',
  destination_pool: 'ironwood',
  amount_zatoshi: 100_000_000,
  txid: 'f'.repeat(64),
  block_hash: 'b'.repeat(64),
  status: 'confirmed',
  created_at: '2026-09-22 10:00:00',
  last_error: null,
};
const posts = (mock: MockInstance<typeof fetch>) =>
  mock.mock.calls.filter(([, init]) => init?.method === 'POST');
const key = (call: Parameters<typeof fetch> | undefined) =>
  (
    JSON.parse(typeof call?.[1]?.body === 'string' ? call[1].body : '{}') as {
      idempotency_key: string;
    }
  ).idempotency_key;
beforeEach(() => {
  sessionStorage.clear();
  localStorage.clear();
});
afterEach(() => {
  vi.restoreAllMocks();
});

describe('FaucetDialog', () => {
  async function chooseExternalAddress() {
    Object.assign(Element.prototype, {
      hasPointerCapture: () => false,
      releasePointerCapture: () => undefined,
      scrollIntoView: () => undefined,
    });
    await userEvent.click(screen.getByLabelText('Destination'));
    await userEvent.click(await screen.findByRole('option', { name: 'External address' }));
  }

  it('resumes an external faucet payment with the same key after a lost response', async () => {
    const address = 'tmAg2wTARgKBHA9mFouqR727cU98MJmfjSq';
    let posted = false;
    const result = (status: string) => ({
      address,
      amount_zatoshi: 100_000_000,
      txid: 'f'.repeat(64),
      block_hash: status === 'confirmed' ? 'b'.repeat(64) : null,
      status,
      last_error: null,
    });
    const fetchSpy = vi.spyOn(globalThis, 'fetch').mockImplementation((input, init) => {
      const url = requestUrl(input);
      if (url.endsWith('/zip321/parse'))
        return Promise.resolve(
          Response.json({
            address,
            destination_pool: 'transparent',
            to_account: null,
            amount_zatoshi: null,
            memo: null,
          }),
        );
      if (url.endsWith('/faucet/address') && init?.method === 'POST') {
        if (!posted) {
          posted = true;
          return Promise.reject(new TypeError('response lost'));
        }
        return Promise.resolve(Response.json(result('confirmed')));
      }
      if (url.includes('/faucet/address?idempotency_key='))
        return Promise.resolve(Response.json(result(posted ? 'broadcast' : 'preparing')));
      if (url.endsWith('/accounts') || url.includes('/activity?'))
        return Promise.resolve(Response.json([]));
      if (url.endsWith('/mining/jobs')) return Promise.resolve(Response.json({ job: null }));
      throw new Error(`Unexpected request: ${url}`);
    });
    const addressPosts = () =>
      fetchSpy.mock.calls.filter(
        ([input, init]) => requestUrl(input).endsWith('/faucet/address') && init?.method === 'POST',
      );
    const first = renderWithProviders(
      <FaucetDialog open onOpenChange={vi.fn()} accounts={testAccounts} />,
    );
    await chooseExternalAddress();
    await userEvent.type(screen.getByLabelText('Destination address'), address);
    await userEvent.click(screen.getByRole('button', { name: 'Add funds' }));
    await waitFor(() => expect(addressPosts()).toHaveLength(1));
    expect(
      await screen.findByText(/A saved payment has these details \(broadcast\)/),
    ).toBeInTheDocument();
    const firstKey = key(addressPosts()[0]);
    expect(firstKey).toHaveLength(36);
    first.unmount();
    sessionStorage.clear();

    renderWithProviders(<FaucetDialog open onOpenChange={vi.fn()} accounts={testAccounts} />);
    await chooseExternalAddress();
    await userEvent.type(screen.getByLabelText('Destination address'), address);
    await userEvent.click(await screen.findByRole('button', { name: 'Resume payment' }));
    await waitFor(() => expect(addressPosts()).toHaveLength(2));
    expect(key(addressPosts()[1])).toBe(firstKey);
    expect(
      fetchSpy.mock.calls.some(([input]) =>
        requestUrl(input).includes(`/faucet/address?idempotency_key=${firstKey}`),
      ),
    ).toBe(true);
  });

  it('submits a unified Regtest address to the address faucet', async () => {
    const address = 'uregtest1externalrecipient';
    const fetchSpy = vi.spyOn(globalThis, 'fetch').mockImplementation((input, init) => {
      const url = requestUrl(input);
      if (url.endsWith('/zip321/parse'))
        return Promise.resolve(
          Response.json({
            address,
            destination_pool: 'ironwood',
            to_account: null,
            amount_zatoshi: null,
            memo: null,
          }),
        );
      if (url.endsWith('/faucet/address') && init?.method === 'POST')
        return Promise.resolve(
          Response.json({
            address,
            amount_zatoshi: 100_000_000,
            txid: 'f'.repeat(64),
            block_hash: 'b'.repeat(64),
            status: 'confirmed',
            last_error: null,
          }),
        );
      if (url.endsWith('/accounts') || url.includes('/activity?'))
        return Promise.resolve(Response.json([]));
      if (url.endsWith('/mining/jobs')) return Promise.resolve(Response.json({ job: null }));
      throw new Error(`Unexpected request: ${url}`);
    });
    renderWithProviders(<FaucetDialog open onOpenChange={vi.fn()} accounts={testAccounts} />);
    await chooseExternalAddress();
    await userEvent.type(screen.getByLabelText('Destination address'), address);
    await userEvent.click(screen.getByRole('button', { name: 'Add funds' }));
    await waitFor(() =>
      expect(
        fetchSpy.mock.calls.some(
          ([input, init]) =>
            requestUrl(input).endsWith('/faucet/address') && init?.method === 'POST',
        ),
      ).toBe(true),
    );
    const call = fetchSpy.mock.calls.find(
      ([input, init]) => requestUrl(input).endsWith('/faucet/address') && init?.method === 'POST',
    );
    expect(call).toBeDefined();
    const request = JSON.parse(typeof call?.[1]?.body === 'string' ? call[1].body : '{}') as {
      address: string;
      amount_zatoshi: number;
      idempotency_key: string;
    };
    expect(request).toMatchObject({ address, amount_zatoshi: 100_000_000 });
    expect(request.idempotency_key).toHaveLength(36);
  });

  it.each(['invalid', 't1wrongnetworkaddress'])(
    'rejects %s before funding an address',
    async (address) => {
      const fetchSpy = vi.spyOn(globalThis, 'fetch').mockImplementation((input) =>
        requestUrl(input).endsWith('/zip321/parse')
          ? Promise.resolve(
              new Response(
                JSON.stringify({
                  error: { message: 'Not a valid Regtest address', status: 400 },
                }),
                { status: 400 },
              ),
            )
          : Promise.resolve(Response.json([])),
      );
      renderWithProviders(<FaucetDialog open onOpenChange={vi.fn()} accounts={testAccounts} />);
      await chooseExternalAddress();
      await userEvent.type(screen.getByLabelText('Destination address'), address);
      await userEvent.click(screen.getByRole('button', { name: 'Add funds' }));
      expect(await screen.findByText('Not a valid Regtest address')).toBeInTheDocument();
      expect(
        fetchSpy.mock.calls.some(([input]) => requestUrl(input).endsWith('/faucet/address')),
      ).toBe(false);
    },
  );
  it('routes a development-account address through the account faucet', async () => {
    const address = 'tmAg2wTARgKBHA9mFouqR727cU98MJmfjSq';
    const fetchSpy = vi.spyOn(globalThis, 'fetch').mockImplementation((input) =>
      requestUrl(input).endsWith('/zip321/parse')
        ? Promise.resolve(
            Response.json({
              address,
              destination_pool: 'transparent',
              to_account: 2,
              amount_zatoshi: null,
              memo: null,
            }),
          )
        : Promise.resolve(Response.json([])),
    );
    renderWithProviders(<FaucetDialog open onOpenChange={vi.fn()} accounts={testAccounts} />);
    await chooseExternalAddress();
    await userEvent.type(screen.getByLabelText('Destination address'), address);
    await userEvent.click(screen.getByRole('button', { name: 'Add funds' }));
    expect(
      await screen.findByText("This is Account 2's address. Choose Account 2 as the destination."),
    ).toBeInTheDocument();
    expect(
      fetchSpy.mock.calls.some(([input]) => requestUrl(input).endsWith('/faucet/address')),
    ).toBe(false);
  });
  it('exposes modal semantics the previous hand-rolled dialog lacked', () => {
    renderWithProviders(<FaucetDialog open onOpenChange={vi.fn()} accounts={testAccounts} />);

    const dialog = screen.getByRole('dialog');
    // The dialog is named and described by real elements, so screen readers
    // announce it. The previous implementation was an unlabelled <div>.
    const labelId = dialog.getAttribute('aria-labelledby');
    const descriptionId = dialog.getAttribute('aria-describedby');
    expect(labelId).toBeTruthy();
    expect(document.getElementById(labelId!)).toHaveTextContent('Fund an account or address');
    expect(descriptionId).toBeTruthy();
    expect(document.getElementById(descriptionId!)).toBeInTheDocument();
    expect(screen.getByRole('heading', { name: 'Fund an account or address' })).toBeInTheDocument();
  });

  it('moves focus into the dialog on open', () => {
    renderWithProviders(<FaucetDialog open onOpenChange={vi.fn()} accounts={testAccounts} />);
    expect(screen.getByRole('dialog').contains(document.activeElement)).toBe(true);
  });

  it('closes on Escape and restores focus to its opener', async () => {
    function Harness() {
      const [open, setOpen] = useState(false);
      return (
        <>
          <button onClick={() => setOpen(true)}>Open faucet</button>
          {open && <FaucetDialog open onOpenChange={setOpen} accounts={testAccounts} />}
        </>
      );
    }
    renderWithProviders(<Harness />);
    const opener = screen.getByRole('button', { name: 'Open faucet' });
    await userEvent.click(opener);
    expect(screen.getByRole('dialog')).toBeInTheDocument();
    await userEvent.keyboard('{Escape}');
    await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument());
    await waitFor(() => expect(opener).toHaveFocus());
  });

  it('defaults to 1 ZEC and keeps an edited draft when its response confirms', async () => {
    let confirmed = false;
    let confirm!: (response: Response) => void;
    const response = new Promise<Response>((resolve) => {
      confirm = resolve;
    });
    vi.spyOn(globalThis, 'fetch').mockImplementation((_, init) =>
      init?.method === 'POST'
        ? response
        : Promise.resolve(new Response(JSON.stringify(confirmed ? [activity] : []))),
    );
    const close = vi.fn();
    renderWithProviders(<FaucetDialog open onOpenChange={close} accounts={testAccounts} />);
    expect(screen.getByLabelText('Amount (ZEC)')).toHaveValue('1');
    await userEvent.click(screen.getByRole('button', { name: 'Add funds' }));
    expect(await screen.findAllByText('Submitting payment…')).toHaveLength(1);
    await userEvent.clear(screen.getByLabelText('Amount (ZEC)'));
    await userEvent.type(screen.getByLabelText('Amount (ZEC)'), '2');
    await act(() => {
      confirmed = true;
      confirm(new Response(JSON.stringify(activity)));
      return response;
    });
    await waitFor(() =>
      expect(screen.getByText('Payments').closest('button')).toHaveAttribute(
        'aria-label',
        expect.stringMatching(/1 confirmed/),
      ),
    );
    expect(close).not.toHaveBeenCalled();
    expect(screen.getByLabelText('Amount (ZEC)')).toHaveValue('2');
    expect(screen.getAllByText('Funded Account 1')).toHaveLength(1);
  });

  it.each([false, true])(
    'reuses the idempotency key in a new tab after the response is lost (durable only: %s)',
    async (durableOnly) => {
      let lost = true;
      const fetchSpy = vi.spyOn(globalThis, 'fetch').mockImplementation((input) => {
        if (requestUrl(input).includes('/activity?')) return Promise.resolve(new Response('[]'));
        if (lost && requestUrl(input).endsWith('/faucet')) {
          lost = false;
          return Promise.reject(new TypeError('response lost'));
        }
        return Promise.resolve(new Response(JSON.stringify(activity)));
      });
      const first = renderWithProviders(
        <FaucetDialog open onOpenChange={vi.fn()} accounts={testAccounts} />,
      );

      await userEvent.click(screen.getByRole('button', { name: 'Add funds' }));
      await waitFor(() => expect(posts(fetchSpy)).toHaveLength(1));

      first.unmount();
      sessionStorage.clear();
      if (durableOnly) localStorage.removeItem('ths:faucet:1:ironwood:100000000');
      const second = renderWithProviders(
        <FaucetDialog open onOpenChange={vi.fn()} accounts={testAccounts} />,
      );
      await userEvent.click(await screen.findByRole('button', { name: 'Resume payment' }));
      await waitFor(() => expect(posts(fetchSpy)).toHaveLength(2));

      expect(key(posts(fetchSpy)[0])).toBe(key(posts(fetchSpy)[1]));
      await waitFor(() =>
        expect(localStorage.getItem('ths:faucet:1:ironwood:100000000')).toBeNull(),
      );

      second.unmount();
      renderWithProviders(<FaucetDialog open onOpenChange={vi.fn()} accounts={testAccounts} />);
      await userEvent.click(screen.getByRole('button', { name: 'Add funds' }));
      await waitFor(() => expect(posts(fetchSpy)).toHaveLength(3));
      expect(key(posts(fetchSpy)[2])).not.toBe(key(posts(fetchSpy)[1]));
      fetchSpy.mockRestore();
    },
  );

  it('keeps pending status outside the dialog and resumes with its original key', async () => {
    const onOpenChange = vi.fn();
    let payment = { ...activity, status: 'broadcast', block_hash: null as string | null };
    const fetchSpy = vi
      .spyOn(globalThis, 'fetch')
      .mockImplementation((input) =>
        Promise.resolve(
          new Response(
            JSON.stringify(requestUrl(input).includes('/activity?') ? [payment] : payment),
          ),
        ),
      );
    const first = renderWithProviders(
      <FaucetDialog open onOpenChange={onOpenChange} accounts={testAccounts} />,
    );

    await userEvent.click(screen.getByRole('button', { name: 'Add funds' }));
    await waitFor(() =>
      expect(screen.getByText('Payments').closest('button')).toHaveAttribute(
        'aria-label',
        expect.stringMatching(/1 unresolved/),
      ),
    );
    expect(onOpenChange).toHaveBeenCalledWith(false);
    onOpenChange.mockClear();
    await userEvent.click(screen.getByRole('button', { name: 'Resume payment' }));
    await waitFor(() => expect(posts(fetchSpy)).toHaveLength(2));
    expect(key(posts(fetchSpy)[1])).toBe(key(posts(fetchSpy)[0]));
    onOpenChange.mockClear();
    const amount = screen.getByLabelText('Amount (ZEC)');
    await userEvent.clear(amount);
    await userEvent.type(amount, '2');
    expect(screen.getByRole('button', { name: 'Add funds' })).toBeInTheDocument();
    expect(screen.getByText('Payments').closest('button')).toHaveAttribute(
      'aria-label',
      expect.stringMatching(/1 unresolved/),
    );

    first.unmount();
    sessionStorage.clear();
    const second = renderWithProviders(
      <FaucetDialog open onOpenChange={onOpenChange} accounts={testAccounts} />,
    );
    await waitFor(() =>
      expect(screen.getByText('Payments').closest('button')).toHaveAttribute(
        'aria-label',
        expect.stringMatching(/1 unresolved/),
      ),
    );
    expect(fetchSpy.mock.calls.map(([input]) => requestUrl(input))).toContain(
      `/api/v1/activity?idempotency_key=${key(posts(fetchSpy)[0])}`,
    );
    expect(posts(fetchSpy)).toHaveLength(2);
    second.unmount();
    payment = activity;
    const blockedStorage = vi.spyOn(Storage.prototype, 'removeItem').mockImplementation(() => {
      throw new DOMException('Browser storage is blocked', 'SecurityError');
    });
    vi.spyOn(api, 'accounts').mockResolvedValue(
      testAccounts.map((account) => ({
        ...account,
        ironwood_zatoshi: account.ironwood_zatoshi + 100_000_000n,
      })),
    );
    const client = new QueryClient({
      defaultOptions: { queries: { retry: false, staleTime: Infinity } },
    });
    client.setQueryData(queryKeys.accounts, testAccounts);
    client.setQueryData(queryKeys.activity(30), [{ ...activity, status: 'broadcast' }]);
    function WalletSnapshot() {
      const accounts = useAccounts();
      const recent = useActivity();
      return (
        <>
          <output aria-label="Account balance">
            {accounts.data?.[0]?.ironwood_zatoshi.toString()}
          </output>
          <output aria-label="Recent payment">{recent.data?.[0]?.status}</output>
        </>
      );
    }
    const third = renderWithProviders(
      <>
        <FaucetDialog open onOpenChange={onOpenChange} accounts={testAccounts} />
        <WalletSnapshot />
      </>,
      client,
    );
    await waitFor(() =>
      expect(screen.getByText('Payments').closest('button')).toHaveAttribute(
        'aria-label',
        expect.stringMatching(/1 confirmed/),
      ),
    );
    expect(await screen.findByText(/Previous payment finished/)).toBeInTheDocument();
    await waitFor(() => {
      expect(screen.getByLabelText('Account balance')).toHaveTextContent('600000000');
      expect(screen.getByLabelText('Recent payment')).toHaveTextContent('confirmed');
    });
    expect(localStorage.getItem('ths:faucet:1:ironwood:100000000')).toBe(key(posts(fetchSpy)[0]));
    blockedStorage.mockRestore();
    third.unmount();
    renderWithProviders(<FaucetDialog open onOpenChange={onOpenChange} accounts={testAccounts} />);
    await waitFor(() =>
      expect(screen.getByText('Payments').closest('button')).toHaveAttribute(
        'aria-label',
        expect.stringMatching(/1 confirmed/),
      ),
    );
    await waitFor(() => expect(localStorage.getItem('ths:faucet:1:ironwood:100000000')).toBeNull());
    expect(onOpenChange).not.toHaveBeenCalled();
    expect(screen.getByRole('dialog')).toBeInTheDocument();
    expect(posts(fetchSpy)).toHaveLength(2);
    fetchSpy.mockRestore();
  });

  it('shows the faucet limit while editing and prevents submission', async () => {
    const fetchSpy = vi.spyOn(globalThis, 'fetch');
    renderWithProviders(<FaucetDialog open onOpenChange={vi.fn()} accounts={testAccounts} />);

    const amount = screen.getByLabelText('Amount (ZEC)');
    await userEvent.clear(amount);
    await userEvent.type(amount, '6');

    expect(await screen.findByRole('alert')).toHaveTextContent(
      'The faucet is limited to 5 ZEC per request.',
    );
    expect(screen.getByRole('button', { name: 'Add funds' })).toBeDisabled();

    await userEvent.keyboard('{Enter}');
    expect(posts(fetchSpy)).toHaveLength(0);
    fetchSpy.mockRestore();
  });

  it('rejects non-numeric input', async () => {
    renderWithProviders(<FaucetDialog open onOpenChange={vi.fn()} accounts={testAccounts} />);

    const amount = screen.getByLabelText('Amount (ZEC)');
    await userEvent.clear(amount);
    await userEvent.type(amount, 'abc');
    await userEvent.click(screen.getByRole('button', { name: 'Add funds' }));

    expect(await screen.findByRole('alert')).toHaveTextContent('up to 8 decimal places');
  });
});

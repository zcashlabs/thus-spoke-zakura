import { describe, expect, it, vi, beforeEach, afterEach } from 'vitest';
import { act, fireEvent, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { MemoryRouter, useLocation } from 'react-router-dom';
import { QueryClient } from '@tanstack/react-query';
import {
  renderWithProviders,
  renderWithWalletActions,
  requestUrl,
  testAccounts,
} from '@/test/utils';
import { queryKeys } from '@/hooks/queries';
import { api, type Activity } from '@/lib/api';
import { PaymentUpdates } from './PaymentUpdates';
import { WalletActionsProvider } from './WalletActions';
import { WalletPage } from './WalletPage';

const TXID = 'ab'.repeat(32);
const PAYMENT_URI = 'zcash:uregtest1account2?amount=1.23456789&memo=cmVudCBmb3IgT2N0b2Jlcg';
const testLocks = Object.getOwnPropertyDescriptor(navigator, 'locks')!;

function activityRow(
  id: string,
  overrides: Partial<{
    kind: 'send' | 'faucet' | 'transaction';
    status: string;
    to_account: number;
  }> = {},
) {
  return {
    id,
    kind: overrides.kind ?? 'transaction',
    from_account: null,
    to_account: overrides.to_account ?? 1,
    to_address: null,
    source_pool: null,
    destination_pool: 'ironwood',
    amount_zatoshi: 1_000_000,
    txid: id.padEnd(64, '0'),
    block_hash: 'cd'.repeat(32),
    status: overrides.status ?? 'confirmed',
    last_error: null,
    created_at: '2026-10-18 10:00:00',
  };
}

function json(body: unknown, headers?: HeadersInit): Promise<Response> {
  const responseHeaders = new Headers(headers);
  responseHeaders.set('content-type', 'application/json');
  return Promise.resolve(
    new Response(JSON.stringify(body), {
      status: 200,
      headers: responseHeaders,
    }),
  );
}

function mockWalletApis(status = 'confirmed') {
  vi.spyOn(globalThis, 'fetch').mockImplementation((input, init) => {
    const url = requestUrl(input);
    if (url.includes('/accounts')) {
      return json([
        {
          id: 1,
          name: 'Account 1',
          unified_address: 'uregtest1account1verylongunifiedaddressfortruncation',
          transparent_address: 'tmAccount1',
          transparent_zatoshi: 0,
          ironwood_zatoshi: 500_000_000,
        },
        {
          id: 2,
          name: 'Account 2',
          unified_address: 'uregtest1account2',
          transparent_address: 'tmAccount2',
          transparent_zatoshi: 0,
          ironwood_zatoshi: 0,
        },
      ]);
    }
    if (url.includes('/activity')) {
      return json([
        {
          id: 'act-1',
          kind: 'faucet',
          from_account: null,
          to_account: 1,
          source_pool: 'ironwood',
          destination_pool: 'ironwood',
          amount_zatoshi: 500_000_000,
          txid: status === 'failed' ? '' : TXID,
          block_hash: 'cd'.repeat(32),
          status,
          last_error:
            status === 'failed'
              ? 'Insufficient funds'
              : status === 'broadcast'
                ? 'Lightwalletd unavailable'
                : null,
          created_at: '2026-09-18 10:00:00',
        },
      ]);
    }
    if (url.endsWith('/zip321/request')) return json({ uri: PAYMENT_URI });
    if (url.endsWith('/zip321/parse')) {
      const body = JSON.parse(typeof init?.body === 'string' ? init.body : '{}') as {
        uri?: string;
      };
      if (body.uri !== PAYMENT_URI) throw new Error(`Unexpected payment URI: ${body.uri}`);
      return json({
        address: 'uregtest1account2',
        destination_pool: 'ironwood',
        to_account: 2,
        amount_zatoshi: 123_456_789,
        memo: 'rent for October',
      });
    }
    if (url.endsWith('/send/quote')) {
      return json({
        available_zatoshi: 500_000_000,
        fee_zatoshi: 10_000,
        max_zatoshi: 499_990_000,
      });
    }
    return json({
      instance: 'default',
      node: null,
      account_count: 5,
      mining_mode: 'auto',
      auto_mine: true,
      network: 'regtest',
    });
  });
}

function renderWallet(status = 'confirmed') {
  mockWalletApis(status);
  return renderWithProviders(
    <MemoryRouter>
      <WalletActionsProvider>
        <PaymentUpdates />
        <WalletPage />
      </WalletActionsProvider>
    </MemoryRouter>,
  );
}

function CurrentPath() {
  const { pathname } = useLocation();
  return <output aria-label="Current path">{pathname}</output>;
}

function savedFaucet(
  key: string,
  account: number,
  status: Activity['status'] | null,
): Activity | null {
  const name = `ths:faucet:${account}:ironwood:500000000`;
  localStorage.setItem(name, key);
  localStorage.setItem(`ths:faucet:retained:${key}`, name);
  return status === null
    ? null
    : {
        id: key,
        kind: 'faucet',
        from_account: null,
        to_account: account,
        to_address: null,
        source_pool: 'ironwood',
        destination_pool: 'ironwood',
        amount_zatoshi: 500_000_000n,
        txid: status === 'confirmed' ? TXID : '',
        block_hash: null,
        status,
        last_error: status === 'failed' ? 'Insufficient funds' : null,
        created_at: '2026-10-11 00:00:00',
      };
}

function savedName(kind: 'send' | 'faucet') {
  return kind === 'send'
    ? 'ths:send:[1,2,"ironwood","ironwood","100000000",null]'
    : 'ths:faucet:1:ironwood:100000000';
}

function retainSaved(kind: 'send' | 'faucet', key: string, name = savedName(kind)) {
  localStorage.setItem(name, key);
  localStorage.setItem(`ths:${kind}:retained:${key}`, name);
}

function preparingPayment(kind: 'send' | 'faucet', key: string): Activity {
  return {
    id: key,
    kind,
    from_account: kind === 'send' ? 1 : null,
    to_account: kind === 'send' ? 2 : 1,
    to_address: null,
    source_pool: 'ironwood',
    destination_pool: 'ironwood',
    amount_zatoshi: 100_000_000n,
    txid: '',
    block_hash: null,
    status: 'preparing',
    last_error: null,
    created_at: '2026-10-14 00:00:00',
  };
}

function deferPaymentLock() {
  let release!: () => void;
  let tail = new Promise<void>((resolve) => {
    release = resolve;
  });
  const request = vi.fn((name: string, callback: (lock: Lock) => unknown) => {
    const result = tail.then(() => callback({ name, mode: 'exclusive' }));
    tail = result.then(
      () => undefined,
      () => undefined,
    );
    return result;
  });
  Object.defineProperty(navigator, 'locks', { configurable: true, value: { request } });
  return {
    request,
    release: async () => {
      release();
      await tail;
    },
  };
}

beforeEach(() => {
  localStorage.clear();
  sessionStorage.clear();
});

afterEach(() => {
  vi.restoreAllMocks();
  Object.defineProperty(navigator, 'locks', testLocks);
});

describe('WalletPage', () => {
  it('closes Payments when following a confirmed payment transaction link', async () => {
    mockWalletApis();
    savedFaucet('confirmed-faucet-key', 1, 'confirmed');
    renderWithProviders(
      <MemoryRouter initialEntries={['/wallet']}>
        <WalletActionsProvider>
          <PaymentUpdates />
          <CurrentPath />
        </WalletActionsProvider>
      </MemoryRouter>,
    );

    await userEvent.click(await screen.findByRole('button', { name: /View payments/ }));
    const payments = await screen.findByRole('dialog', { name: 'Payments' });
    await userEvent.click(within(payments).getByRole('link', { name: 'View transaction' }));

    await waitFor(() =>
      expect(screen.getByLabelText('Current path')).toHaveTextContent(`/explorer/tx/${TXID}`),
    );
    await waitFor(() => expect(screen.queryByRole('dialog', { name: 'Payments' })).toBeNull());
  });

  it.each(['ctrlKey', 'metaKey', 'shiftKey'] as const)(
    'keeps Payments open when opening a confirmed transaction in another context with %s',
    async (modifier) => {
      mockWalletApis();
      savedFaucet('confirmed-faucet-key', 1, 'confirmed');
      renderWithProviders(
        <MemoryRouter initialEntries={['/wallet']}>
          <WalletActionsProvider>
            <PaymentUpdates />
            <CurrentPath />
          </WalletActionsProvider>
        </MemoryRouter>,
      );

      await userEvent.click(await screen.findByRole('button', { name: /View payments/ }));
      const payments = await screen.findByRole('dialog', { name: 'Payments' });
      fireEvent.click(within(payments).getByRole('link', { name: 'View transaction' }), {
        [modifier]: true,
      });

      expect(screen.getByRole('dialog', { name: 'Payments' })).toBeInTheDocument();
      expect(screen.getByLabelText('Current path')).toHaveTextContent('/wallet');
    },
  );

  it.each(['confirmed', 'failed', 'broadcast'])(
    'restores %s outside the dialogs using GET only',
    async (status) => {
      const name = 'ths:faucet:1:ironwood:500000000';
      const key = 'restored-faucet-key';
      localStorage.setItem(name, key);
      localStorage.setItem(`ths:faucet:retained:${key}`, name);
      renderWallet(status);
      const trigger = await screen.findByRole('button', { name: /View payments/ });
      expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
      await waitFor(() =>
        expect(trigger).toHaveAccessibleName(
          status === 'broadcast' ? /1 unresolved/ : new RegExp(`1 ${status}`),
        ),
      );
      // Cleanup happens while the panel is closed; opening it still shows the result.
      if (status !== 'broadcast')
        await waitFor(() => expect(localStorage.getItem(name)).toBeNull());
      await userEvent.click(trigger);
      const updates = await screen.findByRole('region', { name: 'Payment updates' });
      expect(
        await within(updates).findByText(
          status === 'broadcast' ? 'Pending' : status === 'confirmed' ? 'Confirmed' : 'Failed',
        ),
      ).toBeInTheDocument();
      expect(screen.queryByText('Funded Account 1')).not.toBeInTheDocument();
      expect(screen.queryByText('Payment failed', { exact: true })).not.toBeInTheDocument();
      expect(vi.mocked(fetch).mock.calls.some(([, init]) => init?.method === 'POST')).toBe(false);
      expect(
        vi
          .mocked(fetch)
          .mock.calls.some(([input]) => requestUrl(input).includes(`idempotency_key=${key}`)),
      ).toBe(true);
      if (status === 'broadcast') {
        expect(within(updates).getByText('Waiting for confirmation')).toBeInTheDocument();
        expect(localStorage.getItem(name)).toBe(key);
        expect(within(updates).getByRole('button', { name: 'Resume' })).toBeInTheDocument();
        expect(
          within(updates).queryByRole('button', { name: 'Forget saved request' }),
        ).not.toBeInTheDocument();
      } else {
        await waitFor(() => expect(localStorage.getItem(name)).toBeNull());
        expect(updates).toBeInTheDocument();
        await userEvent.click(screen.getByRole('button', { name: 'Clear finished' }));
        expect(within(updates).queryByRole('heading')).not.toBeInTheDocument();
        await userEvent.click(screen.getByRole('button', { name: 'Close dialog' }));
        expect(screen.queryByRole('button', { name: /View payments/ })).not.toBeInTheDocument();
      }
    },
  );

  it.each(['send', 'faucet'] as const)(
    "prefers this tab's %s key when matching saved payments have different keys",
    async (kind) => {
      mockWalletApis();
      vi.spyOn(api, 'sendQuote').mockResolvedValue({
        available_zatoshi: 500_000_000n,
        fee_zatoshi: 10_000n,
        max_zatoshi: 499_990_000n,
      });
      const name =
        kind === 'send'
          ? 'ths:send:[1,2,"ironwood","ironwood","100000000",""]'
          : 'ths:faucet:1:ironwood:100000000';
      const ownKey = 'current-tab-key';
      const otherKey = 'other-tab-key';
      // The shared records put the other tab first; this tab retains its own key.
      for (const key of [otherKey, ownKey])
        localStorage.setItem(`ths:${kind}:retained:${key}`, name);
      localStorage.setItem(name, otherKey);
      sessionStorage.setItem(`${name}:tab`, ownKey);
      const payment = (key: string): Activity => ({
        id: key,
        kind,
        from_account: kind === 'send' ? 1 : null,
        to_account: kind === 'send' ? 2 : 1,
        to_address: null,
        source_pool: 'ironwood',
        destination_pool: 'ironwood',
        amount_zatoshi: 100_000_000n,
        txid: key === ownKey ? TXID : '',
        block_hash: null,
        status: key === ownKey ? 'prepared' : 'preparing',
        last_error: null,
        created_at: '2026-10-12 00:00:00',
      });
      vi.spyOn(api, 'payment').mockImplementation((key) => Promise.resolve(payment(key)));
      const post = vi.spyOn(api, kind).mockResolvedValue(payment(ownKey));
      renderWithWalletActions(<WalletPage />);
      await userEvent.click(
        screen.getByRole('button', {
          name: kind === 'send' ? 'Send from Account 1' : 'Fund Account 1',
        }),
      );
      const feedback = await screen.findByText(
        /saved payment has these details \((preparing|prepared)\)/,
      );
      const shownStatus = feedback.textContent;
      expect(post).not.toHaveBeenCalled();
      await userEvent.click(screen.getByRole('button', { name: 'Resume payment' }));
      await waitFor(() => expect(post).toHaveBeenCalledTimes(1));
      expect(post).toHaveBeenCalledWith(
        kind === 'send'
          ? {
              from_account: 1,
              to_account: 2,
              source_pool: 'ironwood',
              destination_pool: 'ironwood',
              amount_zatoshi: 100_000_000n,
              memo: '',
              idempotency_key: ownKey,
            }
          : {
              account_id: 1,
              pool: 'ironwood',
              amount_zatoshi: 100_000_000n,
              idempotency_key: ownKey,
            },
      );
      expect(shownStatus).toContain('(prepared)');
      expect(sessionStorage.getItem(`${name}:tab`)).toBe(ownKey);
      expect(localStorage.getItem(name)).toBe(otherKey);
      expect(localStorage.getItem(`ths:${kind}:retained:${otherKey}`)).toBe(name);
    },
  );

  it('filters results without stopping hidden checks or clearing hidden results', async () => {
    mockWalletApis();
    const payments = new Map([
      ['pending-key', savedFaucet('pending-key', 1, 'broadcast')],
      ['confirmed-key', savedFaucet('confirmed-key', 2, 'confirmed')],
      ['failed-key', savedFaucet('failed-key', 3, 'failed')],
      ['unknown-key', savedFaucet('unknown-key', 4, null)],
    ]);
    const lookup = vi
      .spyOn(api, 'payment')
      .mockImplementation((key) => Promise.resolve(payments.get(key) ?? null));
    const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    renderWithWalletActions(<WalletPage />, client);
    await userEvent.click(await screen.findByRole('button', { name: /View payments/ }));
    const filters = screen.getByRole('group', { name: 'Filter payments' });
    const updates = screen.getByRole('region', { name: 'Payment updates' });
    expect(
      await within(filters).findByRole('button', { name: 'Unresolved 2' }),
    ).toBeInTheDocument();
    expect(await within(updates).findByText('Pending')).toBeInTheDocument();
    expect(await within(updates).findByText('Unknown')).toBeInTheDocument();
    await userEvent.click(await within(filters).findByRole('button', { name: 'Failed 1' }));
    expect(within(updates).getAllByRole('heading')).toHaveLength(1);
    expect(within(updates).getByText('Failed')).toBeInTheDocument();
    const reads = lookup.mock.calls.filter(([key]) => key === 'pending-key').length;
    await act(() =>
      client.refetchQueries({ queryKey: queryKeys.payment('pending-key'), type: 'active' }),
    );
    expect(lookup.mock.calls.filter(([key]) => key === 'pending-key')).toHaveLength(reads + 1);
    await userEvent.click(screen.getByRole('button', { name: 'Clear failed' }));
    expect(await within(updates).findByText('No failed payments.')).toBeInTheDocument();
    expect(within(filters).getByRole('button', { name: 'Failed 0' })).toBeEnabled();
    expect(within(filters).getByRole('button', { name: 'Confirmed 1' })).toBeInTheDocument();
    expect(localStorage.getItem('ths:faucet:1:ironwood:500000000')).toBe('pending-key');
    expect(localStorage.getItem('ths:faucet:4:ironwood:500000000')).toBe('unknown-key');
    await userEvent.keyboard('{Escape}');
    await userEvent.click(screen.getByRole('button', { name: /View payments/ }));
    expect(screen.getByRole('button', { name: 'All 3' })).toHaveAttribute('aria-pressed', 'true');
    expect(
      within(screen.getByRole('region', { name: 'Payment updates' })).getAllByRole('heading'),
    ).toHaveLength(3);
    expect(vi.mocked(fetch).mock.calls.some(([, init]) => init?.method === 'POST')).toBe(false);
  });

  it.each(['confirmed', 'failed'] as const)(
    'keeps lookup errors unresolved, announces restored %s once, and preserves filter focus',
    async (outcome) => {
      mockWalletApis();
      let payment = savedFaucet('live-key', 1, 'broadcast');
      let readError = false;
      vi.spyOn(api, 'payment').mockImplementation(() =>
        readError ? Promise.reject(new Error('Connection lost')) : Promise.resolve(payment),
      );
      const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
      renderWithWalletActions(<WalletPage />, client);
      await userEvent.click(await screen.findByRole('button', { name: /View payments/ }));
      const updates = screen.getByRole('region', { name: 'Payment updates' });
      const selected = screen.getByRole('button', { name: 'Unresolved 1' });
      await userEvent.click(selected);
      const resume = await within(updates).findByRole('button', { name: 'Resume' });
      resume.focus();
      readError = true;
      await act(() => client.refetchQueries({ queryKey: queryKeys.payment('live-key') }));
      expect(await within(updates).findByText(/Couldn’t check status/)).toBeInTheDocument();
      expect(within(updates).getByText('Pending')).toBeInTheDocument();
      expect(within(updates).getByText('Last known: Waiting for confirmation')).toBeInTheDocument();
      expect(
        within(updates).queryByRole('button', { name: 'Forget saved request' }),
      ).not.toBeInTheDocument();
      expect(screen.getByRole('button', { name: 'Failed 0' })).toBeInTheDocument();
      expect(resume).toHaveFocus();
      expect(localStorage.getItem('ths:faucet:1:ironwood:500000000')).toBe('live-key');
      readError = false;
      payment = {
        ...payment!,
        status: outcome,
        txid: outcome === 'confirmed' ? TXID : '',
        last_error: outcome === 'failed' ? 'Insufficient funds' : null,
      };
      await act(() => client.refetchQueries({ queryKey: queryKeys.payment('live-key') }));
      expect(await within(updates).findByText('No unresolved payments.')).toBeInTheDocument();
      await waitFor(() => expect(selected).toHaveFocus());
      expect(selected).toHaveAccessibleName('Unresolved 0');
      expect(selected).toHaveAttribute('aria-pressed', 'true');
      expect(within(updates).queryByRole('heading')).not.toBeInTheDocument();
      const label = outcome === 'confirmed' ? 'Confirmed' : 'Failed';
      expect(screen.getByRole('button', { name: `${label} 1` })).toBeInTheDocument();
      const announcement = outcome === 'confirmed' ? 'Funded Account 1' : 'Payment failed';
      expect(screen.getAllByText(announcement, { exact: true })).toHaveLength(1);
      if (outcome === 'failed') {
        expect(screen.getByText('5 ZEC to Account 1. Insufficient funds')).toBeInTheDocument();
      }
      await waitFor(() => {
        const liveAnnouncement = Array.from(
          document.querySelectorAll('[role="status"][aria-live]'),
        ).find((element) => element.textContent?.includes(announcement));
        expect(liveAnnouncement).toHaveAttribute(
          'aria-live',
          outcome === 'confirmed' ? 'polite' : 'assertive',
        );
      });
      expect(
        document.querySelector('li[data-state="open"] button[aria-label="Dismiss notification"]'),
      ).not.toBeNull();
      await waitFor(() =>
        expect(localStorage.getItem('ths:faucet:1:ironwood:500000000')).toBeNull(),
      );
      await userEvent.click(within(updates).getByRole('button', { name: 'Show all' }));
      expect(within(updates).getByText(label)).toBeInTheDocument();
      await act(() => client.refetchQueries({ queryKey: queryKeys.payment('live-key') }));
      expect(screen.getAllByText(announcement, { exact: true })).toHaveLength(1);
      expect(vi.mocked(fetch).mock.calls.some(([, init]) => init?.method === 'POST')).toBe(false);
    },
  );

  it.each([
    ['send', 'resume-first'],
    ['send', 'forget-first'],
    ['faucet', 'resume-first'],
    ['faucet', 'forget-first'],
  ] as const)('preserves a queued %s request when %s races for its lock', async (kind, order) => {
    mockWalletApis();
    const fallback = vi.mocked(fetch).getMockImplementation()!;
    const name = savedName(kind);
    const key = 'queued-payment-key';
    const record = `ths:${kind}:retained:${key}`;
    retainSaved(kind, key);
    let payment: Activity | null = null;
    vi.spyOn(api, 'payment').mockImplementation(() => Promise.resolve(payment));
    let finishPost!: (response: Response) => void;
    const response = new Promise<Response>((resolve) => {
      finishPost = resolve;
    });
    vi.mocked(fetch).mockImplementation((input, init) =>
      init?.method === 'POST' && requestUrl(input).endsWith(`/${kind}`)
        ? response
        : fallback(input, init),
    );
    const posts = () =>
      vi
        .mocked(fetch)
        .mock.calls.filter(
          ([input, init]) => init?.method === 'POST' && requestUrl(input).endsWith(`/${kind}`),
        );
    const bodies = () =>
      posts().map(([, init]): unknown =>
        JSON.parse(typeof init?.body === 'string' ? init.body : '{}'),
      );

    renderWithWalletActions(<WalletPage />);
    await userEvent.click(await screen.findByRole('button', { name: /View payments/ }));
    const forget = await screen.findByRole('button', { name: 'Forget saved request' });
    const resume = screen.getByRole('button', { name: 'Resume' });
    const row = forget.closest('article')!;
    const lock = deferPaymentLock();
    const removed = vi.spyOn(Storage.prototype, 'removeItem');

    // Batch both clicks to exercise an already-rendered Forget control as well as its UI gate.
    act(() => {
      if (order === 'resume-first') {
        resume.click();
        forget.click();
      } else {
        forget.click();
        resume.click();
      }
    });
    await waitFor(() => expect(lock.request).toHaveBeenCalledTimes(2));
    expect(lock.request.mock.calls.every(([lockedName]) => lockedName === name)).toBe(true);
    expect(posts()).toHaveLength(0);
    const forgetWhileQueued = screen.queryByRole('button', { name: 'Forget saved request' });
    await act(lock.release);
    await waitFor(() => expect(posts()).toHaveLength(1));

    payment = preparingPayment(kind, key);
    await act(async () => {
      finishPost(
        new Response(JSON.stringify({ ...payment, amount_zatoshi: 100_000_000 }), {
          status: 202,
          headers: { 'content-type': 'application/json' },
        }),
      );
      await response;
    });
    expect(forgetWhileQueued).toBeNull();
    expect(document.body).toContainElement(row);
    expect(localStorage.getItem(name)).toBe(key);
    expect(localStorage.getItem(record)).toBe(name);
    expect(sessionStorage.getItem(`${name}:tab`)).toBe(key);
    expect(removed).not.toHaveBeenCalledWith(record);
    expect(bodies()[0]).toMatchObject({ idempotency_key: key });
    await userEvent.click(await within(row).findByRole('button', { name: 'Resume' }));
    await waitFor(() => expect(posts()).toHaveLength(2));
    expect(bodies()[1]).toMatchObject({ idempotency_key: key });
    expect(await within(row).findByText('Preparing payment')).toBeInTheDocument();
    await userEvent.click(screen.getByRole('button', { name: 'Close dialog' }));
    expect(screen.getByRole('button', { name: /View payments/ })).toHaveAccessibleName(
      /1 unresolved/,
    );
  });

  it.each([
    ['send', 'record'],
    ['send', 'lookup-failure'],
    ['faucet', 'record'],
    ['faucet', 'lookup-failure'],
  ] as const)('keeps a %s request if %s arrives while Forget waits', async (kind, outcome) => {
    mockWalletApis();
    const name = savedName(kind);
    const key = 'lookup-race-key';
    const record = `ths:${kind}:retained:${key}`;
    retainSaved(kind, key);
    const lookup = vi.spyOn(api, 'payment').mockResolvedValue(null);
    const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    renderWithWalletActions(<WalletPage />, client);
    await userEvent.click(await screen.findByRole('button', { name: /View payments/ }));
    const forget = await screen.findByRole('button', { name: 'Forget saved request' });
    const row = forget.closest('article')!;
    const lock = deferPaymentLock();
    const removed = vi.spyOn(Storage.prototype, 'removeItem');
    await userEvent.click(forget);
    await waitFor(() => expect(lock.request).toHaveBeenCalledTimes(1));

    if (outcome === 'record') lookup.mockResolvedValue(preparingPayment(kind, key));
    else lookup.mockRejectedValue(new Error('status offline'));
    await act(() => client.refetchQueries({ queryKey: queryKeys.payment(key) }));
    await act(lock.release);

    expect(document.body).toContainElement(row);
    expect(localStorage.getItem(name)).toBe(key);
    expect(localStorage.getItem(record)).toBe(name);
    expect(removed).not.toHaveBeenCalledWith(record);
    expect(screen.queryByRole('button', { name: 'Forget saved request' })).not.toBeInTheDocument();
    expect(vi.mocked(fetch).mock.calls.some(([, init]) => init?.method === 'POST')).toBe(false);
  });

  it.each(['send', 'faucet'] as const)(
    'handles a delayed %s startup failure across reload and same-origin restart',
    async (kind) => {
      mockWalletApis();
      const fallback = vi.mocked(fetch).getMockImplementation()!;
      let finishPost!: (response: Response) => void;
      const postResponse = new Promise<Response>((resolve) => {
        finishPost = resolve;
      });
      vi.mocked(fetch).mockImplementation((input, init) =>
        init?.method === 'POST' && requestUrl(input).endsWith(`/${kind}`)
          ? postResponse
          : fallback(input, init),
      );
      const payment = vi.spyOn(api, 'payment').mockResolvedValue(null);

      const first = renderWithWalletActions(<WalletPage />);
      await userEvent.click(
        await screen.findByRole('button', {
          name: kind === 'send' ? 'Send ZEC' : 'Fund Account 1',
        }),
      );
      await userEvent.click(
        within(screen.getByRole('dialog')).getByRole('button', {
          name: kind === 'send' ? 'Send ZEC' : 'Add funds',
        }),
      );
      await waitFor(() =>
        expect(
          vi
            .mocked(fetch)
            .mock.calls.filter(
              ([input, init]) => init?.method === 'POST' && requestUrl(input).endsWith(`/${kind}`),
            ),
        ).toHaveLength(1),
      );
      const name = savedName(kind);
      const key = localStorage.getItem(name)!;
      expect(key).toBeTruthy();
      await userEvent.keyboard('{Escape}');
      await userEvent.click(await screen.findByRole('button', { name: /View payments/ }));
      expect(await screen.findByText('Submitting')).toBeInTheDocument();
      expect(await screen.findByText('No record yet. The request is saved.')).toBeInTheDocument();
      expect(payment).toHaveBeenCalledWith(key);
      expect(
        screen.queryByRole('button', { name: 'Forget saved request' }),
      ).not.toBeInTheDocument();

      finishPost(
        new Response(
          JSON.stringify({ error: { message: 'the environment is still starting', status: 503 } }),
          { status: 503, headers: { 'content-type': 'application/json' } },
        ),
      );
      expect(
        await screen.findByRole('button', { name: 'Forget saved request' }),
      ).toBeInTheDocument();
      expect(localStorage.getItem(`ths:${kind}:retained:${key}`)).toBe(name);

      first.unmount();
      const reloaded = renderWithWalletActions(<WalletPage />);
      await userEvent.click(await screen.findByRole('button', { name: /View payments/ }));
      expect(await screen.findByText('No record yet. The request is saved.')).toBeInTheDocument();
      expect(
        screen.getByText(
          'Forgetting removes it from this browser only; it does not cancel server work.',
        ),
      ).toBeInTheDocument();
      await userEvent.click(screen.getByRole('button', { name: 'Forget saved request' }));
      await waitFor(() => expect(localStorage.getItem(name)).toBeNull());
      expect(localStorage.getItem(`ths:${kind}:retained:${key}`)).toBeNull();

      reloaded.unmount();
      // Stop/start preserves the dashboard origin, so cleared browser state must stay cleared.
      renderWithWalletActions(<WalletPage />);
      await waitFor(() =>
        expect(screen.queryByRole('button', { name: /View payments/ })).not.toBeInTheDocument(),
      );
    },
  );

  it.each(['send', 'faucet'] as const)(
    "forgets only this tab's absent %s request",
    async (kind) => {
      mockWalletApis();
      const name = savedName(kind);
      const ownKey = 'current-tab-key';
      const otherKey = 'other-tab-key';
      retainSaved(kind, ownKey, name);
      retainSaved(kind, otherKey, name);
      localStorage.setItem(name, otherKey);
      sessionStorage.setItem(`${name}:tab`, ownKey);
      vi.spyOn(api, 'payment').mockImplementation((key) =>
        Promise.resolve(
          key === ownKey
            ? null
            : {
                id: otherKey,
                kind,
                from_account: kind === 'send' ? 1 : null,
                to_account: kind === 'send' ? 2 : 1,
                to_address: null,
                source_pool: 'ironwood',
                destination_pool: 'ironwood',
                amount_zatoshi: 100_000_000n,
                txid: '',
                block_hash: null,
                status: 'preparing',
                last_error: null,
                created_at: '2026-10-13 00:00:00',
              },
        ),
      );

      renderWithWalletActions(<WalletPage />);
      await userEvent.click(await screen.findByRole('button', { name: /View payments/ }));
      const own = document.querySelector<HTMLElement>(`article[data-payment-key="${ownKey}"]`)!;
      await userEvent.click(
        await within(own).findByRole('button', { name: 'Forget saved request' }),
      );
      await waitFor(() => expect(document.body).not.toContainElement(own));
      expect(sessionStorage.getItem(`${name}:tab`)).toBeNull();
      expect(localStorage.getItem(`ths:${kind}:retained:${ownKey}`)).toBeNull();
      expect(localStorage.getItem(name)).toBe(otherKey);
      expect(localStorage.getItem(`ths:${kind}:retained:${otherKey}`)).toBe(name);
    },
  );

  it('keeps a Faucet treasury failure pending and retained', async () => {
    mockWalletApis();
    const payment = savedFaucet('treasury-key', 1, 'preparing')!;
    payment.last_error = 'Treasury shielding failed';
    vi.spyOn(api, 'payment').mockResolvedValue(payment);

    renderWithWalletActions(<WalletPage />);
    await userEvent.click(await screen.findByRole('button', { name: /View payments/ }));
    expect(
      await screen.findByText('Latest execution error: Treasury shielding failed'),
    ).toBeInTheDocument();
    expect(screen.getByText('Recovery may still be running.')).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Forget saved request' })).not.toBeInTheDocument();
    expect(localStorage.getItem('ths:faucet:1:ironwood:500000000')).toBe('treasury-key');
  });

  it('tracks a late POST after closing its dialog and preserves a newly opened draft', async () => {
    vi.spyOn(api, 'accounts').mockResolvedValue(testAccounts);
    vi.spyOn(api, 'sendQuote').mockResolvedValue({
      available_zatoshi: 500_000_000n,
      fee_zatoshi: 10_000n,
      max_zatoshi: 499_990_000n,
    });
    let resolve!: (activity: Activity) => void;
    let payment: Activity | null = null;
    const response = new Promise<Activity>((done) => {
      resolve = done;
    });
    const post = vi.spyOn(api, 'send').mockReturnValue(response);
    vi.spyOn(api, 'payment').mockImplementation(() => Promise.resolve(payment));
    renderWallet();
    await waitFor(() =>
      expect(screen.getAllByRole('heading', { name: /Account [1-5]/ })).toHaveLength(5),
    );
    await userEvent.click(screen.getByRole('button', { name: 'Send ZEC' }));
    await userEvent.click(
      within(screen.getByRole('dialog')).getByRole('button', { name: 'Send ZEC' }),
    );
    await waitFor(() => expect(post).toHaveBeenCalledTimes(1));
    await userEvent.keyboard('{Escape}');
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
    expect(screen.getByRole('button', { name: /View payments/ })).toBeInTheDocument();
    await userEvent.click(screen.getByRole('button', { name: 'Send ZEC' }));
    await userEvent.clear(screen.getByLabelText('Amount (ZEC)'));
    await userEvent.type(screen.getByLabelText('Amount (ZEC)'), '2');
    await act(async () => {
      payment = {
        id: 'late-send',
        kind: 'send',
        from_account: 1,
        to_account: 2,
        to_address: null,
        source_pool: 'ironwood',
        destination_pool: 'ironwood',
        amount_zatoshi: 100_000_000n,
        status: 'confirmed',
        txid: TXID,
        block_hash: null,
        last_error: null,
        created_at: '2026-10-11 00:00:00',
      };
      resolve(payment);
      await response;
    });
    await waitFor(() =>
      expect(screen.getByText('Payments').closest('button')).toHaveAttribute(
        'aria-label',
        expect.stringMatching(/1 confirmed/),
      ),
    );
    expect(screen.getByRole('dialog')).toBeInTheDocument();
    expect(screen.getByLabelText('Amount (ZEC)')).toHaveValue('2');
    expect(screen.getAllByText('Sent to Account 2')).toHaveLength(1);
    expect(post).toHaveBeenCalledTimes(1);
  });

  it.each(['send', 'faucet'] as const)(
    'links a late %s error and loads the selected failed payment',
    async (kind) => {
      Object.assign(Element.prototype, {
        scrollIntoView: vi.fn(),
        hasPointerCapture: () => false,
        releasePointerCapture: () => undefined,
      });
      mockWalletApis();
      vi.spyOn(api, 'accounts').mockResolvedValue(testAccounts);
      const payments = new Map([
        ['other-confirmed', savedFaucet('other-confirmed', 4, 'confirmed')],
        ['other-failed', savedFaucet('other-failed', 5, 'failed')],
      ]);
      vi.spyOn(api, 'payment').mockImplementation((key) =>
        Promise.resolve(payments.get(key) ?? null),
      );
      vi.spyOn(api, 'sendQuote').mockResolvedValue({
        available_zatoshi: 500_000_000n,
        fee_zatoshi: 10_000n,
        max_zatoshi: 499_990_000n,
      });
      let reject!: (error: Error) => void;
      const response = new Promise<Activity>((_resolve, fail) => {
        reject = fail;
      });
      const post = vi.spyOn(api, kind).mockReturnValue(response);
      const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
      renderWithWalletActions(<WalletPage />, client);
      const trigger = await screen.findByRole('button', { name: /View payments/ });
      await userEvent.click(trigger);
      await userEvent.click(await screen.findByRole('button', { name: 'Failed 1' }));
      await userEvent.keyboard('{Escape}');
      await userEvent.click(
        screen.getByRole('button', {
          name: kind === 'send' ? 'Send ZEC' : 'Fund Account 1',
        }),
      );
      const submit = within(screen.getByRole('dialog')).getByRole('button', {
        name: kind === 'send' ? 'Send ZEC' : 'Add funds',
      });
      await waitFor(() => expect(submit).toBeEnabled());
      await userEvent.click(submit);
      await waitFor(() => expect(post).toHaveBeenCalledTimes(1));
      const key = post.mock.calls[0]![0].idempotency_key;
      await userEvent.keyboard('{Escape}');
      await act(async () => {
        reject(new Error('Connection lost'));
        await response.catch(() => undefined);
      });
      expect(await screen.findByText('Payment not confirmed')).toBeInTheDocument();
      expect(
        screen.getByText(`1 ZEC to Account ${kind === 'send' ? 2 : 1}. Connection lost`),
      ).toBeInTheDocument();
      expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
      await userEvent.click(screen.getByRole('button', { name: 'View payment' }));
      const updates = screen.getByRole('region', { name: 'Payment updates' });
      const heading = within(updates).getByRole('heading', {
        name: kind === 'send' ? 'Account 1 → Account 2 · 1 ZEC' : 'Faucet → Account 1 · 1 ZEC',
      });
      await waitFor(() => expect(heading).toHaveFocus());
      expect(heading.closest('article')).toHaveAttribute('data-payment-key', key);
      expect(screen.getByRole('button', { name: 'All 3' })).toHaveAttribute('aria-pressed', 'true');
      expect(within(updates).getByText('Unknown')).toBeInTheDocument();
      expect(localStorage.getItem(`ths:${kind}:retained:${key}`)).not.toBeNull();
      expect(post).toHaveBeenCalledTimes(1);
      await userEvent.keyboard('{Escape}');
      await waitFor(() => expect(trigger).toHaveFocus());

      await userEvent.click(
        screen.getByRole('button', {
          name: kind === 'send' ? 'Send from Account 1' : 'Fund Account 1',
        }),
      );
      await userEvent.clear(screen.getByLabelText('Amount (ZEC)'));
      await userEvent.type(screen.getByLabelText('Amount (ZEC)'), '2');
      if (kind === 'send')
        await userEvent.type(screen.getByLabelText('Memo (optional)'), 'Keep this draft');
      await userEvent.click(
        within(screen.getByRole('dialog')).getByRole('button', {
          name: kind === 'send' ? 'Send ZEC' : 'Add funds',
        }),
      );
      await waitFor(() => expect(post).toHaveBeenCalledTimes(2));
      const viewFromDraft = await within(screen.getByRole('dialog')).findByRole('button', {
        name: 'View payment',
      });
      payments.set(key, {
        id: key,
        kind,
        from_account: kind === 'send' ? 1 : null,
        to_account: kind === 'send' ? 2 : 1,
        to_address: null,
        source_pool: 'ironwood',
        destination_pool: 'ironwood',
        amount_zatoshi: 100_000_000n,
        txid: '',
        block_hash: null,
        status: 'failed',
        last_error: 'Insufficient funds',
        created_at: '2026-10-12 00:00:00',
      });
      await act(() => client.refetchQueries({ queryKey: queryKeys.payment(key) }));
      expect(await screen.findByText('Payment failed', { exact: true })).toBeInTheDocument();
      const underlying = screen.getByRole('dialog');
      await userEvent.click(viewFromDraft);
      // Inspecting a failure alone must not discard the underlying draft.
      expect(within(underlying).getByLabelText('Amount (ZEC)')).toHaveValue('2');

      // Each explicit new-payment request replaces the draft, including a
      // second selection of the same failed identity for the same account.
      for (let selection = 0; selection < 2; selection++) {
        if (selection === 1) {
          await userEvent.clear(screen.getByLabelText('Amount (ZEC)'));
          await userEvent.type(screen.getByLabelText('Amount (ZEC)'), '3');
          await userEvent.click(
            within(screen.getByRole('dialog')).getByRole('button', {
              name: kind === 'send' ? 'Send ZEC' : 'Add funds',
            }),
          );
          await waitFor(() => expect(post).toHaveBeenCalledTimes(3));
          await userEvent.click(
            await within(screen.getByRole('dialog')).findByRole('button', {
              name: 'View payment',
            }),
          );
        }
        const failedHeading = within(
          screen.getByRole('region', { name: 'Payment updates' }),
        ).getByRole('heading', { name: heading.textContent });
        await userEvent.click(
          within(failedHeading.closest('article')!).getByRole('button', {
            name: kind === 'send' ? 'New Send' : 'Request funds',
          }),
        );
        await waitFor(() => expect(screen.getByLabelText('Amount (ZEC)')).toHaveValue('1'));
        if (kind === 'send') expect(screen.getByLabelText('Memo (optional)')).toHaveValue('');
        expect(post).toHaveBeenCalledTimes(selection + 2);
      }
    },
  );

  it('uses the wrapping page rhythm the explorer and network surfaces share', async () => {
    const { container } = renderWallet();
    await waitFor(() => expect(screen.getByText('Account 1')).toBeInTheDocument());

    expect(container.firstElementChild).toHaveClass('grid', 'gap-4');
    expect(screen.getByRole('button', { name: 'Send ZEC' }).parentElement).toHaveClass('flex-wrap');
  });

  it('opens a payment request dialog beside the send action', async () => {
    renderWallet();
    const send = await screen.findByRole('button', { name: 'Send ZEC' });
    const request = screen.getByRole('button', { name: 'Request' });
    expect(request.parentElement).toBe(send.parentElement);

    await userEvent.click(request);
    expect(screen.getByRole('dialog', { name: 'Request ZEC' })).toBeInTheDocument();
  });

  it('fills Send from a generated payment request', async () => {
    Object.assign(Element.prototype, {
      hasPointerCapture: () => false,
      releasePointerCapture: () => undefined,
      scrollIntoView: () => undefined,
    });
    renderWallet();
    await screen.findByText('Account 2');

    await userEvent.click(screen.getByRole('button', { name: 'Request' }));
    await userEvent.click(screen.getByLabelText('Receiving account'));
    await userEvent.click(await screen.findByRole('option', { name: 'Account 2' }));
    await userEvent.type(screen.getByLabelText('Amount (ZEC, optional)'), '1.23456789');
    await userEvent.type(screen.getByLabelText('Memo (optional)'), 'rent for October');
    await userEvent.click(screen.getByRole('button', { name: 'Generate request' }));
    expect(
      (await screen.findByRole('region', { name: 'Generated payment request' })).querySelector(
        'code',
      ),
    ).toHaveTextContent(PAYMENT_URI);

    await userEvent.click(screen.getByRole('button', { name: 'Close dialog' }));
    await userEvent.click(screen.getByRole('button', { name: 'Send ZEC' }));
    const uri = screen.getByLabelText('Payment request (optional)');
    await userEvent.click(uri);
    await userEvent.paste(PAYMENT_URI);
    await userEvent.click(screen.getByRole('button', { name: 'Apply' }));

    await waitFor(() =>
      expect(screen.getByLabelText('Destination')).toHaveTextContent('Account 2'),
    );
    expect(screen.getByLabelText('Amount (ZEC)')).toHaveValue('1.23456789');
    expect(screen.getByLabelText('Memo (optional)')).toHaveValue('rent for October');
  });

  it('does not dump a full transaction id into the activity table', async () => {
    renderWallet();
    const tx = await screen.findByTitle(TXID);
    expect(tx).not.toHaveTextContent(TXID);
    expect(tx.textContent?.includes('…')).toBe(true);
  });

  it.each(['failed', 'broadcast'])(
    'describes %s payment errors and links only prepared transactions',
    async (status) => {
      renderWallet(status);
      expect(await screen.findByText('1 event')).toBeInTheDocument();
      const transfer = screen.getByRole('cell', { name: 'Faucet → Account 1' });
      const row = transfer.closest('tr')!;
      const detail = row.nextElementSibling as HTMLElement;
      const message =
        status === 'failed'
          ? 'Failed: Insufficient funds'
          : 'Latest execution error: Lightwalletd unavailable';
      expect(within(detail).getByText(message)).toBeInTheDocument();
      expect(transfer).toHaveAccessibleDescription(message);
      expect(within(detail).queryByRole('link')).not.toBeInTheDocument();
      if (status === 'failed') {
        expect(within(row).getByText('Not prepared')).toBeInTheDocument();
        expect(within(row).queryByRole('link')).not.toBeInTheDocument();
      } else {
        const link = within(transfer).getByRole('link');
        expect(link).toHaveAccessibleDescription(message);
        expect(link).toHaveAttribute('href', `/explorer/tx/${TXID}`);
      }
    },
  );

  it('combines activity filters in the server request', async () => {
    Object.assign(Element.prototype, {
      hasPointerCapture: () => false,
      releasePointerCapture: () => undefined,
      scrollIntoView: () => undefined,
    });
    const requests: string[] = [];
    vi.spyOn(globalThis, 'fetch').mockImplementation((input) => {
      const url = requestUrl(input);
      if (url.includes('/accounts')) return json([]);
      if (url.includes('/activity')) {
        requests.push(url);
        return json([activityRow('filtered', { kind: 'send', status: 'failed', to_account: 2 })]);
      }
      return json({ instance: 'default', node: null, account_count: 5 });
    });
    renderWithProviders(
      <MemoryRouter>
        <WalletActionsProvider>
          <WalletPage />
        </WalletActionsProvider>
      </MemoryRouter>,
    );
    await screen.findByText('1 event');

    await userEvent.click(screen.getByLabelText('Account'));
    await userEvent.click(await screen.findByRole('option', { name: 'Account 2' }));
    await userEvent.click(screen.getByLabelText('Kind'));
    await userEvent.click(await screen.findByRole('option', { name: 'Send' }));
    await userEvent.click(screen.getByLabelText('Status'));
    await userEvent.click(await screen.findByRole('option', { name: 'Failed' }));

    await waitFor(() =>
      expect(requests).toContain('/api/v1/activity?limit=10&account=2&kind=send&status=failed'),
    );
  });

  it('loads the next activity page from the last displayed id', async () => {
    const firstPage = Array.from({ length: 10 }, (_, index) => activityRow(`page-${index + 1}`));
    const requests: string[] = [];
    vi.spyOn(globalThis, 'fetch').mockImplementation((input) => {
      const url = requestUrl(input);
      if (url.includes('/accounts')) return json([]);
      if (url.includes('/activity')) {
        requests.push(url);
        return url.includes('before=page-10')
          ? json([activityRow('page-11')], { 'x-ths-has-more': 'false' })
          : json(firstPage, { 'x-ths-has-more': 'true' });
      }
      return json({ instance: 'default', node: null, account_count: 5 });
    });
    renderWithProviders(
      <MemoryRouter>
        <WalletActionsProvider>
          <WalletPage />
        </WalletActionsProvider>
      </MemoryRouter>,
    );

    expect(await screen.findByText('10 events')).toBeInTheDocument();
    expect(screen.getByText('Page 1')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'First' })).toBeDisabled();
    await userEvent.click(screen.getByRole('button', { name: 'Next page' }));
    expect(await screen.findByText('1 event')).toBeInTheDocument();
    expect(screen.getByText('Page 2')).toBeInTheDocument();
    expect(requests).toContain('/api/v1/activity?limit=10&before=page-10');
    expect(screen.getByRole('button', { name: 'Next page' })).toBeDisabled();
    await userEvent.click(screen.getByRole('button', { name: 'Previous page' }));
    expect(await screen.findByText('10 events')).toBeInTheDocument();
    expect(screen.getByText('Page 1')).toBeInTheDocument();
  });

  it('disables next when the first page contains exactly the selected number of entries', async () => {
    const exactPage = Array.from({ length: 10 }, (_, index) => activityRow(`exact-${index + 1}`));
    vi.spyOn(globalThis, 'fetch').mockImplementation((input) => {
      const url = requestUrl(input);
      if (url.includes('/accounts')) return json([]);
      if (url.includes('/activity')) {
        return json(exactPage, { 'x-ths-has-more': 'false' });
      }
      return json({ instance: 'default', node: null, account_count: 5 });
    });
    renderWithProviders(
      <MemoryRouter>
        <WalletActionsProvider>
          <WalletPage />
        </WalletActionsProvider>
      </MemoryRouter>,
    );

    expect(await screen.findByText('10 events')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Next page' })).toBeDisabled();
    expect(screen.getByRole('button', { name: 'Last' })).toBeDisabled();
  });

  it('loads every remaining cursor page before showing the last page', async () => {
    const firstPage = Array.from({ length: 10 }, (_, index) => activityRow(`first-${index + 1}`));
    const secondPage = Array.from({ length: 10 }, (_, index) => activityRow(`second-${index + 1}`));
    const requests: string[] = [];
    vi.spyOn(globalThis, 'fetch').mockImplementation((input) => {
      const url = requestUrl(input);
      if (url.includes('/accounts')) return json([]);
      if (url.includes('/activity')) {
        requests.push(url);
        if (url.includes('before=second-10')) {
          return json([activityRow('last-1')], { 'x-ths-has-more': 'false' });
        }
        if (url.includes('before=first-10')) {
          return json(secondPage, { 'x-ths-has-more': 'true' });
        }
        return json(firstPage, { 'x-ths-has-more': 'true' });
      }
      return json({ instance: 'default', node: null, account_count: 5 });
    });
    renderWithProviders(
      <MemoryRouter>
        <WalletActionsProvider>
          <WalletPage />
        </WalletActionsProvider>
      </MemoryRouter>,
    );

    expect(await screen.findByText('Page 1')).toBeInTheDocument();
    await userEvent.click(screen.getByRole('button', { name: 'Last' }));

    expect(await screen.findByText('Page 3')).toBeInTheDocument();
    expect(screen.getByText('1 event')).toBeInTheDocument();
    expect(requests).toContain('/api/v1/activity?limit=10&before=first-10');
    expect(requests).toContain('/api/v1/activity?limit=10&before=second-10');
    expect(screen.getByRole('button', { name: 'Last' })).toBeDisabled();

    await userEvent.click(screen.getByRole('button', { name: 'Previous page' }));
    expect(await screen.findByText('Page 2')).toBeInTheDocument();
    expect(screen.getByText('10 events')).toBeInTheDocument();
  });

  it('reloads the first page when an activity cursor becomes stale', async () => {
    const firstPage = Array.from({ length: 10 }, (_, index) => activityRow(`stale-${index + 1}`));
    let firstPageReads = 0;
    vi.spyOn(globalThis, 'fetch').mockImplementation((input) => {
      const url = requestUrl(input);
      if (url.includes('/accounts')) return json([]);
      if (url.includes('before=stale-10')) {
        return Promise.resolve(
          Response.json(
            {
              error: {
                message: 'activity cursor is stale; reload from the first page',
                status: 409,
              },
            },
            { status: 409 },
          ),
        );
      }
      if (url.includes('/activity')) {
        firstPageReads += 1;
        return json(firstPageReads === 1 ? firstPage : [activityRow('refreshed')], {
          'x-ths-has-more': firstPageReads === 1 ? 'true' : 'false',
        });
      }
      return json({ instance: 'default', node: null, account_count: 5 });
    });
    renderWithProviders(
      <MemoryRouter>
        <WalletActionsProvider>
          <WalletPage />
        </WalletActionsProvider>
      </MemoryRouter>,
    );

    expect(await screen.findByText('10 events')).toBeInTheDocument();
    await userEvent.click(screen.getByRole('button', { name: 'Next page' }));
    expect(await screen.findByText('1 event')).toBeInTheDocument();
    expect(screen.getByText('Page 1')).toBeInTheDocument();
    expect(firstPageReads).toBe(2);
    expect(screen.queryByText(/cursor is stale/i)).not.toBeInTheDocument();
  });

  it('changes the activity page size and starts again from the first page', async () => {
    Object.assign(Element.prototype, {
      hasPointerCapture: () => false,
      releasePointerCapture: () => undefined,
      scrollIntoView: () => undefined,
    });
    const requests: string[] = [];
    vi.spyOn(globalThis, 'fetch').mockImplementation((input) => {
      const url = requestUrl(input);
      if (url.includes('/accounts')) return json([]);
      if (url.includes('/activity')) {
        requests.push(url);
        return json([]);
      }
      return json({ instance: 'default', node: null, account_count: 5 });
    });
    renderWithProviders(
      <MemoryRouter>
        <WalletActionsProvider>
          <WalletPage />
        </WalletActionsProvider>
      </MemoryRouter>,
    );

    const pageSize = await screen.findByRole('combobox', { name: /Show.*entries/ });
    expect(pageSize).toHaveTextContent('10');
    expect(requests).toContain('/api/v1/activity?limit=10');
    await userEvent.click(pageSize);
    await userEvent.click(await screen.findByRole('option', { name: '25' }));
    await waitFor(() => expect(requests).toContain('/api/v1/activity?limit=25'));
    expect(screen.getByRole('combobox', { name: /Show.*entries/ })).toHaveTextContent('25');
    expect(requests.at(-1)).not.toContain('before=');
  });
});

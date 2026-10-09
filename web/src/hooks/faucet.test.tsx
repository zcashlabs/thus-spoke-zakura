import { act, renderHook, waitFor } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import type { ReactNode } from 'react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { api, type Activity } from '@/lib/api';
import { useFaucet } from './mutations';

const request = { account_id: 1, pool: 'ironwood' as const, amount_zatoshi: 100_000_000n };
const storageKey = 'ths:faucet:1:ironwood:100000000';
const tabStorageKey = `${storageKey}:tab`;
const testLocks = Object.getOwnPropertyDescriptor(navigator, 'locks')!;

function confirmed(id: string): Activity {
  return {
    id,
    kind: 'faucet',
    from_account: null,
    to_account: 1,
    to_address: null,
    source_pool: 'ironwood',
    destination_pool: 'ironwood',
    amount_zatoshi: 100_000_000n,
    txid: id,
    block_hash: 'b'.repeat(64),
    status: 'confirmed',
    created_at: '2026-10-06 00:00:00',
  };
}

function pending(id: string): Activity {
  return { ...confirmed(id), status: 'broadcast', block_hash: null };
}

function tab() {
  const client = new QueryClient({ defaultOptions: { mutations: { retry: false } } });
  const wrapper = ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={client}>{children}</QueryClientProvider>
  );
  return renderHook(useFaucet, { wrapper });
}

beforeEach(() => {
  localStorage.clear();
  sessionStorage.clear();
});
afterEach(() => {
  vi.restoreAllMocks();
  Object.defineProperty(navigator, 'locks', testLocks);
});

describe('faucet operation ownership', () => {
  it('a delayed confirmation cannot erase a newer payment in another tab', async () => {
    const replies: Array<(activity: Activity) => void> = [];
    const post = vi
      .spyOn(api, 'faucet')
      .mockImplementation(() => new Promise<Activity>((resolve) => replies.push(resolve)));
    const first = tab();
    const second = tab();

    let firstResult!: Promise<unknown>;
    let secondResult!: Promise<unknown>;
    act(() => {
      firstResult = first.result.current.mutateAsync(request);
      secondResult = second.result.current.mutateAsync(request);
    });
    await waitFor(() => expect(post).toHaveBeenCalledTimes(2));
    const originalKey = post.mock.calls[0]?.[0].idempotency_key;
    expect(post.mock.calls[1]?.[0].idempotency_key).toBe(originalKey);

    await act(async () => {
      replies[1]?.(confirmed('first-payment'));
      await secondResult;
    });
    expect(localStorage.getItem(storageKey)).toBeNull();

    let nextResult!: Promise<unknown>;
    act(() => {
      nextResult = second.result.current.mutateAsync(request);
    });
    await waitFor(() => expect(post).toHaveBeenCalledTimes(3));
    const newKey = post.mock.calls[2]?.[0].idempotency_key;
    expect(newKey).not.toBe(originalKey);
    expect(localStorage.getItem(storageKey)).toBe(newKey);

    await act(async () => {
      replies[0]?.(confirmed('first-payment'));
      await firstResult;
    });
    expect(localStorage.getItem(storageKey)).toBe(newKey);

    await act(async () => {
      replies[2]?.(confirmed('second-payment'));
      await nextResult;
    });
    expect(localStorage.getItem(storageKey)).toBeNull();
  });

  it('a pending tab retries its own key after another tab starts a new payment', async () => {
    const replies: Array<(activity: Activity) => void> = [];
    const post = vi
      .spyOn(api, 'faucet')
      .mockImplementation(() => new Promise<Activity>((resolve) => replies.push(resolve)));
    const first = tab();
    const second = tab();
    let firstResult!: Promise<unknown>;
    let secondResult!: Promise<unknown>;
    act(() => {
      firstResult = first.result.current.mutateAsync(request);
      secondResult = second.result.current.mutateAsync(request);
    });
    await waitFor(() => expect(post).toHaveBeenCalledTimes(2));
    const originalKey = post.mock.calls[0]?.[0].idempotency_key;
    expect(post.mock.calls[1]?.[0].idempotency_key).toBe(originalKey);

    await act(async () => {
      replies[0]?.(pending('first-payment'));
      await firstResult;
      replies[1]?.(confirmed('first-payment'));
      await secondResult;
    });
    expect(localStorage.getItem(storageKey)).toBeNull();

    let nextResult!: Promise<unknown>;
    act(() => {
      nextResult = second.result.current.mutateAsync(request);
    });
    await waitFor(() => expect(post).toHaveBeenCalledTimes(3));
    const newKey = post.mock.calls[2]?.[0].idempotency_key;
    expect(newKey).not.toBe(originalKey);

    let retryResult!: Promise<unknown>;
    act(() => {
      retryResult = first.result.current.mutateAsync(request);
    });
    await waitFor(() => expect(post).toHaveBeenCalledTimes(4));
    expect(post.mock.calls[3]?.[0].idempotency_key).toBe(originalKey);

    await act(async () => {
      replies[3]?.(confirmed('first-payment'));
      await retryResult;
      replies[2]?.(pending('second-payment'));
      await nextResult;
    });
    expect(localStorage.getItem(storageKey)).toBe(newKey);
  });

  it('a reloaded pending tab keeps its key when another tab owns the shared key', async () => {
    const pendingKey = crypto.randomUUID();
    const newKey = crypto.randomUUID();
    sessionStorage.setItem(tabStorageKey, pendingKey);
    localStorage.setItem(storageKey, newKey);
    const post = vi.spyOn(api, 'faucet').mockResolvedValue(pending('first-payment'));
    const hook = tab();

    await act(async () => {
      await hook.result.current.mutateAsync(request);
    });
    expect(post.mock.calls[0]?.[0].idempotency_key).toBe(pendingKey);
    expect(localStorage.getItem(storageKey)).toBe(newKey);
  });

  it('retries a migrated legacy key until this tab sees confirmation', async () => {
    const legacyKey = crypto.randomUUID();
    sessionStorage.setItem(storageKey, legacyKey);
    const post = vi
      .spyOn(api, 'faucet')
      .mockRejectedValueOnce(new TypeError('response lost'))
      .mockResolvedValueOnce(confirmed('legacy-payment'))
      .mockResolvedValueOnce(pending('new-payment'));
    const first = tab();

    await act(async () => {
      await expect(first.result.current.mutateAsync(request)).rejects.toThrow('response lost');
    });
    expect(post.mock.calls[0]?.[0].idempotency_key).toBe(legacyKey);
    expect(localStorage.getItem(storageKey)).toBe(legacyKey);
    expect(sessionStorage.getItem(storageKey)).toBeNull();
    expect(sessionStorage.getItem(tabStorageKey)).toBe(legacyKey);

    // Another tab clears the shared key after confirmation, leaving this tab's session intact.
    first.unmount();
    const reloaded = tab();
    localStorage.removeItem(storageKey);

    await act(async () => {
      await reloaded.result.current.mutateAsync(request);
    });
    expect(post.mock.calls[1]?.[0].idempotency_key).toBe(legacyKey);
    expect(sessionStorage.getItem(tabStorageKey)).toBeNull();

    await act(async () => {
      await reloaded.result.current.mutateAsync(request);
    });
    expect(post.mock.calls[2]?.[0].idempotency_key).not.toBe(legacyKey);
  });

  it('keeps a migrated legacy payment pending when another tab owns a newer shared key', async () => {
    const legacyKey = crypto.randomUUID();
    const sharedKey = crypto.randomUUID();
    sessionStorage.setItem(storageKey, legacyKey);
    localStorage.setItem(storageKey, sharedKey);
    const post = vi
      .spyOn(api, 'faucet')
      .mockResolvedValueOnce(pending('legacy-payment'))
      .mockResolvedValueOnce(confirmed('legacy-payment'))
      .mockResolvedValueOnce(pending('shared-payment'));
    const hook = tab();

    await act(async () => {
      await hook.result.current.mutateAsync(request);
    });
    expect(post.mock.calls[0]?.[0].idempotency_key).toBe(legacyKey);
    expect(sessionStorage.getItem(storageKey)).toBeNull();
    expect(sessionStorage.getItem(tabStorageKey)).toBe(legacyKey);
    expect(localStorage.getItem(storageKey)).toBe(sharedKey);

    await act(async () => {
      await hook.result.current.mutateAsync(request);
    });
    expect(post.mock.calls[1]?.[0].idempotency_key).toBe(legacyKey);
    expect(sessionStorage.getItem(tabStorageKey)).toBeNull();
    expect(localStorage.getItem(storageKey)).toBe(sharedKey);

    await act(async () => {
      await hook.result.current.mutateAsync(request);
    });
    expect(post.mock.calls[2]?.[0].idempotency_key).toBe(sharedKey);
  });

  it('two tabs starting together share one key before either POST', async () => {
    let releaseFirst!: () => void;
    const gate = new Promise<void>((resolve) => {
      releaseFirst = resolve;
    });
    let tail = Promise.resolve();
    let acquisitions = 0;
    const lock = vi.fn((name: string, callback: (lock: Lock) => unknown) => {
      const previous = tail;
      let release!: () => void;
      tail = new Promise<void>((resolve) => {
        release = resolve;
      });
      return previous.then(async () => {
        try {
          if (++acquisitions === 1) await gate;
          return await callback({ name, mode: 'exclusive' });
        } finally {
          release();
        }
      });
    });
    Object.defineProperty(navigator, 'locks', { configurable: true, value: { request: lock } });
    const post = vi.spyOn(api, 'faucet').mockResolvedValue(pending('pending-payment'));
    const first = tab();
    const second = tab();

    let firstResult!: Promise<unknown>;
    let secondResult!: Promise<unknown>;
    act(() => {
      firstResult = first.result.current.mutateAsync(request);
      secondResult = second.result.current.mutateAsync(request);
    });
    await waitFor(() => expect(lock).toHaveBeenCalledTimes(2));
    expect(lock.mock.calls[0]?.[0]).toBe(storageKey);
    expect(lock.mock.calls[1]?.[0]).toBe(storageKey);
    expect(post).not.toHaveBeenCalled();

    await act(async () => {
      releaseFirst();
      await Promise.all([firstResult, secondResult]);
    });
    expect(post).toHaveBeenCalledTimes(2);
    expect(post.mock.calls[0]?.[0].idempotency_key).toBe(post.mock.calls[1]?.[0].idempotency_key);
    expect(localStorage.getItem(storageKey)).toBe(post.mock.calls[0]?.[0].idempotency_key);
  });

  it('does not send a payment when cross-tab coordination is unavailable', async () => {
    Object.defineProperty(navigator, 'locks', { configurable: true, value: undefined });
    const post = vi.spyOn(api, 'faucet');
    const hook = tab();

    await act(async () => {
      await expect(hook.result.current.mutateAsync(request)).rejects.toThrow(
        'cannot coordinate faucet payments',
      );
    });
    expect(post).not.toHaveBeenCalled();
    expect(localStorage.getItem(storageKey)).toBeNull();
  });
});

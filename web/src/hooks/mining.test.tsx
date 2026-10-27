import { act, renderHook, waitFor } from '@testing-library/react';
import {
  focusManager,
  onlineManager,
  QueryClient,
  QueryClientProvider,
} from '@tanstack/react-query';
import type { ReactNode } from 'react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { api, type MiningJob } from '@/lib/api';
import { useStartMining } from './mutations';
import { queryKeys, useMiningJob } from './queries';

const job: MiningJob = {
  id: '588e9911-20b6-4c4a-a9ae-5f6c1abae953',
  requested_blocks: 10,
  completed_blocks: 2,
  state: 'mining',
  error: null,
  progress_uncertain: false,
};
function setup() {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: 3 } },
  });
  const wrapper = ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={client}>{children}</QueryClientProvider>
  );
  return { client, wrapper };
}
beforeEach(() => sessionStorage.clear());
afterEach(() => {
  vi.restoreAllMocks();
  vi.useRealTimers();
  focusManager.setFocused(undefined);
});

describe('mining hooks', () => {
  it('publishes an accepted job after unmount without reacquiring denied session storage', async () => {
    let accept!: (job: MiningJob) => void;
    const post = vi.spyOn(api, 'startMining').mockImplementation(
      () =>
        new Promise<MiningJob>((resolve) => {
          accept = resolve;
        }),
    );
    const { client, wrapper } = setup();
    client.setQueryData(queryKeys.mining, { job: null });
    const invalidate = vi.spyOn(client, 'invalidateQueries');
    const hook = renderHook(useStartMining, { wrapper });
    let submission!: Promise<MiningJob>;
    act(() => {
      submission = hook.result.current.mutateAsync(10);
    });
    await waitFor(() => expect(post).toHaveBeenCalledTimes(1));
    const storage = sessionStorage;
    const retainedKey = storage.getItem('ths:mine:10');
    expect(retainedKey).toBe(post.mock.calls[0]?.[1]);
    hook.unmount();
    const getter = vi.spyOn(window, 'sessionStorage', 'get').mockImplementation(() => {
      throw new DOMException('Access denied', 'SecurityError');
    });

    await act(async () => {
      accept(job);
      await expect(submission).resolves.toEqual(job);
    });
    expect(getter).not.toHaveBeenCalled();
    expect(client.getQueryData(queryKeys.mining)).toEqual({ job });
    expect(invalidate).toHaveBeenCalledWith({ queryKey: queryKeys.mining });
    expect(storage.getItem('ths:mine:10')).toBeNull();
    expect(post).toHaveBeenCalledTimes(1);
  });

  it('preserves the accepted result and reports cleanup failure without resubmitting', async () => {
    const post = vi.spyOn(api, 'startMining').mockImplementation(() => {
      vi.spyOn(Storage.prototype, 'removeItem').mockImplementation(() => {
        throw new DOMException('Access denied', 'SecurityError');
      });
      return Promise.resolve(job);
    });
    const { client, wrapper } = setup();
    const hook = renderHook(useStartMining, { wrapper });
    await act(async () => {
      await expect(hook.result.current.mutateAsync(10)).resolves.toMatchObject(job);
    });
    expect(hook.result.current.isSuccess).toBe(true);
    expect(hook.result.current.cleanupError).toBe('Access denied');
    expect(client.getQueryData(queryKeys.mining)).toEqual({ job });
    expect(sessionStorage.getItem('ths:mine:10')).toBe(post.mock.calls[0]?.[1]);
    expect(post).toHaveBeenCalledTimes(1);
  });

  it('preserves an uncertain request key when a storage failure blocks its retry', async () => {
    const post = vi.spyOn(api, 'startMining').mockRejectedValue(new Error('response lost'));
    const first = setup();
    const hook = renderHook(useStartMining, { wrapper: first.wrapper });
    await act(async () => {
      await expect(hook.result.current.mutateAsync(10)).rejects.toThrow('response lost');
    });
    const retainedKey = sessionStorage.getItem('ths:mine:10');
    expect(retainedKey).toBe(post.mock.calls[0]?.[1]);
    const write = vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => {
      throw new DOMException('Access denied', 'SecurityError');
    });
    await act(async () => {
      await expect(hook.result.current.mutateAsync(10)).rejects.toThrow(/browser session storage/i);
    });
    expect(post).toHaveBeenCalledTimes(1);
    expect(sessionStorage.getItem('ths:mine:10')).toBe(retainedKey);
    write.mockRestore();
    hook.unmount();

    post.mockResolvedValue(job);
    const second = setup();
    const retry = renderHook(useStartMining, { wrapper: second.wrapper });
    await act(async () => {
      await retry.result.current.mutateAsync(10);
    });
    expect(post).toHaveBeenLastCalledWith(10, retainedKey);
    expect(sessionStorage.getItem('ths:mine:10')).toBeNull();
  });

  it.each(['network', 'validation'])(
    'retains the POST key after a %s failure across a fresh cache',
    async (failure) => {
      const post = vi.spyOn(globalThis, 'fetch').mockImplementation(async (_input, init) => {
        await Promise.resolve();
        if (init?.method !== 'POST') return new Response(JSON.stringify({ job }));
        if (post.mock.calls.filter(([, options]) => options?.method === 'POST').length === 1) {
          if (failure === 'network') throw new Error('response lost');
          return new Response(JSON.stringify({ ...job, id: 'invalid' }), { status: 202 });
        }
        return new Response(JSON.stringify(job), { status: 202 });
      });
      const first = setup();
      const hook = renderHook(useStartMining, { wrapper: first.wrapper });
      await act(async () => {
        await expect(hook.result.current.mutateAsync(10)).rejects.toThrow();
      });
      expect(post.mock.calls.filter(([, init]) => init?.method === 'POST')).toHaveLength(1);
      hook.unmount();
      const second = setup();
      const invalidate = vi.spyOn(second.client, 'invalidateQueries');
      const retry = renderHook(useStartMining, { wrapper: second.wrapper });
      await act(async () => {
        await retry.result.current.mutateAsync(10);
      });
      expect(second.client.getQueryData(queryKeys.mining)).toEqual({ job });
      expect(invalidate).toHaveBeenCalledWith({ queryKey: queryKeys.mining });
      expect(invalidate.mock.calls.every(([options]) => options?.queryKey?.[0] === 'mining')).toBe(
        true,
      );
      await act(async () => {
        await retry.result.current.mutateAsync(10);
      });
      const bodies = post.mock.calls
        .filter(([, init]) => init?.method === 'POST')
        .map(
          ([, init]) =>
            JSON.parse(typeof init?.body === 'string' ? init.body : '{}') as {
              idempotency_key: string;
            },
        );
      expect(bodies[1]?.idempotency_key).toBe(bodies[0]?.idempotency_key);
      expect(bodies[2]?.idempotency_key).not.toBe(bodies[1]?.idempotency_key);
    },
  );

  it('refetches current state after a rejected start', async () => {
    vi.spyOn(api, 'startMining').mockRejectedValue(new Error('busy'));
    const { client, wrapper } = setup();
    const invalidate = vi.spyOn(client, 'invalidateQueries');
    const hook = renderHook(useStartMining, { wrapper });
    await act(async () => {
      await expect(hook.result.current.mutateAsync(10)).rejects.toThrow('busy');
    });
    expect(invalidate).toHaveBeenCalledWith({ queryKey: queryKeys.mining });
  });

  it('refetches stale snapshots on mount and reconnect', async () => {
    const fetchJob = vi.spyOn(api, 'miningJob').mockResolvedValue({ job });
    const { client, wrapper } = setup();
    client.setQueryData(queryKeys.mining, { job: null });
    const hook = renderHook(useMiningJob, { wrapper });
    await waitFor(() => expect(hook.result.current.data?.job).toEqual(job));
    expect(fetchJob).toHaveBeenCalledTimes(1);
    onlineManager.setOnline(false);
    onlineManager.setOnline(true);
    await waitFor(() => expect(fetchJob).toHaveBeenCalledTimes(2));
    hook.unmount();
  });

  it('polls active progress without SSE and discovers jobs while idle', async () => {
    focusManager.setFocused(true);
    let current: MiningJob | null = null;
    const fetchJob = vi
      .spyOn(api, 'miningJob')
      .mockImplementation(() => Promise.resolve({ job: current }));
    const { wrapper } = setup();
    const hook = renderHook(useMiningJob, { wrapper });
    await waitFor(() => expect(hook.result.current.isSuccess).toBe(true));
    current = job;
    await waitFor(() => expect(hook.result.current.data?.job).toEqual(job), { timeout: 6500 });
    current = { ...job, completed_blocks: 3 };
    await waitFor(() => expect(hook.result.current.data?.job?.completed_blocks).toBe(3), {
      timeout: 2500,
    });
    expect(fetchJob.mock.calls.length).toBeGreaterThanOrEqual(3);
  }, 10000);
});

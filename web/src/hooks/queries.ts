import {
  useInfiniteQuery,
  useQuery,
  useQueryClient,
  type UseQueryResult,
} from '@tanstack/react-query';
import { useEffect, useMemo } from 'react';
import {
  api,
  ApiError,
  type Account,
  type Activity,
  type AddressInfo,
  type Block,
  type BlockPage,
  type MiningJob,
  type SendQuote,
  type SendQuoteInput,
  type Status,
  type Transaction,
  type ActivityListOptions,
} from '@/lib/api';

/**
 * Query keys are declared once so cache invalidation cannot drift from the
 * queries it is meant to invalidate.
 */
export const queryKeys = {
  status: ['status'] as const,
  mining: ['mining'] as const,
  accounts: ['accounts'] as const,
  activity: (limit: number) => ['activity', limit] as const,
  activityPages: (options: ActivityListOptions) => ['activity', 'pages', options] as const,
  payment: (key: string) => ['activity', 'payment', key] as const,
  blocks: (before?: number) => ['blocks', before ?? 'tip'] as const,
  block: (id: string) => ['block', id] as const,
  transaction: (txid: string) => ['transaction', txid] as const,
  address: (address: string) => ['address', address] as const,
  sendQuote: (params: SendQuoteInput) => ['send-quote', params] as const,
};

export function useStatus(): UseQueryResult<Status> {
  return useQuery({ queryKey: queryKeys.status, queryFn: api.status });
}

export function useAccounts(): UseQueryResult<Account[]> {
  return useQuery({ queryKey: queryKeys.accounts, queryFn: api.accounts });
}

export function useActivity(limit = 30): UseQueryResult<Activity[]> {
  return useQuery({ queryKey: queryKeys.activity(limit), queryFn: () => api.activity(limit) });
}

export function useActivityPages(options: ActivityListOptions = {}) {
  const limit = options.limit ?? 30;
  const { account, kind, status } = options;
  const queryClient = useQueryClient();
  const queryKey = useMemo(
    () =>
      queryKeys.activityPages({
        limit,
        ...(account === undefined ? {} : { account }),
        ...(kind === undefined ? {} : { kind }),
        ...(status === undefined ? {} : { status }),
      }),
    [account, kind, limit, status],
  );
  const query = useInfiniteQuery({
    queryKey,
    initialPageParam: undefined as string | undefined,
    queryFn: ({ pageParam }) =>
      api.activityPage({
        limit,
        ...(account === undefined ? {} : { account }),
        ...(kind === undefined ? {} : { kind }),
        ...(status === undefined ? {} : { status }),
        ...(pageParam === undefined ? {} : { before: pageParam }),
      }),
    getNextPageParam: (lastPage) => (lastPage.hasMore ? lastPage.entries.at(-1)?.id : undefined),
    retry: (failureCount, error) =>
      !(error instanceof ApiError && error.status === 409) && failureCount < 3,
  });

  // A pending row can become mined and leave the selected filter between
  // pages. The server reports that stale cursor instead of silently skipping
  // rows; restart from the first page so the dashboard heals itself.
  useEffect(() => {
    if (query.error instanceof ApiError && query.error.status === 409) {
      void queryClient.resetQueries({ queryKey, exact: true });
    }
  }, [query.error, queryClient, queryKey]);

  return query;
}

export function isTerminalPayment(activity: Activity | null | undefined): boolean {
  return activity?.status === 'confirmed' || activity?.status === 'failed';
}

export function usePaymentStatus(key: string, enabled: boolean, addressFaucet = false) {
  return useQuery({
    queryKey: queryKeys.payment(key),
    queryFn: () => (addressFaucet ? api.faucetAddressStatus(key) : api.payment(key)),
    enabled,
    retry: false,
    staleTime: 0,
    refetchOnMount: 'always',
    refetchOnWindowFocus: true,
    refetchOnReconnect: true,
    refetchInterval: (query) => (isTerminalPayment(query.state.data) ? false : 2_000),
  });
}

export function useBlocks(before?: number): UseQueryResult<BlockPage> {
  return useQuery({
    queryKey: queryKeys.blocks(before),
    queryFn: () => api.blocks(before === undefined ? {} : { before }),
  });
}

export function useBlock(id: string): UseQueryResult<Block> {
  return useQuery({
    queryKey: queryKeys.block(id),
    queryFn: () => api.block(id),
    enabled: id.length > 0,
  });
}

export function useTransaction(txid: string): UseQueryResult<Transaction> {
  return useQuery({
    queryKey: queryKeys.transaction(txid),
    queryFn: () => api.transaction(txid),
    enabled: txid.length > 0,
  });
}

/** The fee depends on the source and the destination pool, not the destination account. */
export function useSendQuote(params: SendQuoteInput): UseQueryResult<SendQuote> {
  return useQuery({
    queryKey: queryKeys.sendQuote(params),
    queryFn: () => api.sendQuote(params),
  });
}

export function useAddress(address: string): UseQueryResult<AddressInfo> {
  return useQuery({
    queryKey: queryKeys.address(address),
    queryFn: () => api.address(address),
    enabled: address.length > 0,
  });
}

export function useMiningJob(): UseQueryResult<{ job: MiningJob | null }> {
  return useQuery({
    queryKey: queryKeys.mining,
    queryFn: api.miningJob,
    staleTime: 0,
    refetchOnMount: true,
    refetchOnReconnect: true,
    refetchInterval: (query) => {
      const state = query.state.data?.job?.state;
      return state === 'mining' || state === 'syncing' ? 1_000 : 5_000;
    },
  });
}

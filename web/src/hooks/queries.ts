import { useQuery, type UseQueryResult } from '@tanstack/react-query';
import { userAccounts } from '@/lib/accounts';
import {
  api,
  type Account,
  type Activity,
  type AddressInfo,
  type Block,
  type BlockPage,
  type MiningJob,
  type Mempool,
  type SendQuote,
  type SendQuoteInput,
  type Status,
  type Transaction,
  type WalletTransaction,
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
  payment: (key: string) => ['activity', 'payment', key] as const,
  blocks: (before?: number) => ['blocks', before ?? 'tip'] as const,
  mempool: ['mempool'] as const,
  block: (id: string) => ['block', id] as const,
  transaction: (txid: string) => ['transaction', txid] as const,
  walletTransaction: (txid: string) => ['wallet-transaction', txid] as const,
  address: (address: string) => ['address', address] as const,
  sendQuote: (params: SendQuoteInput) => ['send-quote', params] as const,
};

export function useStatus(): UseQueryResult<Status> {
  return useQuery({ queryKey: queryKeys.status, queryFn: api.status });
}

export const accountsQueryOptions = { queryKey: queryKeys.accounts, queryFn: () => api.accounts() };

export function useAccounts(): UseQueryResult<Account[]> {
  return useQuery({ ...accountsQueryOptions, select: userAccounts });
}

export function useActivity(limit = 30): UseQueryResult<Activity[]> {
  return useQuery({ queryKey: queryKeys.activity(limit), queryFn: () => api.activity(limit) });
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

export function useMempool(): UseQueryResult<Mempool> {
  return useQuery({
    queryKey: queryKeys.mempool,
    queryFn: api.mempool,
    // External broadcasts do not advance the chain or emit a server event.
    refetchInterval: 2_000,
  });
}

export function useTransaction(txid: string): UseQueryResult<Transaction> {
  return useQuery({
    queryKey: queryKeys.transaction(txid),
    queryFn: () => api.transaction(txid),
    enabled: txid.length > 0,
  });
}

export function useWalletTransaction(txid: string): UseQueryResult<WalletTransaction> {
  return useQuery({
    queryKey: queryKeys.walletTransaction(txid),
    queryFn: () => api.walletTransaction(txid),
    enabled: txid.length > 0,
    // Fetching the full transaction does not always emit a server event.
    refetchInterval: (query) => (query.state.data?.details === 'pending' ? 5_000 : false),
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

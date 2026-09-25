import { useQuery, type UseQueryResult } from '@tanstack/react-query';
import {
  api,
  type Account,
  type Activity,
  type AddressInfo,
  type Block,
  type BlockPage,
  type SendQuote,
  type SendQuoteInput,
  type Status,
  type Transaction,
} from '@/lib/api';

/**
 * Query keys are declared once so cache invalidation cannot drift from the
 * queries it is meant to invalidate.
 */
export const queryKeys = {
  status: ['status'] as const,
  accounts: ['accounts'] as const,
  activity: (limit: number) => ['activity', limit] as const,
  blocks: (before?: number) => ['blocks', before ?? 'tip'] as const,
  block: (id: string) => ['block', id] as const,
  transaction: (txid: string) => ['transaction', txid] as const,
  mempool: ['mempool'] as const,
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

import { useMutation, useQueryClient, type UseMutationResult } from '@tanstack/react-query';
import { api, idempotencyKey, type Activity, type Pool } from '@/lib/api';
import { queryKeys } from './queries';

/** Everything a successful money movement invalidates. */
function walletKeys() {
  return [[...queryKeys.accounts], ['activity'], [...queryKeys.status], ['blocks']];
}

function useInvalidateWallet() {
  const queryClient = useQueryClient();
  return async () => {
    await Promise.all(
      walletKeys().map((key) =>
        queryClient.invalidateQueries({ queryKey: key, refetchType: 'active' }),
      ),
    );
  };
}

export interface SendVariables {
  from_account: number;
  to_account: number;
  source_pool: Pool;
  destination_pool: Pool;
  amount_zatoshi: bigint;
  memo?: string;
}

export function useSend(): UseMutationResult<Activity, Error, SendVariables> {
  const invalidate = useInvalidateWallet();
  return useMutation({
    mutationFn: (variables: SendVariables) =>
      api.send({ ...variables, idempotency_key: idempotencyKey() }),
    onSuccess: invalidate,
  });
}

export interface FaucetVariables {
  account_id: number;
  pool: Pool;
  amount_zatoshi: bigint;
}

export function useFaucet(): UseMutationResult<Activity, Error, FaucetVariables> {
  const invalidate = useInvalidateWallet();
  return useMutation({
    mutationFn: (variables: FaucetVariables) =>
      api.faucet({ ...variables, idempotency_key: idempotencyKey() }),
    onSuccess: invalidate,
  });
}

export function useMine(): UseMutationResult<{ blocks: number }, Error, number> {
  const invalidate = useInvalidateWallet();
  return useMutation({
    mutationFn: (blocks: number) => api.mine(blocks),
    onSuccess: invalidate,
  });
}

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

function operationKey<T>(kind: string, fingerprint: (variables: T) => string) {
  const storageKey = (variables: T) => `tsz:${kind}:${fingerprint(variables)}`;
  return {
    keyFor(variables: T) {
      const storage = storageKey(variables);
      let key = sessionStorage.getItem(storage);
      if (!key) {
        key = idempotencyKey();
        sessionStorage.setItem(storage, key);
      }
      return key;
    },
    clear(variables: T) {
      sessionStorage.removeItem(storageKey(variables));
    },
  };
}

export interface SendVariables {
  from_account: number;
  to_account: number;
  source_pool: Pool;
  destination_pool: Pool;
  amount_zatoshi: bigint;
}

export function useSend(): UseMutationResult<Activity, Error, SendVariables> {
  const invalidate = useInvalidateWallet();
  const operation = operationKey(
    'send',
    (variables: SendVariables) =>
      `${variables.from_account}:${variables.to_account}:${variables.source_pool}:${variables.destination_pool}:${variables.amount_zatoshi}`,
  );
  return useMutation({
    mutationFn: (variables: SendVariables) =>
      api.send({ ...variables, idempotency_key: operation.keyFor(variables) }),
    onSuccess: async (_activity, variables) => {
      operation.clear(variables);
      await invalidate();
    },
  });
}

export interface FaucetVariables {
  account_id: number;
  pool: Pool;
  amount_zatoshi: bigint;
}

export function useFaucet(): UseMutationResult<Activity, Error, FaucetVariables> {
  const invalidate = useInvalidateWallet();
  const operation = operationKey(
    'faucet',
    (variables: FaucetVariables) =>
      `${variables.account_id}:${variables.pool}:${variables.amount_zatoshi}`,
  );
  return useMutation({
    mutationFn: (variables: FaucetVariables) =>
      api.faucet({ ...variables, idempotency_key: operation.keyFor(variables) }),
    onSuccess: async (_activity, variables) => {
      operation.clear(variables);
      await invalidate();
    },
  });
}

export function useMine(): UseMutationResult<{ blocks: number }, Error, number> {
  const invalidate = useInvalidateWallet();
  return useMutation({
    mutationFn: (blocks: number) => api.mine(blocks),
    onSuccess: invalidate,
  });
}

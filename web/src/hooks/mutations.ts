import { useRef } from 'react';
import { useMutation, useQueryClient, type UseMutationResult } from '@tanstack/react-query';
import { api, idempotencyKey, type Activity, type MiningJob, type Pool } from '@/lib/api';
import { queryKeys } from './queries';

/** Everything a successful money movement invalidates. */
function walletKeys() {
  return [[...queryKeys.accounts], ['activity'], [...queryKeys.status], ['blocks'], ['send-quote']];
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

function operationKey<T>(
  kind: string,
  fingerprint: (variables: T) => string,
  storageArea = sessionStorage,
) {
  const storageKey = (variables: T) => `ths:${kind}:${fingerprint(variables)}`;
  return {
    nameFor: storageKey,
    keyFor(variables: T) {
      const itemKey = storageKey(variables);
      let key = storageArea.getItem(itemKey);
      if (!key) {
        key = idempotencyKey();
      }
      storageArea.setItem(itemKey, key);
      return key;
    },
    clear(variables: T) {
      storageArea.removeItem(storageKey(variables));
      if (storageArea === localStorage) sessionStorage.removeItem(storageKey(variables));
    },
    clearIfMatches(variables: T, expectedKey: string) {
      const itemKey = storageKey(variables);
      if (storageArea.getItem(itemKey) === expectedKey) storageArea.removeItem(itemKey);
      if (storageArea === localStorage && sessionStorage.getItem(itemKey) === expectedKey) {
        sessionStorage.removeItem(itemKey);
      }
    },
  };
}

function withFaucetLock<T>(name: string, action: () => T | Promise<T>): Promise<T> {
  if (!navigator.locks) {
    return Promise.reject(new Error('This browser cannot coordinate faucet payments across tabs.'));
  }
  return navigator.locks.request(name, action);
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
  const operation = operationKey('send', (variables: SendVariables) =>
    JSON.stringify([
      variables.from_account,
      variables.to_account,
      variables.source_pool,
      variables.destination_pool,
      variables.amount_zatoshi.toString(),
      variables.memo ?? null,
    ]),
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

type FaucetOperationResult = { activity: Activity; key: string };

export function useFaucet(): UseMutationResult<FaucetOperationResult, Error, FaucetVariables> {
  const invalidate = useInvalidateWallet();
  const ownedKeys = useRef(new Map<string, string>());
  const operation = operationKey(
    'faucet',
    (variables: FaucetVariables) =>
      `${variables.account_id}:${variables.pool}:${variables.amount_zatoshi}`,
    localStorage,
  );
  return useMutation({
    mutationFn: async (variables: FaucetVariables) => {
      const name = operation.nameFor(variables);
      const tabName = `${name}:tab`;
      const key = await withFaucetLock(name, () => {
        const legacyKey = sessionStorage.getItem(name);
        const ownedKey = ownedKeys.current.get(name) ?? sessionStorage.getItem(tabName);
        if (ownedKey) {
          sessionStorage.setItem(tabName, ownedKey);
          if (legacyKey !== null) sessionStorage.removeItem(name);
          ownedKeys.current.set(name, ownedKey);
          return ownedKey;
        }

        const sharedKey = localStorage.getItem(name);
        if (legacyKey) {
          if (sharedKey === null) localStorage.setItem(name, legacyKey);
          // Keep the migrated payment with this tab until it sees confirmation.
          sessionStorage.setItem(tabName, legacyKey);
          sessionStorage.removeItem(name);
          ownedKeys.current.set(name, legacyKey);
          return legacyKey;
        }
        if (legacyKey !== null) sessionStorage.removeItem(name);

        const key = sharedKey ?? operation.keyFor(variables);
        sessionStorage.setItem(tabName, key);
        ownedKeys.current.set(name, key);
        return key;
      });
      const activity = await api.faucet({ ...variables, idempotency_key: key });
      return { activity, key };
    },
    onSuccess: async ({ activity, key }, variables) => {
      if (activity.status === 'confirmed') {
        const name = operation.nameFor(variables);
        await withFaucetLock(name, () => {
          operation.clearIfMatches(variables, key);
          const tabName = `${name}:tab`;
          if (sessionStorage.getItem(tabName) === key) sessionStorage.removeItem(tabName);
          if (ownedKeys.current.get(name) === key) ownedKeys.current.delete(name);
        });
      }
      await invalidate();
    },
  });
}

export function useStartMining(): UseMutationResult<MiningJob, Error, number> {
  const queryClient = useQueryClient();
  const operation = operationKey('mine', (blocks: number) => String(blocks));
  return useMutation({
    mutationFn: (blocks: number) => {
      const state = queryClient.getQueryData<{ job: MiningJob | null }>(queryKeys.mining)?.job
        ?.state;
      if (state === 'completed' || state === 'failed') operation.clear(blocks);
      return api.startMining(blocks, operation.keyFor(blocks));
    },
    retry: false,
    onSuccess: async (job, blocks) => {
      operation.clear(blocks);
      queryClient.setQueryData(queryKeys.mining, { job });
      await queryClient.invalidateQueries({ queryKey: queryKeys.mining });
    },
    onError: async () => {
      await queryClient.invalidateQueries({ queryKey: queryKeys.mining });
    },
  });
}

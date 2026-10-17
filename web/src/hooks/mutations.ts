import { useCallback, useRef, useState } from 'react';
import { useMutation, useQueryClient, type UseMutationResult } from '@tanstack/react-query';
import {
  api,
  errorMessage,
  idempotencyKey,
  type Activity,
  type MiningJob,
  type PaymentRequest,
  type PaymentUri,
} from '@/lib/api';
import { isTerminalPayment, queryKeys } from './queries';
import {
  clearPayment,
  paymentName,
  retainPayment,
  retainedPaymentKey,
  type PaymentIdentity,
  type PaymentKind,
  type SendVariables,
  type FaucetVariables,
} from './payment-identity';
export type { SendVariables, FaucetVariables } from './payment-identity';

/** Everything a successful money movement invalidates. */
function walletKeys() {
  return [[...queryKeys.accounts], ['activity'], [...queryKeys.status], ['blocks'], ['send-quote']];
}

function useInvalidateWallet() {
  const queryClient = useQueryClient();
  return useCallback(async () => {
    await Promise.all(
      walletKeys().map((key) =>
        queryClient.invalidateQueries({ queryKey: key, refetchType: 'active' }),
      ),
    );
  }, [queryClient]);
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
  };
}

export type PaymentResult = { activity: Activity; key: string; cleanupError?: string };
type PaymentSubmission<T> = { variables: T; key: string | undefined; error?: unknown };
type PaymentCallbacks<T> = {
  onError?: (error: Error, variables: T, key: string | undefined) => void;
  onSuccess?: (result: PaymentResult, variables: T) => void;
};
type PaymentLifecycle = {
  onRetained: (identity: PaymentIdentity) => void;
  onError: (
    error: Error,
    variables: SendVariables | FaucetVariables,
    key: string | undefined,
  ) => void;
};

function usePaymentMutation<T extends SendVariables | FaucetVariables>(
  kind: PaymentKind,
  post: (variables: T, key: string) => Promise<Activity>,
  lifecycle?: PaymentLifecycle,
) {
  const queryClient = useQueryClient();
  const invalidate = useInvalidateWallet();
  const ownedKeys = useRef(new Map<string, string>());
  const submissions = useRef(new Set<PaymentSubmission<T>>());
  const [pendingSubmissions, setPendingSubmissions] = useState<PaymentSubmission<T>[]>([]);
  const updateSubmissions = () =>
    setPendingSubmissions([...submissions.current].map((item) => ({ ...item })));
  const matches = useCallback(
    (item: PaymentSubmission<T>, identity: PaymentIdentity) =>
      item.key === identity.key ||
      (item.key === undefined && paymentName(kind, item.variables) === identity.name),
    [kind],
  );
  const clear = useCallback(
    async (identity: PaymentIdentity) => {
      try {
        await clearPayment(identity, ownedKeys.current);
      } finally {
        void invalidate();
      }
    },
    [invalidate],
  );
  const forget = useCallback(
    (identity: PaymentIdentity, onForgot: () => void) =>
      clearPayment(
        identity,
        ownedKeys.current,
        () => {
          const status = queryClient.getQueryState<Activity | null>(
            queryKeys.payment(identity.key),
          );
          return (
            ![...submissions.current].some((item) => matches(item, identity)) &&
            status?.status === 'success' &&
            status.data === null
          );
        },
        onForgot,
      ),
    [queryClient, matches],
  );
  // The sender and the status card both learn of a confirmation, in an order
  // that depends on rendering; whichever sees it first announces it.
  const announced = useRef(new Set<string>());
  const claimAnnouncement = useCallback((key: string) => {
    if (announced.current.has(key)) return false;
    announced.current.add(key);
    return true;
  }, []);
  const mutation = useMutation<PaymentResult, Error, PaymentSubmission<T>>({
    mutationFn: async (submission) => {
      if ('error' in submission) throw submission.error;
      const { variables, key } = submission;
      const identity = await retainPayment(kind, variables, ownedKeys.current, key);
      submission.key = identity.key;
      updateSubmissions();
      // Publish before POST: an immediate terminal response can release storage.
      lifecycle?.onRetained(identity);
      const activity = await post(variables, identity.key);
      return { activity, key: identity.key };
    },
    retry: false,
    onError: (error, submission) => lifecycle?.onError(error, submission.variables, submission.key),
    onSuccess: async (result, { variables }) => {
      const { activity, key } = result;
      // A lookup started before acceptance must not overwrite this response.
      await queryClient.cancelQueries({ queryKey: queryKeys.payment(key) });
      queryClient.setQueryData<Activity | null>(queryKeys.payment(key), (previous) =>
        isTerminalPayment(previous) ? previous : activity,
      );
      if (isTerminalPayment(activity)) {
        try {
          await clearPayment(
            { kind, name: paymentName(kind, variables), key, variables } as PaymentIdentity,
            ownedKeys.current,
          );
        } catch (error) {
          // Cleanup cannot change the outcome; the persistent status view
          // keeps the result and offers a storage-only recovery action.
          result.cleanupError = errorMessage(error);
        }
      }
      // Query owners handle wallet refresh errors. Acceptance must not wait
      // for node-dependent views while stored payment status is available.
      void invalidate();
    },
    onSettled: (_result, _error, submission) => {
      submissions.current.delete(submission);
      updateSubmissions();
    },
  });
  const submission = (variables: T, key?: string): PaymentSubmission<T> => {
    try {
      // Capture retained identity at the user action, before a queued lock or
      // terminal status update can release it. This action only replays that key.
      const item = {
        variables,
        key: key ?? retainedPaymentKey(paymentName(kind, variables), ownedKeys.current),
      };
      // Reserve ownership at the click, before retainPayment can wait for its Web Lock.
      submissions.current.add(item);
      updateSubmissions();
      return item;
    } catch (error) {
      return {
        variables,
        key: undefined,
        error: error instanceof DOMException ? new Error(error.message, { cause: error }) : error,
      };
    }
  };
  const callbacks = (options?: PaymentCallbacks<T>) => ({
    onError: (error: Error, { variables, key }: PaymentSubmission<T>) =>
      options?.onError?.(error, variables, key),
    onSuccess: (result: PaymentResult, { variables }: PaymentSubmission<T>) =>
      options?.onSuccess?.(result, variables),
  });
  const resume = (identity: PaymentIdentity, options?: PaymentCallbacks<T>) => {
    mutation.mutate(submission(identity.variables as T, identity.key), callbacks(options));
  };
  return {
    ...mutation,
    clear,
    forget,
    resume,
    claimAnnouncement,
    isSubmitting: (identity: PaymentIdentity) =>
      pendingSubmissions.some((item) => matches(item, identity)),
    mutate: (variables: T, options?: PaymentCallbacks<T>) => {
      mutation.mutate(submission(variables), callbacks(options));
    },
    mutateAsync: (variables: T) => {
      return mutation.mutateAsync(submission(variables));
    },
  };
}

export function useSend(lifecycle?: PaymentLifecycle) {
  return usePaymentMutation(
    'send',
    (variables: SendVariables, key) => api.send({ ...variables, idempotency_key: key }),
    lifecycle,
  );
}

export function useFaucet(lifecycle?: PaymentLifecycle) {
  return usePaymentMutation(
    'faucet',
    (variables: FaucetVariables, key) =>
      'address' in variables
        ? api.faucetAddress({ ...variables, idempotency_key: key })
        : api.faucet({ ...variables, idempotency_key: key }),
    lifecycle,
  );
}

/** Resolves a pasted `zcash:` URI; it moves no funds, so nothing is invalidated. */
export function useParsePaymentUri(): UseMutationResult<PaymentUri, Error, string> {
  return useMutation({ mutationFn: (uri: string) => api.parsePaymentUri(uri) });
}

export interface PaymentRequestVariables {
  account_id: number;
  amount_zatoshi?: bigint;
  memo?: string;
}

export function usePaymentRequest(): UseMutationResult<
  PaymentRequest,
  Error,
  PaymentRequestVariables
> {
  return useMutation({ mutationFn: (variables) => api.paymentRequest(variables) });
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

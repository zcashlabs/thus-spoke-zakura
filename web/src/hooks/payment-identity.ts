import { useCallback, useEffect, useState } from 'react';
import { z } from 'zod';
import { idempotencyKey, poolSchema } from '@/lib/api';
import { shortHash } from '@/lib/format';

export interface SendVariables {
  from_account: number;
  to_account?: number;
  to_address?: string;
  source_pool: z.infer<typeof poolSchema>;
  destination_pool: z.infer<typeof poolSchema>;
  amount_zatoshi: bigint;
  memo?: string;
}

export type FaucetVariables =
  | { account_id: number; pool: z.infer<typeof poolSchema>; amount_zatoshi: bigint }
  | { address: string; amount_zatoshi: bigint };

export function paymentRecipient(variables: SendVariables | FaucetVariables) {
  if ('account_id' in variables) return `Account ${variables.account_id}`;
  if ('address' in variables) return shortHash(variables.address, 14, 6);
  return variables.to_account === undefined
    ? shortHash(variables.to_address ?? '', 14, 6)
    : `Account ${variables.to_account}`;
}

export type PaymentIdentity =
  | { kind: 'send'; name: string; key: string; variables: SendVariables }
  | { kind: 'faucet'; name: string; key: string; variables: FaucetVariables };
export type PaymentKind = PaymentIdentity['kind'];
const changed = 'ths:payment-identity';
const recordName = (kind: PaymentKind, key: string) => `ths:${kind}:retained:${key}`;

export function paymentName(kind: PaymentKind, variables: SendVariables | FaucetVariables) {
  const fingerprint =
    'from_account' in variables
      ? JSON.stringify([
          variables.from_account,
          variables.to_account ?? variables.to_address,
          variables.source_pool,
          variables.destination_pool,
          variables.amount_zatoshi.toString(),
          variables.memo ?? null,
        ])
      : 'address' in variables
        ? `address:${variables.address}:${variables.amount_zatoshi}`
        : `${variables.account_id}:${variables.pool}:${variables.amount_zatoshi}`;
  return `ths:${kind}:${fingerprint}`;
}

export async function withPaymentLock<T>(name: string, action: () => T | Promise<T>): Promise<T> {
  if (!navigator.locks) {
    throw new Error('This browser cannot coordinate payments across tabs.');
  }
  try {
    return await navigator.locks.request(name, action);
  } catch (error) {
    // DOMException is not an Error in every browser/runtime.
    if (error instanceof DOMException) throw new Error(error.message, { cause: error });
    throw error;
  }
}

export async function retainPayment(
  kind: PaymentKind,
  variables: SendVariables | FaucetVariables,
  owned: Map<string, string>,
  expectedKey?: string,
): Promise<PaymentIdentity> {
  const name = paymentName(kind, variables);
  return withPaymentLock(name, () => {
    const key = expectedKey ?? retainedPaymentKey(name, owned) ?? idempotencyKey();
    // The record survives a browser restart even if a newer shared key exists.
    // Complete every persistence step before allowing a payment POST.
    const writes: [Storage, string, string | null][] = [];
    const write = (storage: Storage, item: string, value: string) => {
      const previous = storage.getItem(item);
      if (previous === value) return;
      storage.setItem(item, value);
      writes.push([storage, item, previous]);
    };
    try {
      write(localStorage, recordName(kind, key), name);
      if (!localStorage.getItem(name)) write(localStorage, name, key);
      write(sessionStorage, `${name}:tab`, key);
      if (
        localStorage.getItem(recordName(kind, key)) !== name ||
        !localStorage.getItem(name) ||
        sessionStorage.getItem(`${name}:tab`) !== key
      ) {
        throw new Error('Could not retain the payment identity. No payment was submitted.');
      }
      sessionStorage.removeItem(name);
    } catch (error) {
      // A failed attempt must not become a resumable payment after reload.
      const errors = [error];
      for (const [storage, item, previous] of writes.reverse()) {
        try {
          if (previous === null) storage.removeItem(item);
          else storage.setItem(item, previous);
        } catch (rollbackError) {
          errors.push(rollbackError);
        }
      }
      if (errors.length > 1)
        throw new AggregateError(
          errors,
          'Could not restore payment storage. No payment was submitted.',
          { cause: error },
        );
      throw error;
    }
    owned.set(name, key);
    window.dispatchEvent(new Event(changed));
    return { kind, name, key, variables } as PaymentIdentity;
  });
}

export function retainedPaymentKey(name: string, owned: Map<string, string>) {
  return (
    owned.get(name) ??
    (sessionStorage.getItem(`${name}:tab`) || undefined) ??
    (sessionStorage.getItem(name) || undefined) ??
    (localStorage.getItem(name) || undefined)
  );
}

export async function clearPayment(
  identity: PaymentIdentity,
  owned: Map<string, string>,
  canClear?: () => boolean,
  onCleared?: () => void,
) {
  return withPaymentLock(identity.name, () => {
    if (canClear && !canClear()) return false;
    const { kind, name, key } = identity;
    if (localStorage.getItem(name) === key) localStorage.removeItem(name);
    if (sessionStorage.getItem(name) === key) sessionStorage.removeItem(name);
    if (sessionStorage.getItem(`${name}:tab`) === key) sessionStorage.removeItem(`${name}:tab`);
    if (localStorage.getItem(recordName(kind, key)) === name) {
      localStorage.removeItem(recordName(kind, key));
    }
    if (owned.get(name) === key) owned.delete(name);
    window.dispatchEvent(new Event(changed));
    // A later Resume may restore this row, so Forget must remove it before releasing the lock.
    onCleared?.();
    return true;
  });
}

const account = z.coerce.number().int().min(1).max(5);
const amount = z
  .string()
  .regex(/^[1-9]\d*$/)
  .transform(BigInt);
const sendFingerprint = z.tuple([
  account,
  z.union([account, z.string().min(1)]),
  poolSchema,
  poolSchema,
  amount,
  z.string().nullable(),
]);
const faucetFingerprint = z.tuple([account, poolSchema, amount]);
const addressFaucetFingerprint = z.tuple([z.literal('address'), z.string().min(1), amount]);

function decodeIdentity(kind: PaymentKind, name: string, key: string): PaymentIdentity {
  const fingerprint = name.slice(`ths:${kind}:`.length);
  if (kind === 'send') {
    const [from_account, destination, source_pool, destination_pool, amount_zatoshi, memo] =
      sendFingerprint.parse(JSON.parse(fingerprint));
    return {
      kind,
      name,
      key,
      variables: {
        from_account,
        ...(typeof destination === 'number'
          ? { to_account: destination }
          : { to_address: destination }),
        source_pool,
        destination_pool,
        amount_zatoshi,
        ...(memo === null ? {} : { memo }),
      },
    };
  }
  if (fingerprint.startsWith('address:')) {
    const [, address, amount_zatoshi] = addressFaucetFingerprint.parse(fingerprint.split(':'));
    return { kind, name, key, variables: { address, amount_zatoshi } };
  }
  const [account_id, pool, amount_zatoshi] = faucetFingerprint.parse(fingerprint.split(':'));
  return { kind, name, key, variables: { account_id, pool, amount_zatoshi } };
}

function retainedPayments(
  kind: PaymentKind,
  // Forms resume the first match, so this tab's keys must precede shared work.
  areas = [sessionStorage, localStorage],
): PaymentIdentity[] {
  const identities = new Map<string, PaymentIdentity>();
  for (const storage of areas) {
    for (let index = 0; index < storage.length; index++) {
      const item = storage.key(index)!;
      if (!item.startsWith(`ths:${kind}:`)) continue;
      const value = storage.getItem(item);
      if (!value) continue;
      const isRecord = item.startsWith(`ths:${kind}:retained:`);
      const name = isRecord ? value : item.replace(/:tab$/, '');
      const key = isRecord ? item.slice(`ths:${kind}:retained:`.length) : value;
      identities.set(key, decodeIdentity(kind, name, key));
    }
  }
  return [...identities.values()];
}

/** Local state contains caller identity only; server snapshots stay in Query. */
export function useRetainedPayments(kind: PaymentKind) {
  const [snapshot, setSnapshot] = useState<{
    displayed: PaymentIdentity[];
    retained: PaymentIdentity[];
    error: Error | null;
  }>({ displayed: [], retained: [], error: null });
  useEffect(() => {
    let mounted = true;
    const read = () => {
      try {
        const retained = retainedPayments(kind);
        if (!mounted) return;
        setSnapshot((previous) => ({
          retained,
          displayed: [
            ...new Map(
              [...previous.displayed, ...retained].map((item) => [item.key, item]),
            ).values(),
          ],
          error: null,
        }));
      } catch (error) {
        if (mounted) setSnapshot((previous) => ({ ...previous, error: error as Error }));
      }
    };
    // Older builds retained identity only for the life of the tab. Migrate
    // without replacing another tab's newer shared payment, and never POST.
    void (async () => {
      try {
        const legacy = retainedPayments(kind, [sessionStorage]);
        for (const { name, key } of legacy) {
          await withPaymentLock(name, () => {
            if (
              sessionStorage.getItem(name) !== key &&
              sessionStorage.getItem(`${name}:tab`) !== key
            )
              return;
            localStorage.setItem(recordName(kind, key), name);
            if (!localStorage.getItem(name)) localStorage.setItem(name, key);
            if (sessionStorage.getItem(name) === key) {
              sessionStorage.setItem(`${name}:tab`, key);
              sessionStorage.removeItem(name);
            }
          });
        }
        read();
      } catch (error) {
        if (mounted) setSnapshot((previous) => ({ ...previous, error: error as Error }));
      }
    })();
    const update = () => read();
    window.addEventListener('storage', update);
    window.addEventListener(changed, update);
    return () => {
      mounted = false;
      window.removeEventListener('storage', update);
      window.removeEventListener(changed, update);
    };
  }, [kind]);
  // Displayed identities outlive storage cleanup so results stay readable.
  const show = useCallback((identity: PaymentIdentity) => {
    setSnapshot((previous) => ({
      ...previous,
      displayed: [
        ...new Map([...previous.displayed, identity].map((item) => [item.key, item])).values(),
      ],
    }));
  }, []);
  const dismissKey = useCallback((key: string) => {
    setSnapshot((previous) => ({
      ...previous,
      displayed: previous.displayed.filter(
        (item) => item.key !== key || previous.retained.some((retained) => retained.key === key),
      ),
    }));
  }, []);
  const forgetKey = useCallback((key: string) => {
    setSnapshot((previous) => ({
      ...previous,
      retained: previous.retained.filter((item) => item.key !== key),
      displayed: previous.displayed.filter((item) => item.key !== key),
    }));
  }, []);
  return { ...snapshot, show, dismissKey, forgetKey };
}

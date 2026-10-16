import type { Account } from '@/lib/api';

export function isUserAccount(account: Account): boolean {
  return Number.isInteger(account.id) && account.id >= 1 && account.id <= 5;
}

export function userAccounts(accounts: Account[]): Account[] {
  return accounts.filter(isUserAccount);
}

export function accountForAddress(accounts: Account[] | undefined, address: string) {
  return accounts?.find(
    (account) =>
      isUserAccount(account) &&
      (account.unified_address === address || account.transparent_address === address),
  );
}

export function accountPath(id: number): string {
  return `/wallet/accounts/${id}`;
}

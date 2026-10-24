import { describe, expect, it } from 'vitest';
import { testAccounts } from '@/test/utils';
import { accountForAddress, userAccounts } from './accounts';

describe('development account identity', () => {
  it('matches full unified and transparent addresses exactly', () => {
    const account = testAccounts[1]!;
    expect(accountForAddress(testAccounts, account.unified_address)).toBe(account);
    expect(accountForAddress(testAccounts, account.transparent_address)).toBe(account);
    expect(accountForAddress(testAccounts, account.unified_address.toUpperCase())).toBeUndefined();
    expect(accountForAddress(testAccounts, account.unified_address.slice(0, -1))).toBeUndefined();
    expect(accountForAddress(undefined, account.unified_address)).toBeUndefined();
  });

  it('filters and refuses to match non-user IDs even in raw cached accounts', () => {
    const extras = [0, 6, 1.5, 99].map((id) => ({
      ...testAccounts[0]!,
      id,
      unified_address: `uregtest1extra${id}`,
    }));
    expect(userAccounts([...testAccounts, ...extras])).toEqual(testAccounts);
    for (const account of extras)
      expect(
        accountForAddress([...testAccounts, ...extras], account.unified_address),
      ).toBeUndefined();
  });
});

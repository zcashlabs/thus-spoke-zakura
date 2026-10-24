import { Link } from 'react-router-dom';
import type { Account } from '@/lib/api';
import { accountForAddress, accountPath } from '@/lib/accounts';

export function AccountLabel({
  accounts,
  address,
}: {
  accounts: Account[] | undefined;
  address: string;
}) {
  const account = accountForAddress(accounts, address);
  return account ? (
    <Link
      to={accountPath(account.id)}
      className="text-accent-strong block text-[12px] hover:underline"
    >
      Account {account.id}
    </Link>
  ) : null;
}

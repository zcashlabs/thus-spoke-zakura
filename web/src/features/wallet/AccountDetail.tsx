import { Link, useParams } from 'react-router-dom';
import { Copy } from 'lucide-react';
import { CopyButton } from '@/components/ui/CopyButton';
import { Button } from '@/components/ui/Button';
import { DataRow, Panel, PanelNote } from '@/components/ui/Panel';
import { ErrorState, LoadingState } from '@/components/ui/StateBlock';
import { useAccounts } from '@/hooks/queries';
import { errorMessage } from '@/lib/api';
import { BACK_LINK } from '@/features/explorer/back-link';

export function AccountDetail() {
  const { accountId = '' } = useParams<{ accountId: string }>();
  const accounts = useAccounts();
  const validId = /^[1-5]$/.test(accountId);
  const account = validId ? accounts.data?.find(({ id }) => String(id) === accountId) : undefined;
  const retry = (
    <Button size="sm" disabled={accounts.isFetching} onClick={() => void accounts.refetch()}>
      Retry
    </Button>
  );

  return (
    <div className="grid gap-4">
      <header className="flex flex-wrap items-center justify-between gap-3">
        <h1 className="text-2xl font-bold tracking-[-0.02em]">
          {validId ? `Account ${accountId}` : 'Account'}
        </h1>
        <Link to="/wallet" className={BACK_LINK}>
          Back to Wallet
        </Link>
      </header>
      {!validId ? (
        <ErrorState message="Account not found. Choose a development account from 1 to 5." />
      ) : accounts.isPending ? (
        <LoadingState label="Loading account…" />
      ) : accounts.isError && !account ? (
        <ErrorState message={errorMessage(accounts.error)} action={retry} />
      ) : !account ? (
        <ErrorState message="Account not found. Choose a development account from 1 to 5." />
      ) : (
        <>
          {accounts.isError && (
            <ErrorState
              message={`Unable to refresh account details. Showing the last loaded values. ${errorMessage(accounts.error)}`}
              action={retry}
            />
          )}
          <Panel title="Account details">
            <dl>
              {[
                {
                  label: 'Unified address',
                  copyLabel: 'unified address',
                  value: account.unified_address,
                },
                {
                  label: 'Transparent address',
                  copyLabel: 'transparent address',
                  value: account.transparent_address,
                },
                ...(account.unified_full_viewing_key
                  ? [
                      {
                        label: 'Unified full viewing key',
                        copyLabel: 'unified full viewing key',
                        value: account.unified_full_viewing_key,
                      },
                    ]
                  : []),
              ].map(({ label, copyLabel, value }) => (
                <DataRow key={label} label={label}>
                  <div className="flex items-start gap-3">
                    <code className="min-w-0 flex-1 font-mono">{value}</code>
                    <CopyButton
                      value={value}
                      label={`Copy Account ${account.id} ${copyLabel}`}
                      icon={<Copy className="size-4" />}
                    />
                  </div>
                </DataRow>
              ))}
            </dl>
            <PanelNote>
              These identities belong to this local development account. Its unified address has no
              public chain-history page.
            </PanelNote>
          </Panel>
          <Link
            to={`/explorer/address/${account.transparent_address}`}
            className="text-accent-strong text-[13px] hover:underline"
          >
            View transparent address in Explorer
          </Link>
        </>
      )}
    </div>
  );
}

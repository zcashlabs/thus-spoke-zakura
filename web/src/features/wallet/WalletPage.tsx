import { SakuraMark } from '@/components/ui/SakuraMark';
import { QrCode, Send } from 'lucide-react';
import { useMemo, useState } from 'react';
import { useAccounts, useActivityPages } from '@/hooks/queries';
import { errorMessage } from '@/lib/api';
import { AccountCard } from './AccountCard';
import { ZecAmount } from '@/components/ui/ZecAmount';
import { PetalFall } from '@/components/ui/PetalFall';
import { ActivityList } from './ActivityList';
import { AccountCardSkeleton } from './AccountCardSkeleton';
import { Skeleton } from '@/components/ui/Skeleton';
import { useWalletActions } from './wallet-actions-context';
import { Button } from '@/components/ui/Button';
import { EmptyState, ErrorState } from '@/components/ui/StateBlock';
import { Select, type SelectOption } from '@/components/ui/Select';

const ACCOUNT_FILTERS: ReadonlyArray<SelectOption<string>> = [
  { value: 'all', label: 'All accounts' },
  ...[1, 2, 3, 4, 5].map((account) => ({
    value: String(account),
    label: `Account ${account}`,
  })),
];
const KIND_FILTERS: ReadonlyArray<SelectOption<string>> = [
  { value: 'all', label: 'All kinds' },
  { value: 'send', label: 'Send' },
  { value: 'faucet', label: 'Faucet' },
  { value: 'transaction', label: 'Transaction' },
];
const STATUS_FILTERS: ReadonlyArray<SelectOption<string>> = [
  { value: 'all', label: 'All statuses' },
  ...['preparing', 'prepared', 'broadcast', 'pending', 'confirmed', 'unconfirmed', 'failed'].map(
    (status) => ({ value: status, label: status[0]!.toUpperCase() + status.slice(1) }),
  ),
];

function ActivityFilter({
  id,
  label,
  value,
  options,
  onValueChange,
}: {
  id: string;
  label: string;
  value: string;
  options: ReadonlyArray<SelectOption<string>>;
  onValueChange: (value: string) => void;
}) {
  return (
    <label className="grid gap-1.5 text-[11px] font-bold tracking-[0.08em] uppercase" htmlFor={id}>
      {label}
      <Select id={id} value={value} options={options} onValueChange={onValueChange} />
    </label>
  );
}

export function WalletPage() {
  const accounts = useAccounts();
  const [account, setAccount] = useState('all');
  const [kind, setKind] = useState('all');
  const [status, setStatus] = useState('all');
  const [pageSize, setPageSize] = useState(10);
  const [activityPage, setActivityPage] = useState(0);
  const [isFetchingLastPage, setIsFetchingLastPage] = useState(false);
  const activityOptions = useMemo(
    () => ({
      limit: pageSize,
      ...(account === 'all' ? {} : { account: Number(account) }),
      ...(kind === 'all' ? {} : { kind: kind as 'send' | 'faucet' | 'transaction' }),
      ...(status === 'all' ? {} : { status }),
    }),
    [account, kind, pageSize, status],
  );
  const activity = useActivityPages(activityOptions);
  const activityPages = activity.data?.pages ?? [];
  const currentActivityPage = Math.min(activityPage, Math.max(0, activityPages.length - 1));
  const visibleActivity = activityPages[currentActivityPage]?.entries;
  const hasNextActivityPage =
    currentActivityPage + 1 < activityPages.length || activity.hasNextPage === true;

  const showNextActivityPage = async () => {
    if (currentActivityPage + 1 < activityPages.length) {
      setActivityPage(currentActivityPage + 1);
      return;
    }
    const result = await activity.fetchNextPage();
    if (result.data && currentActivityPage + 1 < result.data.pages.length) {
      setActivityPage(currentActivityPage + 1);
    }
  };

  const showLastActivityPage = async () => {
    setIsFetchingLastPage(true);
    try {
      let pages = activity.data?.pages ?? [];
      let hasMore = activity.hasNextPage === true;
      while (hasMore) {
        const previousLength = pages.length;
        const result = await activity.fetchNextPage();
        if (!result.data || result.isError) return;
        pages = result.data.pages;
        hasMore = result.hasNextPage === true;
        if (pages.length <= previousLength) return;
      }
      setActivityPage(Math.max(0, pages.length - 1));
    } finally {
      setIsFetchingLastPage(false);
    }
  };
  const { openRequest, openSend } = useWalletActions();

  const total = (accounts.data ?? []).reduce(
    (sum, account) => sum + account.ironwood_zatoshi + account.transparent_zatoshi,
    0n,
  );

  return (
    <div className="grid min-w-0 gap-4">
      <section className="xp-window border-accent-line from-accent-soft animate-rise to-panel relative flex min-h-28 items-center overflow-hidden rounded-xs bg-linear-115 px-4 py-5 sm:px-6">
        <PetalFall count={10} />
        <div className="relative z-10 min-w-0 pr-16">
          <span className="text-ink-muted block text-[11px] font-medium tracking-[0.08em] uppercase">
            Total balance
          </span>
          {accounts.isSuccess ? (
            <strong className="text-accent-strong my-1 block text-xl leading-none font-semibold tracking-[-0.03em] break-all sm:text-[32px]">
              <ZecAmount zatoshi={total} muteZero={false} />
            </strong>
          ) : (
            <Skeleton className="my-1.5 h-8 w-44 max-w-full" />
          )}
          <small className="text-ink-muted block text-[12px]">
            Across 5 deterministic development accounts
          </small>
        </div>
        <SakuraMark className="text-petal animate-bloom pointer-events-none absolute -right-4 -bottom-6 size-24 opacity-70 sm:size-32" />
      </section>

      <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="min-w-0">
          <h2 className="text-[14px] font-semibold">Accounts</h2>
          <p className="text-ink-muted mt-0.5 font-mono text-[11px]">
            ZIP-32 derived · Regtest only
          </p>
        </div>
        <div className="flex flex-wrap items-center gap-2">
          <Button variant="ghost" size="sm" onClick={openRequest}>
            <QrCode />
            Request
          </Button>
          <Button variant="ghost" size="sm" onClick={() => openSend()}>
            <Send />
            Send ZEC
          </Button>
        </div>
      </div>

      {accounts.isPending && (
        <section className="grid min-w-0 gap-2.5 sm:grid-cols-2 xl:grid-cols-3">
          {Array.from({ length: 5 }, (_, index) => (
            <AccountCardSkeleton key={index} index={index} />
          ))}
        </section>
      )}
      {accounts.isError && <ErrorState message={errorMessage(accounts.error)} />}
      {accounts.isSuccess &&
        (accounts.data.length === 0 ? (
          <EmptyState message="No development accounts were derived for this environment." />
        ) : (
          <section className="grid min-w-0 gap-2.5 sm:grid-cols-2 xl:grid-cols-3">
            {accounts.data.map((account, index) => (
              <AccountCard key={account.id} account={account} index={index} />
            ))}
          </section>
        ))}

      <ActivityList
        activity={{
          data: visibleActivity,
          isPending: activity.isPending,
          isError: activity.isError,
          error: activity.error,
        }}
        filters={
          <>
            <ActivityFilter
              id="activity-account"
              label="Account"
              value={account}
              options={ACCOUNT_FILTERS}
              onValueChange={(value) => {
                setActivityPage(0);
                setAccount(value);
              }}
            />
            <ActivityFilter
              id="activity-kind"
              label="Kind"
              value={kind}
              options={KIND_FILTERS}
              onValueChange={(value) => {
                setActivityPage(0);
                setKind(value);
              }}
            />
            <ActivityFilter
              id="activity-status"
              label="Status"
              value={status}
              options={STATUS_FILTERS}
              onValueChange={(value) => {
                setActivityPage(0);
                setStatus(value);
              }}
            />
          </>
        }
        page={currentActivityPage + 1}
        hasPreviousPage={currentActivityPage > 0}
        hasNextPage={hasNextActivityPage}
        isFetchingNextPage={activity.isFetchingNextPage}
        isFetchingLastPage={isFetchingLastPage}
        onFirstPage={() => setActivityPage(0)}
        onPreviousPage={() => setActivityPage(Math.max(0, currentActivityPage - 1))}
        onNextPage={() => void showNextActivityPage()}
        onLastPage={() => void showLastActivityPage()}
        pageSize={pageSize}
        onPageSizeChange={(value) => {
          setActivityPage(0);
          setPageSize(value);
        }}
      />
    </div>
  );
}

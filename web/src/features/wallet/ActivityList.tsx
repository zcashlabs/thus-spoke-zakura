import { useEffect, useId, useState } from 'react';
import { Link, useNavigate } from 'react-router-dom';
import { AlertTriangle, ArrowDownToLine, ArrowRight } from 'lucide-react';
import { cn } from '@/lib/cn';
import { absoluteTime, shortHash, timeAgo } from '@/lib/format';
import { formatZecAmount } from '@/lib/money';
import { errorMessage, type Activity } from '@/lib/api';
import { EmptyState, ErrorState } from '@/components/ui/StateBlock';
import { SkeletonRows } from '@/components/ui/Skeleton';
import { Badge } from '@/components/ui/Badge';
import { statusTone } from '@/components/ui/status-tone';
import { DataTable, NumCell, Row, type Column } from '@/components/ui/DataTable';
import { Panel } from '@/components/ui/Panel';

const COLUMNS: Column[] = [
  { key: 'transfer', header: 'Transfer', width: '196px' },
  { key: 'route', header: 'Route', collapse: true, width: '172px' },
  { key: 'txid', header: 'Transaction' },
  { key: 'amount', header: 'Amount', align: 'right', width: '92px' },
  { key: 'when', header: 'When', align: 'right', collapse: true, width: '104px' },
  { key: 'status', header: 'Status', align: 'right', width: '100px' },
];

/** Shielded funds read in accent; public funds read in neutral ink. */
function PoolLabel({
  pool,
  pools,
}: {
  pool: Activity['source_pool'];
  pools: Activity['source_pools'];
}) {
  const knownPools = pool ? [pool] : pools;
  const label = knownPools?.length ? knownPools.join(', ') : 'unknown';
  return (
    <span
      className={cn(
        'text-[11px] font-bold tracking-[0.04em] uppercase',
        knownPools?.includes('ironwood') ? 'text-accent' : 'text-ink-muted',
      )}
    >
      {label}
    </span>
  );
}

function accountLabel(accounts: number[] | undefined, account: number | null): string | null {
  const ids = accounts?.length ? accounts : account === null ? [] : [account];
  if (ids.length === 0) return null;
  return `${ids.length === 1 ? 'Account' : 'Accounts'} ${ids.join(', ')}`;
}

function recipientLabel(activity: Activity): string {
  // A batch to several addresses, or to accounts and addresses together, has
  // no single recipient. `recipient_addresses` can also name accounts' own
  // addresses, so only external ones are counted beside the accounts.
  const accountCount = activity.to_accounts?.length || (activity.to_account === null ? 0 : 1);
  const addresses = activity.external_addresses?.length ?? 0;
  if (addresses > 0 && accountCount + addresses > 1) {
    return `${accountCount + addresses} recipients`;
  }
  const accounts = accountLabel(activity.to_accounts, activity.to_account);
  if (accounts) return accounts;
  return shortHash(activity.to_address ?? '', 14, 6);
}

function ActivityRow({ activity, index, now }: { activity: Activity; index: number; now: Date }) {
  const id = useId();
  const errorId = activity.last_error ? id : undefined;
  const isFaucet = activity.kind === 'faucet';
  const isIncoming = activity.direction === 'incoming';
  const transactionPath = activity.txid ? `/explorer/tx/${activity.txid}` : null;
  const source = accountLabel(activity.from_accounts, activity.from_account) ?? 'External';
  const transfer = isFaucet
    ? `Faucet → ${recipientLabel(activity)}`
    : isIncoming
      ? `${source} → ${recipientLabel(activity)}`
      : `${source === 'External' ? 'Unknown' : source} → ${recipientLabel(activity)}`;
  const navigate = useNavigate();

  return (
    <>
      <Row
        index={index}
        {...(transactionPath ? { to: transactionPath } : {})}
        // The error row below belongs to this activity, so the pair reads,
        // hovers and navigates as one item.
        className={cn(
          activity.last_error &&
            '[&:has(+tr:hover)]:bg-accent-soft [&:hover+tr]:bg-accent-soft border-b-0',
        )}
      >
        <td aria-describedby={errorId}>
          <div className="flex items-center gap-2.5">
            <span
              className={cn(
                'xp-raised grid size-6 shrink-0 place-items-center rounded-xs',
                isFaucet || isIncoming ? 'bg-accent-soft text-accent' : 'bg-raised text-ink-muted',
              )}
              aria-hidden
            >
              {isFaucet || isIncoming ? (
                <ArrowDownToLine className="size-3" />
              ) : (
                <ArrowRight className="size-3" />
              )}
            </span>
            {transactionPath ? (
              <Link
                to={transactionPath}
                aria-describedby={errorId}
                className="min-w-0 truncate font-semibold hover:underline"
              >
                {transfer}
              </Link>
            ) : (
              <span className="min-w-0 truncate font-semibold">{transfer}</span>
            )}
          </div>
        </td>

        <td className="max-sm:hidden">
          <PoolLabel pool={activity.source_pool} pools={activity.source_pools} />
          <span className="text-ink-muted px-1.5">→</span>
          <PoolLabel pool={activity.destination_pool} pools={activity.destination_pools} />
        </td>

        <td className="min-w-0">
          {activity.txid ? (
            <code className="text-ink block truncate font-mono" title={activity.txid}>
              {shortHash(activity.txid)}
            </code>
          ) : (
            <span className="text-ink-muted relative">
              <span aria-hidden>—</span>
              <span className="sr-only">Not prepared</span>
            </span>
          )}
        </td>

        <NumCell className="font-semibold">{formatZecAmount(activity.amount_zatoshi)}</NumCell>

        <NumCell className="text-ink-muted max-sm:hidden">
          {activity.created_at ? (
            <time dateTime={activity.created_at} title={absoluteTime(activity.created_at)}>
              {timeAgo(activity.created_at, now)}
            </time>
          ) : activity.mined_height !== null && activity.mined_height !== undefined ? (
            `Block ${activity.mined_height}`
          ) : (
            <span aria-label="Time unknown">—</span>
          )}
        </NumCell>

        <td className="text-right">
          <Badge tone={statusTone(activity.status)}>{activity.status}</Badge>
        </td>
      </Row>
      {activity.last_error && (
        <tr
          style={{ animationDelay: `${Math.min(index, 8) * 30}ms` }}
          onClick={transactionPath ? () => void navigate(transactionPath) : undefined}
          className={cn(
            'border-line hover:bg-accent-soft animate-fade-in border-b transition-colors last:border-b-0',
            transactionPath && 'cursor-pointer',
          )}
        >
          <td colSpan={COLUMNS.length} className="px-5 pt-0 pb-3.5">
            <p
              id={errorId}
              className={cn(
                'flex items-start gap-1.5 pl-[34px] text-[11px] wrap-anywhere',
                activity.status === 'failed' ? 'text-negative' : 'text-warning',
              )}
            >
              <AlertTriangle className="mt-0.5 size-3 shrink-0" aria-hidden />
              <span>
                {activity.status === 'failed' ? 'Failed: ' : 'Latest execution error: '}
                {activity.last_error}
              </span>
            </p>
          </td>
        </tr>
      )}
    </>
  );
}

export function ActivityList({
  activity,
}: {
  activity: {
    data?: Activity[] | undefined;
    isPending: boolean;
    isError: boolean;
    error: unknown;
  };
}) {
  const count = activity.data?.length ?? 0;
  const [now, setNow] = useState(() => new Date());
  const hasActivity = count > 0;
  useEffect(() => {
    if (!hasActivity) return;
    // Relative labels age independently of server events and query updates.
    const timer = window.setInterval(() => setNow(new Date()), 1000);
    return () => window.clearInterval(timer);
  }, [hasActivity]);

  return (
    <Panel
      title="Recent activity"
      className="min-w-0"
      meta={
        <Badge>
          {count} event{count === 1 ? '' : 's'}
        </Badge>
      }
    >
      {activity.isPending && <SkeletonRows rows={4} />}
      {activity.isError && <ErrorState message={errorMessage(activity.error)} />}
      {activity.data &&
        (count === 0 ? (
          <EmptyState message="No transactions yet. Use the faucet to create your first activity." />
        ) : (
          <DataTable columns={COLUMNS}>
            {activity.data.map((item, index) => (
              <ActivityRow key={item.id} activity={item} index={index} now={now} />
            ))}
          </DataTable>
        ))}
    </Panel>
  );
}

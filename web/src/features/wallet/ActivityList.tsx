import { useId } from 'react';
import { Link, useNavigate } from 'react-router-dom';
import {
  AlertTriangle,
  ArrowDownToLine,
  ArrowRight,
  ChevronLeft,
  ChevronRight,
} from 'lucide-react';
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
import { Button } from '@/components/ui/Button';
import { Select, type SelectOption } from '@/components/ui/Select';
import type { ReactNode } from 'react';

const PAGE_SIZES: ReadonlyArray<SelectOption<string>> = ['10', '25', '50', '100'].map((value) => ({
  value,
  label: value,
}));

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
  const accounts = accountLabel(activity.to_accounts, activity.to_account);
  if (accounts) return accounts;
  return shortHash(activity.to_address ?? '', 14, 6);
}

function ActivityRow({ activity, index }: { activity: Activity; index: number }) {
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
              {timeAgo(activity.created_at)}
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
  filters,
  page = 1,
  hasPreviousPage = false,
  hasNextPage = false,
  isFetchingNextPage = false,
  isFetchingLastPage = false,
  onFirstPage,
  onPreviousPage,
  onNextPage,
  onLastPage,
  pageSize,
  onPageSizeChange,
}: {
  activity: {
    data?: Activity[] | undefined;
    isPending: boolean;
    isError: boolean;
    error: unknown;
  };
  filters?: ReactNode;
  page?: number;
  hasPreviousPage?: boolean;
  hasNextPage?: boolean;
  isFetchingNextPage?: boolean;
  isFetchingLastPage?: boolean;
  onFirstPage?: () => void;
  onPreviousPage?: () => void;
  onNextPage?: () => void;
  onLastPage?: () => void;
  pageSize?: number;
  onPageSizeChange?: (pageSize: number) => void;
}) {
  const count = activity.data?.length ?? 0;

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
      {filters && (
        <div className="border-line bg-raised grid gap-3 border-b px-4 py-3 sm:grid-cols-3">
          {filters}
        </div>
      )}
      {activity.isPending && <SkeletonRows rows={4} />}
      {activity.isError && <ErrorState message={errorMessage(activity.error)} />}
      {activity.data &&
        (count === 0 ? (
          <EmptyState message="No transactions yet. Use the faucet to create your first activity." />
        ) : (
          <DataTable columns={COLUMNS}>
            {activity.data.map((item, index) => (
              <ActivityRow key={item.id} activity={item} index={index} />
            ))}
          </DataTable>
        ))}
      {pageSize !== undefined && onPageSizeChange && (
        <div className="border-line bg-raised flex flex-wrap items-center justify-between gap-3 border-t px-4 py-3">
          <label
            className="text-ink-muted flex items-center gap-2 text-[12px]"
            htmlFor="activity-page-size"
          >
            Show
            <span className="w-20">
              <Select
                id="activity-page-size"
                value={String(pageSize)}
                options={PAGE_SIZES}
                onValueChange={(value) => onPageSizeChange(Number(value))}
              />
            </span>
            entries
          </label>
          <nav aria-label="Activity pages" className="flex items-center gap-1.5">
            <Button variant="subtle" size="sm" disabled={!hasPreviousPage} onClick={onFirstPage}>
              First
            </Button>
            <Button
              variant="subtle"
              size="sm"
              aria-label="Previous page"
              disabled={!hasPreviousPage}
              onClick={onPreviousPage}
            >
              <ChevronLeft />
            </Button>
            <span className="xp-face text-ink-muted rounded-xs px-3 py-2 text-[12px] font-medium">
              Page {page}
            </span>
            <Button
              variant="subtle"
              size="sm"
              aria-label="Next page"
              loading={isFetchingNextPage}
              disabled={!hasNextPage || isFetchingNextPage || isFetchingLastPage}
              onClick={onNextPage}
            >
              <ChevronRight />
            </Button>
            <Button
              variant="subtle"
              size="sm"
              loading={isFetchingLastPage}
              disabled={!hasNextPage || isFetchingNextPage || isFetchingLastPage}
              onClick={onLastPage}
            >
              Last
            </Button>
          </nav>
        </div>
      )}
    </Panel>
  );
}

import { useRef, useState } from 'react';
import { useQueries } from '@tanstack/react-query';
import { AlertTriangle, Clock3, Loader2 } from 'lucide-react';
import { Button } from '@/components/ui/Button';
import { Dialog } from '@/components/ui/Dialog';
import { api, errorMessage } from '@/lib/api';
import { cn } from '@/lib/cn';
import { queryKeys } from '@/hooks/queries';
import type { PaymentIdentity } from '@/hooks/payment-identity';
import { useWalletPayments } from './payment-context';
import { useWalletActions } from './wallet-actions-context';
import { PaymentStatus, PaymentWarning } from './PaymentStatus';

const FILTERS = ['all', 'unresolved', 'confirmed', 'failed'] as const;
type PaymentFilter = (typeof FILTERS)[number];
const LABELS: Record<PaymentFilter, string> = {
  all: 'All',
  unresolved: 'Unresolved',
  confirmed: 'Confirmed',
  failed: 'Failed',
};

export function PaymentUpdates() {
  const {
    send,
    faucet,
    savedSend,
    savedFaucet,
    isRestored,
    viewedPayment,
    viewPayment,
    closePayments,
  } = useWalletPayments();
  const { openSend, openFaucet } = useWalletActions();
  const identities = [...savedSend.displayed, ...savedFaucet.displayed];
  const open = viewedPayment !== null;
  // null selects All without announcing results when the panel first opens.
  const [selection, setSelection] = useState<{
    view: typeof viewedPayment;
    filter: PaymentFilter;
  } | null>(null);
  // Each open request starts at All, including a link to a previously hidden row.
  const filter = selection?.view === viewedPayment ? selection.filter : null;
  const selected = filter ?? 'all';
  const setFilter = (value: PaymentFilter) => setSelection({ view: viewedPayment, filter: value });
  const [visibleKeys, setVisibleKeys] = useState<string[]>([]);
  const [container, setContainer] = useState<HTMLDivElement | null>(null);
  const trigger = useRef<HTMLButtonElement>(null);
  const activeFilter = useRef<HTMLButtonElement>(null);
  // Observe the same Query snapshots for counts, without starting extra reads.
  const statuses = useQueries({
    queries: identities.map(({ key, variables }) => ({
      queryKey: queryKeys.payment(key),
      queryFn: () => ('address' in variables ? api.faucetAddressStatus(key) : api.payment(key)),
      enabled: false,
    })),
  });
  // Prioritize unresolved work when opening, then keep rows stable while the
  // panel is in use so a background confirmation cannot move focused controls.
  const ordered = identities
    .map((identity, index) => {
      const status = statuses[index]?.data?.status;
      const group: Exclude<PaymentFilter, 'all'> =
        status === 'confirmed' || status === 'failed' ? status : 'unresolved';
      return { identity, group };
    })
    .sort((a, b) => {
      const first = visibleKeys.indexOf(a.identity.key);
      const second = visibleKeys.indexOf(b.identity.key);
      return (first < 0 ? identities.length : first) - (second < 0 ? identities.length : second);
    });
  const counts = { all: identities.length, unresolved: 0, confirmed: 0, failed: 0 };
  for (const { group } of ordered) counts[group]++;
  const visible = ordered.filter(({ group }) => selected === 'all' || group === selected);
  const includedKeys = new Set(visible.map(({ identity }) => identity.key));
  const summary = Object.entries(counts)
    .filter(([state, count]) => state !== 'all' && count > 0)
    .map(([state, count]) => `${count} ${state}`)
    .join(' · ');
  const errors = [savedSend.error, savedFaucet.error].filter((error) => error !== null);
  const finished = visible.filter(
    ({ identity, group }) =>
      group !== 'unresolved' &&
      !(identity.kind === 'send' ? savedSend : savedFaucet).retained.some(
        ({ key }) => key === identity.key,
      ),
  );
  const attention = errors.length > 0 || counts.failed > 0;
  const active = send.isPending || faucet.isPending;
  const StatusIcon = attention ? AlertTriangle : active ? Loader2 : Clock3;
  const focusNext = (identity: PaymentIdentity) => {
    const headings = [
      ...(container?.querySelectorAll<HTMLHeadingElement>('article:not([hidden]) h3') ?? []),
    ];
    const index = ordered.findIndex((item) => item.identity.key === identity.key);
    const position = (heading: HTMLHeadingElement) =>
      ordered.findIndex(
        (item) => item.identity.key === heading.closest('article')?.dataset.paymentKey,
      );
    const next =
      headings.find((heading) => position(heading) > index) ??
      [...headings].reverse().find((heading) => position(heading) < index);
    (next ?? activeFilter.current ?? container)?.focus();
  };
  const dismiss = (identity: PaymentIdentity) => {
    (identity.kind === 'send' ? savedSend : savedFaucet).dismissKey(identity.key);
    requestAnimationFrame(() => focusNext(identity));
  };
  const forget = async (identity: PaymentIdentity) => {
    const operation = identity.kind === 'send' ? send : faucet;
    const saved = identity.kind === 'send' ? savedSend : savedFaucet;
    if (!(await operation.forget(identity, () => saved.forgetKey(identity.key)))) return;
    requestAnimationFrame(() => focusNext(identity));
  };
  const startNew = (identity: PaymentIdentity) => {
    closePayments();
    // Use a connected opener so closing the next form returns focus to the
    // summary rather than the row removed when this panel closes.
    requestAnimationFrame(() => {
      trigger.current?.focus();
      if (identity.kind === 'send') openSend(identity.variables.from_account, identity);
      else
        openFaucet(
          'account_id' in identity.variables ? identity.variables.account_id : undefined,
          identity,
        );
    });
  };
  if (!identities.length && !errors.length && !open) return null;
  return (
    <>
      <Button
        ref={trigger}
        variant="ghost"
        aria-label={`View payments: ${summary || 'Storage unavailable'}`}
        aria-haspopup="dialog"
        onClick={() => {
          const priority = (status: string | undefined) =>
            status === 'confirmed' ? 2 : status === 'failed' ? 1 : 0;
          setVisibleKeys(
            identities
              .map((identity, index) => ({ identity, status: statuses[index]?.data?.status }))
              .sort((a, b) => priority(a.status) - priority(b.status))
              .map(({ identity }) => identity.key),
          );
          viewPayment();
        }}
      >
        <StatusIcon
          aria-hidden
          className={attention ? 'text-warning' : active ? 'motion-safe:animate-spin' : undefined}
        />
        Payments
        {identities.length > 0 && (
          <span className="bg-accent-soft text-accent-strong rounded-xs px-1.5 text-[11px] tabular-nums">
            {identities.length}
          </span>
        )}
      </Button>
      <Dialog
        open={open}
        onOpenChange={(next) => {
          if (!next) closePayments();
          if (!next && !identities.length && !errors.length) {
            requestAnimationFrame(() => {
              const heading = document.querySelector<HTMLElement>('main h1');
              if (heading) {
                heading.tabIndex = -1;
                heading.focus();
              }
            });
          }
        }}
        title="Payments"
        description={
          identities.length ? 'Status checks are automatic. Resume repeats the saved request.' : ''
        }
        className="flex max-h-[calc(100dvh-36px)] w-[min(540px,calc(100vw-36px))] flex-col overflow-y-hidden"
        bodyClassName="relative flex min-h-0 flex-col [&>p]:shrink-0"
        fallbackFocus={trigger}
      >
        {identities.length > 0 && (
          <div
            role="group"
            aria-label="Filter payments"
            className="mb-3 grid shrink-0 grid-cols-2 gap-1.5 sm:grid-cols-4"
          >
            {FILTERS.map((value) => (
              <button
                key={value}
                ref={value === selected ? activeFilter : undefined}
                type="button"
                aria-pressed={value === selected}
                onClick={() => {
                  setFilter(value);
                  if (container) container.scrollTop = 0;
                }}
                className={cn(
                  'rounded-control flex min-h-9 cursor-pointer items-center justify-between gap-1.5 border px-2.5 py-1.5 text-[12px] whitespace-nowrap',
                  value === selected
                    ? 'border-accent-strong bg-accent-soft text-accent-strong font-semibold'
                    : 'border-line bg-panel text-ink-muted hover:border-line-strong hover:bg-raised hover:text-ink',
                )}
              >
                {LABELS[value]}{' '}
                <span
                  className={cn(
                    'min-w-5 rounded-xs px-1 text-center text-[11px] leading-5 tabular-nums',
                    value === selected
                      ? 'bg-accent-strong text-accent-ink'
                      : 'bg-raised text-ink-muted',
                  )}
                >
                  {counts[value]}
                </span>
              </button>
            ))}
          </div>
        )}
        <p role="status" className="sr-only">
          {filter !== null &&
            (visible.length
              ? `Showing ${visible.length} ${selected === 'all' ? '' : `${selected} `}${visible.length === 1 ? 'payment' : 'payments'}.`
              : selected === 'all'
                ? 'No payment updates.'
                : `No ${selected} payments.`)}
        </p>
        <div
          ref={setContainer}
          role="region"
          aria-label="Payment updates"
          tabIndex={0}
          className={cn(
            'payment-scroll-area border-line relative max-h-[min(55dvh,440px)] min-h-0 overflow-y-auto overscroll-contain rounded-xs focus-visible:-outline-offset-2',
            (visible.length > 0 || errors.length > 0) && 'border',
          )}
        >
          {errors.length > 0 && (
            <PaymentWarning role="alert" className="m-3">
              <p>Couldn’t load saved payments: {errors.map(errorMessage).join('; ')}</p>
              <p>Allow site storage and reload.</p>
            </PaymentWarning>
          )}
          {!visible.length && (identities.length > 0 || errors.length === 0) && (
            <div className="text-ink-muted p-4 text-[12px]">
              <p>{selected === 'all' ? 'No payment updates' : `No ${selected} payments.`}</p>
              {selected !== 'all' && (
                <Button
                  size="sm"
                  className="mt-3"
                  onClick={() => {
                    setFilter('all');
                    requestAnimationFrame(() => activeFilter.current?.focus());
                  }}
                >
                  Show all
                </Button>
              )}
            </div>
          )}
        </div>
        {finished.length > 0 && (
          <div className="mt-3 flex shrink-0 justify-end">
            <Button
              size="sm"
              variant="ghost"
              onClick={() => {
                for (const { identity } of finished) {
                  (identity.kind === 'send' ? savedSend : savedFaucet).dismissKey(identity.key);
                }
                requestAnimationFrame(() =>
                  (finished.length === visible.length
                    ? (activeFilter.current ?? container)
                    : container
                  )?.focus(),
                );
              }}
            >
              {selected === 'all' ? 'Clear finished' : `Clear ${selected}`}
            </Button>
          </div>
        )}
      </Dialog>
      {ordered.map(({ identity }) => {
        const operation = identity.kind === 'send' ? send : faucet;
        return (
          <PaymentStatus
            key={identity.key}
            container={container}
            hidden={!includedKeys.has(identity.key)}
            onHiddenFocus={() => focusNext(identity)}
            focusRequest={viewedPayment?.key === identity.key ? viewedPayment : null}
            onView={() => viewPayment(identity.key)}
            onFollowTransaction={closePayments}
            identity={identity}
            restored={isRestored(identity.key)}
            inFlight={operation.isSubmitting(identity)}
            busy={send.isPending || faucet.isPending}
            claimAnnouncement={operation.claimAnnouncement}
            onTerminal={operation.clear}
            onResume={(item) => operation.resume(item)}
            onForget={() => forget(identity)}
            onDismiss={() => dismiss(identity)}
            onNew={() => startNew(identity)}
          />
        );
      })}
    </>
  );
}

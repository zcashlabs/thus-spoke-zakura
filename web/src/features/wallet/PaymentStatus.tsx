import { useEffect, useId, useLayoutEffect, useRef, useState, type ReactNode } from 'react';
import { Link } from 'react-router-dom';
import { createPortal } from 'react-dom';
import {
  AlertTriangle,
  CheckCircle2,
  ChevronRight,
  Clock3,
  HelpCircle,
  Loader2,
  RotateCcw,
  XCircle,
} from 'lucide-react';
import { Badge } from '@/components/ui/Badge';
import { Button } from '@/components/ui/Button';
import { useToast } from '@/components/ui/toast-context';
import { usePaymentStatus, isTerminalPayment } from '@/hooks/queries';
import { paymentRecipient, type PaymentIdentity } from '@/hooks/payment-identity';
import { errorMessage } from '@/lib/api';
import { formatZecAmount } from '@/lib/money';
import { cn } from '@/lib/cn';
import { absoluteTime } from '@/lib/format';

export function PaymentStatus({
  identity,
  restored,
  inFlight,
  busy,
  claimAnnouncement,
  onTerminal,
  onResume,
  onForget,
  onDismiss,
  onNew,
  container,
  hidden,
  onHiddenFocus,
  focusRequest,
  onView,
  onFollowTransaction,
}: {
  identity: PaymentIdentity;
  restored: boolean;
  inFlight: boolean;
  busy: boolean;
  claimAnnouncement: (key: string) => boolean;
  onTerminal: (identity: PaymentIdentity) => Promise<void>;
  onResume: (identity: PaymentIdentity) => void;
  onForget: () => Promise<void>;
  onDismiss: () => void;
  onNew: () => void;
  container: HTMLElement | null;
  hidden: boolean;
  onHiddenFocus: () => void;
  focusRequest: object | null;
  onView: () => void;
  onFollowTransaction: () => void;
}) {
  const status = usePaymentStatus(identity.key, true, 'address' in identity.variables);
  const activity = status.data;
  const toast = useToast();
  const titleId = useId();
  const recoveryId = useId();
  const detailsId = useId();
  const heading = useRef<HTMLHeadingElement>(null);
  const article = useRef<HTMLElement>(null);
  const focused = useRef(false);
  const recoveryFocused = useRef(false);
  const focusedRequest = useRef<object | null>(null);
  const handled = useRef(false);
  const observedUnresolved = useRef(false);
  const mounted = useRef(true);
  const [cleanupError, setCleanupError] = useState<string | null>(null);
  const [checkingCleanup, setCheckingCleanup] = useState(false);
  const [detailsOpen, setDetailsOpen] = useState(false);
  const terminal = isTerminalPayment(activity);
  const confirmed = activity?.status === 'confirmed';
  const failed = activity?.status === 'failed';
  const canForget = status.isSuccess && activity === null && !inFlight;
  const variables = identity.variables;
  const destination = paymentRecipient(variables);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);
  useEffect(() => {
    // Suppress outcomes already terminal at restoration, not new completions
    // after this session has observed an unresolved payment.
    if (status.isSuccess && !terminal) observedUnresolved.current = true;
    if (!terminal || handled.current) return;
    handled.current = true;
    if ((!restored || observedUnresolved.current) && claimAnnouncement(identity.key)) {
      if (confirmed) {
        toast.success(
          identity.kind === 'send' ? `Sent to ${destination}` : `Funded ${destination}`,
          `${formatZecAmount(variables.amount_zatoshi)} confirmed.`,
        );
      } else {
        toast.error(
          'Payment failed',
          `${formatZecAmount(variables.amount_zatoshi)} to ${destination}. ${activity?.last_error ?? 'Check payment details.'}`,
          { label: 'View payment', onClick: onView },
        );
      }
    }
    void onTerminal(identity).catch((error: unknown) => {
      if (mounted.current) setCleanupError(errorMessage(error));
    });
  }, [
    terminal,
    status.isSuccess,
    restored,
    confirmed,
    activity,
    claimAnnouncement,
    destination,
    identity,
    onTerminal,
    onView,
    toast,
    variables.amount_zatoshi,
  ]);
  useLayoutEffect(() => {
    if (!container || hidden || !focusRequest || focusedRequest.current === focusRequest) return;
    // Let Radix finish mounting the focus scope, including over an open form.
    const frame = requestAnimationFrame(() => {
      focusedRequest.current = focusRequest;
      heading.current?.focus({ preventScroll: true });
      article.current?.scrollIntoView({ block: 'nearest' });
    });
    return () => cancelAnimationFrame(frame);
  }, [container, hidden, focusRequest]);
  useLayoutEffect(() => {
    if (
      hidden &&
      focused.current &&
      (document.activeElement === document.body ||
        article.current?.contains(document.activeElement))
    ) {
      focused.current = false;
      recoveryFocused.current = false;
      onHiddenFocus();
      return;
    }
    // A removed recovery button needs a destination; ordinary polls keep focus.
    if (
      !hidden &&
      (terminal || inFlight) &&
      recoveryFocused.current &&
      document.activeElement === document.body
    ) {
      heading.current?.focus();
      recoveryFocused.current = false;
    }
  }, [hidden, terminal, inFlight, onHiddenFocus]);
  const checkCleanup = async () => {
    setCheckingCleanup(true);
    try {
      const result = await status.refetch();
      if (result.error) throw result.error;
      if (isTerminalPayment(result.data)) await onTerminal(identity);
      if (mounted.current) setCleanupError(null);
    } catch (error) {
      if (mounted.current) setCleanupError(errorMessage(error));
    } finally {
      if (mounted.current) setCheckingCleanup(false);
    }
  };
  const forget = async () => {
    setCheckingCleanup(true);
    try {
      await onForget();
      if (mounted.current) setCleanupError(null);
    } catch (error) {
      if (mounted.current) setCleanupError(errorMessage(error));
    } finally {
      if (mounted.current) setCheckingCleanup(false);
    }
  };
  const checking = !activity && status.isPending && status.isFetching;
  const label = confirmed
    ? 'Confirmed'
    : failed
      ? 'Failed'
      : inFlight
        ? 'Submitting'
        : activity
          ? 'Pending'
          : checking
            ? 'Checking'
            : 'Unknown';
  const StatusIcon = confirmed
    ? CheckCircle2
    : failed
      ? XCircle
      : inFlight || checking
        ? Loader2
        : activity
          ? Clock3
          : HelpCircle;
  const phases: Record<string, string> = {
    preparing: 'Preparing payment',
    prepared: 'Ready to broadcast',
    broadcast: 'Waiting for confirmation',
  };
  const phase = activity && !terminal ? phases[activity.status] : undefined;

  // Polling and terminal cleanup outlive the on-demand panel. Only the view
  // is portalled into its scroll area, so closing it cannot interrupt recovery.
  if (!container) return null;
  return createPortal(
    <article
      ref={article}
      hidden={hidden}
      data-payment-key={identity.key}
      className="border-line focus-within:bg-accent-soft border-b px-4 py-3 text-[12px] wrap-anywhere last:border-b-0"
      onFocusCapture={(event) => {
        focused.current = true;
        recoveryFocused.current =
          event.target instanceof HTMLElement && event.target.dataset.recovery === 'true';
      }}
      onBlurCapture={(event) => {
        if (event.relatedTarget) {
          recoveryFocused.current = false;
          focused.current = event.currentTarget.contains(event.relatedTarget);
        }
      }}
    >
      <h3 id={titleId} ref={heading} tabIndex={-1} className="font-semibold">
        {identity.kind === 'send' ? `Account ${identity.variables.from_account} → ` : 'Faucet → '}
        {destination} · {formatZecAmount(variables.amount_zatoshi)}
      </h3>
      <div role="status" className="mt-2 flex flex-wrap items-center gap-x-2 gap-y-1">
        <Badge
          tone={confirmed ? 'positive' : failed ? 'negative' : 'neutral'}
          className="gap-1.5 text-[12px] font-medium tracking-normal normal-case"
        >
          <StatusIcon
            className={cn('size-3 shrink-0', StatusIcon === Loader2 && 'motion-safe:animate-spin')}
            aria-hidden
          />
          {label}
        </Badge>
        {phase && (
          <span className="text-ink-muted">
            {status.isError || inFlight ? `Last known: ${phase}` : phase}
          </span>
        )}
      </div>
      {failed && (
        <p className="text-negative mt-2">
          {activity.last_error ?? 'The server marked this payment as failed.'}
        </p>
      )}
      {!terminal && !activity && !status.isPending && !status.isError && (
        <p className="text-ink-muted mt-2">No record yet. The request is saved.</p>
      )}
      {canForget && (
        <p className="text-ink-muted mt-1">
          Forgetting removes it from this browser only; it does not cancel server work.
        </p>
      )}
      {!terminal && activity?.last_error && (
        <PaymentWarning className="mt-2">
          <p>Latest execution error: {activity.last_error}</p>
          <p>Recovery may still be running.</p>
        </PaymentWarning>
      )}
      {!terminal && status.isError && (
        <PaymentWarning className="mt-2">
          <p>Couldn’t check status: {errorMessage(status.error)}</p>
          <p>
            {activity
              ? 'Last known status shown. The request is saved.'
              : 'We’ll keep checking. The request is saved.'}
          </p>
        </PaymentWarning>
      )}
      {cleanupError && (
        <PaymentWarning className="mt-2">
          <p>Couldn’t clear saved request: {cleanupError}</p>
          <p>Allow site storage, then check again.</p>
        </PaymentWarning>
      )}
      <div className="mt-2 flex items-start gap-2">
        <button
          type="button"
          aria-expanded={detailsOpen}
          aria-controls={detailsId}
          aria-describedby={titleId}
          className="text-ink-muted hover:text-ink inline-flex min-h-9 shrink-0 cursor-pointer items-center gap-1"
          onClick={() => setDetailsOpen((open) => !open)}
        >
          <ChevronRight
            aria-hidden
            className={cn('size-3', detailsOpen ? 'rotate-90' : 'rtl:rotate-180')}
          />
          Details
        </button>
        {!terminal && !inFlight && (
          <p id={recoveryId} className="sr-only">
            Resume continues this saved payment.
          </p>
        )}
        <div className="flex min-w-0 flex-1 flex-wrap justify-end gap-2">
          {!terminal && !inFlight && !status.isPending && (
            <Button
              type="button"
              size="sm"
              variant="subtle"
              disabled={busy}
              data-recovery="true"
              aria-describedby={`${titleId} ${recoveryId}`}
              onClick={() => onResume(identity)}
            >
              <RotateCcw aria-hidden />
              Resume
            </Button>
          )}
          {canForget && (
            <Button
              type="button"
              size="sm"
              variant="ghost"
              loading={checkingCleanup}
              aria-describedby={titleId}
              onClick={() => void forget()}
            >
              Forget saved request
            </Button>
          )}
          {confirmed && activity.txid && (
            <Link
              className="text-accent-strong self-center underline underline-offset-2"
              aria-describedby={titleId}
              to={`/explorer/tx/${activity.txid}`}
              onClick={(event) => {
                const { button, altKey, ctrlKey, metaKey, shiftKey, currentTarget } = event;
                const opensSeparateContext =
                  currentTarget.target !== '' && currentTarget.target.toLowerCase() !== '_self';
                if (
                  button === 0 &&
                  !altKey &&
                  !ctrlKey &&
                  !metaKey &&
                  !shiftKey &&
                  !opensSeparateContext
                ) {
                  onFollowTransaction();
                }
              }}
            >
              View transaction
            </Link>
          )}
          {failed && (
            <Button type="button" size="sm" aria-describedby={titleId} onClick={onNew}>
              {identity.kind === 'send' ? 'New Send' : 'Request funds'}
            </Button>
          )}
          {terminal &&
            (cleanupError ? (
              <Button
                type="button"
                size="sm"
                loading={checkingCleanup}
                aria-describedby={titleId}
                onClick={() => void checkCleanup()}
              >
                Check again
              </Button>
            ) : (
              <Button
                type="button"
                size="sm"
                variant="ghost"
                aria-describedby={titleId}
                onClick={onDismiss}
              >
                Dismiss
              </Button>
            ))}
        </div>
      </div>
      <div id={detailsId} hidden={!detailsOpen} className="text-ink-muted mt-2 space-y-1">
        {restored && <p>Previous payment</p>}
        {identity.kind === 'send' ? (
          <p>
            Pool route: {identity.variables.source_pool} → {identity.variables.destination_pool}
          </p>
        ) : 'pool' in identity.variables ? (
          <p>Pool route: {identity.variables.pool}</p>
        ) : null}
        {identity.kind === 'send' && identity.variables.to_address && (
          <p>Destination: {identity.variables.to_address}</p>
        )}
        {'address' in identity.variables && <p>Destination: {identity.variables.address}</p>}
        {identity.kind === 'send' && (
          <p>
            Memo:{' '}
            {identity.variables.memo === undefined
              ? 'None'
              : identity.variables.memo === ''
                ? '(empty)'
                : identity.variables.memo}
          </p>
        )}
        {activity?.created_at && <p>Created: {absoluteTime(activity.created_at)}</p>}
        {failed && <p>Adding funds won’t restart this payment. Start a new one to try again.</p>}
      </div>
    </article>,
    container,
  );
}

export function PaymentWarning({
  children,
  role = 'status',
  className,
}: {
  children: ReactNode;
  role?: 'alert' | 'status';
  className?: string;
}) {
  return (
    <div
      role={role}
      className={cn(
        'border-warning/35 bg-warning-soft text-warning flex items-start gap-2 rounded-xs border p-2.5 text-[12px] wrap-anywhere',
        className,
      )}
    >
      <AlertTriangle className="mt-0.5 size-4 shrink-0" aria-hidden />
      <div className="min-w-0 space-y-1">{children}</div>
    </div>
  );
}

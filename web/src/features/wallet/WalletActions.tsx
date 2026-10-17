import { useMemo, useRef, useState, type ReactNode } from 'react';
import { WalletActionsContext, type WalletActionsApi } from './wallet-actions-context';
import { useAccounts, useMiningJob } from '@/hooks/queries';
import { SendDialog } from './SendDialog';
import { RequestDialog } from './RequestDialog';
import { FaucetDialog } from './FaucetDialog';
import { MineDialog } from './MineDialog';
import { MiningStatus } from './MiningStatus';
import { Button } from '@/components/ui/Button';
import { useSend, useFaucet } from '@/hooks/mutations';
import {
  useRetainedPayments,
  paymentRecipient,
  type PaymentIdentity,
} from '@/hooks/payment-identity';
import { WalletPaymentsContext } from './payment-context';
import { useToast } from '@/components/ui/toast-context';
import { errorMessage, isPaymentRejection } from '@/lib/api';
import { formatZecAmount } from '@/lib/money';

type OpenDialog = 'send' | 'request' | 'faucet' | 'mine' | null;

/**
 * Owns the transaction dialogs so the header and the account cards can trigger
 * them without threading state through the tree.
 */
export function WalletActionsProvider({ children }: { children: ReactNode }) {
  const toast = useToast();
  const [viewedPayment, setViewedPayment] = useState<{ key: string | null } | null>(null);
  const viewPayment = (key?: string) => setViewedPayment({ key: key ?? null });
  const [dialog, setDialog] = useState<OpenDialog>(null);
  const [dismissedJobId, setDismissedJobId] = useState<string | null>(null);
  const [formVersion, setFormVersion] = useState(0);
  const [faucetAccount, setFaucetAccount] = useState<number | undefined>(undefined);
  const [sendAccount, setSendAccount] = useState<number | undefined>(undefined);
  const [sendDraft, setSendDraft] = useState<PaymentIdentity | undefined>();
  const [faucetDraft, setFaucetDraft] = useState<PaymentIdentity | undefined>();
  const savedSend = useRetainedPayments('send');
  const savedFaucet = useRetainedPayments('faucet');
  const liveKeys = useRef(new Set<string>());
  const lifecycle = {
    onRetained: (identity: PaymentIdentity) => {
      liveKeys.current.add(identity.key);
      (identity.kind === 'send' ? savedSend : savedFaucet).show(identity);
    },
    onError: (error: Error, variables: PaymentIdentity['variables'], key: string | undefined) => {
      toast.error(
        isPaymentRejection(error) ? 'Payment rejected' : 'Payment not confirmed',
        `${formatZecAmount(variables.amount_zatoshi)} to ${paymentRecipient(variables)}. ${errorMessage(error)}`,
        key ? { label: 'View payment', onClick: () => viewPayment(key) } : undefined,
      );
    },
  };
  const send = useSend(lifecycle);
  const faucet = useFaucet(lifecycle);
  const { data: accounts = [] } = useAccounts();
  const { data: mining } = useMiningJob();
  const job = mining?.job;
  const showJobCard = job && job.state !== 'completed' && job.id !== dismissedJobId;

  const api = useMemo<WalletActionsApi>(
    () => ({
      openSend: (accountId, draft) => {
        setFormVersion((version) => version + 1);
        setSendAccount(accountId);
        setSendDraft(draft);
        setDialog('send');
      },
      openRequest: () => setDialog('request'),
      openFaucet: (accountId, draft) => {
        setFormVersion((version) => version + 1);
        setFaucetAccount(accountId);
        setFaucetDraft(draft);
        setDialog('faucet');
      },
      openMine: () => setDialog('mine'),
    }),
    [],
  );

  const close = (open: boolean) => {
    if (!open) setDialog(null);
  };

  return (
    <WalletActionsContext value={api}>
      <WalletPaymentsContext
        value={{
          send,
          faucet,
          savedSend,
          savedFaucet,
          isRestored: (key) => !liveKeys.current.has(key),
          viewedPayment,
          viewPayment,
          closePayments: () => setViewedPayment(null),
        }}
      >
        {children}
        {showJobCard && <div aria-hidden className="h-64 md:hidden" />}
        {showJobCard && (
          <section
            aria-label="Mining job"
            className="xp-raised bg-panel fixed right-4 bottom-20 z-20 max-h-44 w-[min(380px,calc(100vw-32px))] space-y-3 overflow-y-auto rounded-xs p-4 shadow-(--shadow-float) md:right-6 md:bottom-6 md:max-h-[calc(100dvh-3rem)]"
          >
            <MiningStatus job={job} />
            <Button size="sm" onClick={() => setDialog('mine')}>
              View mining
            </Button>
            {job.state === 'failed' && (
              <Button size="sm" variant="ghost" onClick={() => setDismissedJobId(job.id)}>
                Dismiss mining job
              </Button>
            )}
          </section>
        )}
        {/* An explicit open replaces the form even for the same account/draft. */}
        {dialog === 'send' && (
          <SendDialog
            key={formVersion}
            open
            onOpenChange={close}
            accounts={accounts}
            draft={sendDraft}
            {...(sendAccount === undefined ? {} : { defaultAccountId: sendAccount })}
          />
        )}
        {dialog === 'request' && (
          <RequestDialog key="request" open onOpenChange={close} accounts={accounts} />
        )}
        {dialog === 'faucet' && (
          <FaucetDialog
            key={formVersion}
            open
            onOpenChange={close}
            accounts={accounts}
            draft={faucetDraft}
            {...(faucetAccount === undefined ? {} : { defaultAccountId: faucetAccount })}
          />
        )}
        {dialog === 'mine' && <MineDialog key="mine" open onOpenChange={close} />}
      </WalletPaymentsContext>
    </WalletActionsContext>
  );
}

import { zodResolver } from '@hookform/resolvers/zod';
import { useEffect, useRef, useState, type FormEvent } from 'react';
import { useForm, useWatch } from 'react-hook-form';
import { ArrowRight } from 'lucide-react';
import { Dialog } from '@/components/ui/Dialog';
import { Button } from '@/components/ui/Button';
import { Field } from '@/components/ui/Field';
import { useToast } from '@/components/ui/toast-context';
import { api, errorMessage, type Account, type PaymentUri } from '@/lib/api';
import { shortHash } from '@/lib/format';
import { formatZec, formatZecAmount } from '@/lib/money';
import { useParsePaymentUri, type PaymentResult, type SendVariables } from '@/hooks/mutations';
import { paymentName, type PaymentIdentity } from '@/hooks/payment-identity';
import { isTerminalPayment, usePaymentStatus } from '@/hooks/queries';
import { useWalletPayments } from './payment-context';
import { useSendQuote } from '@/hooks/queries';
import {
  ADDRESS_DESTINATION,
  MEMO_MAX_BYTES,
  memoByteLength,
  sendSchema,
  type SendInput,
  type SendValues,
} from './schemas';
import { controlStyles } from '@/components/ui/control-styles';
import { cn } from '@/lib/cn';
import { SelectField } from './fields';
import { POOL_OPTIONS, accountOptions } from './field-options';
import { PaymentWarning } from './PaymentStatus';

export function SendDialog({
  open,
  onOpenChange,
  accounts,
  defaultAccountId,
  draft,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  accounts: Account[];
  defaultAccountId?: number;
  draft?: PaymentIdentity | undefined;
}) {
  const toast = useToast();
  const parseUri = useParsePaymentUri();
  const [paymentUri, setPaymentUri] = useState('');
  const paymentUriRevision = useRef(0);
  const verifiedAddress = useRef<{
    address: string;
    destination_pool: 'ironwood' | 'transparent';
  } | null>(null);
  const [paymentUriNeedsApply, setPaymentUriNeedsApply] = useState(false);
  const {
    send,
    savedSend: payments,
    send: sendOperation,
    faucet: faucetOperation,
    viewPayment,
  } = useWalletPayments();
  const busy = sendOperation.isPending || faucetOperation.isPending;
  const mounted = useRef(true);
  const sendSubmissionPending = useRef(false);
  const [checkingSendQuote, setCheckingSendQuote] = useState(false);
  const releaseSendSubmission = () => {
    sendSubmissionPending.current = false;
    setCheckingSendQuote(false);
  };
  const [submittedHere, setSubmittedHere] = useState(false);
  const [receipt, setReceipt] = useState<{
    text: string;
    kind: 'accepted' | 'error';
    key?: string | undefined;
  } | null>(null);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);
  const previous = draft?.kind === 'send' ? draft.variables : undefined;
  const fromAccountId = previous?.from_account ?? defaultAccountId ?? 1;

  const form = useForm<SendInput, unknown, SendValues>({
    resolver: zodResolver(sendSchema),
    defaultValues: {
      from_account: String(fromAccountId),
      to_account: previous?.to_address
        ? ADDRESS_DESTINATION
        : String(
            previous?.to_account ??
              accounts.find((account) => account.id !== fromAccountId)?.id ??
              1,
          ),
      to_address: previous?.to_address ?? '',
      source_pool: previous?.source_pool ?? 'ironwood',
      destination_pool: previous?.destination_pool ?? 'ironwood',
      amount: previous ? formatZec(previous.amount_zatoshi, { grouping: false }) : '1',
      memo: previous?.memo ?? '',
    },
  });

  const current = sendSchema.safeParse(useWatch({ control: form.control }));
  const variablesFor = (values: SendValues) => ({
    from_account: values.from_account,
    ...(values.to_account === ADDRESS_DESTINATION
      ? { to_address: values.to_address }
      : { to_account: values.to_account }),
    source_pool: values.source_pool,
    destination_pool: values.destination_pool,
    amount_zatoshi: values.amount,
    ...(values.memo === '' ? {} : { memo: values.memo }),
  });
  // A blank field represents both absent and explicit empty saved memos.
  // Compare their form values here; Resume still sends the original identity.
  const formName = (variables: SendVariables) =>
    paymentName('send', { ...variables, memo: variables.memo ?? '' });
  const retainedFor = (variables: SendVariables) =>
    payments.retained.find(
      (item) => item.kind === 'send' && formName(item.variables) === formName(variables),
    );
  const match = current.success ? retainedFor(variablesFor(current.data)) : undefined;
  const matchStatus = usePaymentStatus(match?.key ?? '', Boolean(match));
  const matchTerminal = isTerminalPayment(matchStatus.data);
  const retryLabel = match && !matchTerminal ? `Resume payment` : null;
  const announce = ({ activity, key }: PaymentResult) => {
    if (!send.claimAnnouncement(key)) return;
    toast.success(
      activity.to_account === null
        ? `Sent to ${shortHash(activity.to_address ?? '', 14, 6)}`
        : `Sent to Account ${activity.to_account}`,
      `${formatZecAmount(activity.amount_zatoshi)} confirmed.`,
    );
  };
  const failed = (error: Error, _variables: SendVariables, key: string | undefined) => {
    if (!mounted.current) return;
    setReceipt({ text: errorMessage(error), kind: 'error', key });
  };
  // A direct confirmation finishes only the submitted draft. Inputs remain
  // editable during the POST, and a later confirmation must preserve edits.
  const submitted = (uriRevision: number) => ({
    onError: (error: Error, variables: SendVariables, key: string | undefined) => {
      releaseSendSubmission();
      failed(error, variables, key);
    },
    onSuccess: (result: PaymentResult, variables: SendVariables) => {
      releaseSendSubmission();
      if (!mounted.current) return;
      if (result.activity.status === 'confirmed') announce(result);
      if (result.activity.status === 'failed') {
        setReceipt({
          text: `Payment failed: ${result.activity.last_error ?? 'The server rejected this payment.'}`,
          kind: 'error',
          key: result.key,
        });
        return;
      }
      if (result.cleanupError) return;
      const draft = sendSchema.safeParse(form.getValues());
      if (
        uriRevision !== paymentUriRevision.current ||
        !draft.success ||
        formName(variablesFor(draft.data)) !== formName(variables)
      ) {
        setReceipt({
          text: `Payment ${result.activity.status === 'confirmed' ? 'confirmed' : 'accepted'}. Your draft is unchanged. View status in Payments.`,
          kind: 'accepted',
        });
        return;
      }
      onOpenChange(false);
      form.reset();
      setPaymentUri('');
      setPaymentUriNeedsApply(false);
      parseUri.reset();
    },
  });

  const fromAccount = useWatch({ control: form.control, name: 'from_account' });
  const toAccount = useWatch({ control: form.control, name: 'to_account' });
  const toAddress = useWatch({ control: form.control, name: 'to_address' }) ?? '';
  const sourcePool = useWatch({ control: form.control, name: 'source_pool' });
  const destinationPool = useWatch({ control: form.control, name: 'destination_pool' });
  const memo = useWatch({ control: form.control, name: 'memo' }) ?? '';
  const memoEnabled = destinationPool === 'ironwood';

  // The server validates the full address before a new payment. The prefix
  // selects the quote pool while the address is being entered.
  const addressPool = /^t/.test(toAddress.trim())
    ? 'transparent'
    : /^uregtest1/.test(toAddress.trim())
      ? 'ironwood'
      : undefined;
  useEffect(() => {
    if (toAccount === ADDRESS_DESTINATION && addressPool && destinationPool !== addressPool) {
      form.setValue('destination_pool', addressPool, {
        shouldValidate: form.formState.isSubmitted,
      });
    }
  }, [toAccount, addressPool, destinationPool, form]);

  // A memo typed for an ironwood output must not linger (hidden) once the
  // destination switches to transparent, where it can never be sent.
  useEffect(() => {
    if (!memoEnabled) {
      form.setValue('memo', '');
      form.clearErrors('memo');
    }
  }, [memoEnabled, form]);

  const source = accounts.find((account) => account.id === Number(fromAccount));
  const available =
    source === undefined
      ? 0n
      : sourcePool === 'ironwood'
        ? source.ironwood_zatoshi
        : source.transparent_zatoshi;

  // The quote is a dry-run proposal, so its fee reflects real input selection.
  const quote = useSendQuote({
    from_account: Number(fromAccount),
    source_pool: sourcePool,
    destination_pool: destinationPool,
  });

  const applyPaymentUri = (parsed: PaymentUri) => {
    verifiedAddress.current = {
      address: parsed.address,
      destination_pool: parsed.destination_pool,
    };
    const options = { shouldValidate: form.formState.isSubmitted };
    form.setValue(
      'to_account',
      parsed.to_account === null ? ADDRESS_DESTINATION : String(parsed.to_account),
      options,
    );
    form.setValue('to_address', parsed.address, options);
    form.setValue('destination_pool', parsed.destination_pool, options);
    // A URI without an amount must not inherit the form's default: the sender
    // has to choose one rather than unknowingly send it.
    form.setValue(
      'amount',
      parsed.amount_zatoshi === null ? '' : formatZec(parsed.amount_zatoshi).replaceAll(',', ''),
      options,
    );
    form.setValue('memo', parsed.memo ?? '', options);
  };

  const submitPaymentUri = () => {
    if (paymentUri.trim() === '') return;
    paymentUriRevision.current += 1;
    const uriRevision = paymentUriRevision.current;
    setPaymentUriNeedsApply(true);
    parseUri.mutate(paymentUri, {
      onSuccess: (parsed) => {
        if (uriRevision === paymentUriRevision.current) {
          applyPaymentUri(parsed);
          setPaymentUriNeedsApply(false);
        }
      },
    });
  };

  const submit = (event: FormEvent<HTMLFormElement>) => {
    const uriRevision = paymentUriRevision.current;
    return form.handleSubmit(async (values) => {
      // Replaying retained work must not depend on the account's current balance
      // or a new proposal; it may already have spent those funds.
      if (
        busy ||
        sendSubmissionPending.current ||
        matchTerminal ||
        parseUri.isPending ||
        paymentUriNeedsApply ||
        uriRevision !== paymentUriRevision.current
      )
        return;
      sendSubmissionPending.current = true;
      setSubmittedHere(true);
      setReceipt(null);
      const variables = variablesFor(values);
      const retained = retainedFor(variables);
      if (retained) {
        send.resume(retained, submitted(uriRevision));
        return;
      }
      if (values.to_account === ADDRESS_DESTINATION) {
        try {
          const parsed =
            verifiedAddress.current?.address === values.to_address &&
            verifiedAddress.current.destination_pool === values.destination_pool
              ? verifiedAddress.current
              : await api.parsePaymentUri(`zcash:${values.to_address}`);
          if (
            parsed.address !== values.to_address ||
            parsed.destination_pool !== values.destination_pool
          ) {
            form.setValue('destination_pool', parsed.destination_pool);
            form.setError('to_address', {
              message: 'The address changed the destination pool. Review the fee and submit again.',
            });
            releaseSendSubmission();
            return;
          }
          verifiedAddress.current = {
            address: parsed.address,
            destination_pool: parsed.destination_pool,
          };
        } catch (error) {
          form.setError('to_address', { message: errorMessage(error) });
          releaseSendSubmission();
          return;
        }
        const currentDraft = sendSchema.safeParse(form.getValues());
        if (
          !currentDraft.success ||
          formName(variablesFor(currentDraft.data)) !== formName(variables)
        ) {
          releaseSendSubmission();
          return;
        }
      }
      if (values.amount > available) {
        form.setError('amount', {
          message: `Account ${values.from_account} holds ${formatZecAmount(available)} in the ${values.source_pool} pool.`,
        });
        releaseSendSubmission();
        return;
      }
      let quoted = quote.data;
      if (quoted === undefined) {
        setCheckingSendQuote(true);
        try {
          quoted = (await quote.refetch()).data;
        } catch (error) {
          releaseSendSubmission();
          throw error;
        } finally {
          setCheckingSendQuote(false);
        }
      }
      if (uriRevision !== paymentUriRevision.current) {
        releaseSendSubmission();
        return;
      }
      const currentDraft = sendSchema.safeParse(form.getValues());
      if (
        !currentDraft.success ||
        formName(variablesFor(currentDraft.data)) !== formName(variables)
      ) {
        setReceipt({
          text: 'The send details changed while checking the fee. Review the current draft and submit again.',
          kind: 'error',
        });
        releaseSendSubmission();
        return;
      }
      if (quoted !== undefined && values.amount > quoted.max_zatoshi) {
        form.setError('amount', {
          message: `The ${formatZecAmount(quoted.fee_zatoshi)} network fee leaves at most ${formatZecAmount(quoted.max_zatoshi)} spendable — Account ${values.from_account} holds ${formatZecAmount(quoted.available_zatoshi)} in the ${values.source_pool} pool.`,
        });
        releaseSendSubmission();
        return;
      }

      send.mutate(variables, submitted(uriRevision));
    })(event);
  };

  const options = accountOptions(accounts);
  const destinationOptions = [...options, { value: ADDRESS_DESTINATION, label: 'Other address' }];

  return (
    <Dialog
      open={open}
      onOpenChange={onOpenChange}
      eyebrow="NEW TRANSACTION"
      title="Send ZEC"
      description="Moves existing funds to another account, pool, or Regtest address."
    >
      <Field
        label="Payment request (optional)"
        hint={
          paymentUriNeedsApply
            ? 'Apply this payment request or clear it before sending.'
            : 'Paste a zcash: URI to fill in the destination, amount, and memo.'
        }
        error={parseUri.isError ? errorMessage(parseUri.error) : undefined}
      >
        {(aria) => (
          <div className="flex gap-2">
            <input
              {...aria}
              value={paymentUri}
              onChange={(event) => {
                paymentUriRevision.current += 1;
                setPaymentUri(event.target.value);
                setPaymentUriNeedsApply(event.target.value.trim() !== '');
                parseUri.reset();
              }}
              onKeyDown={(event) => {
                if (event.key === 'Enter') {
                  event.preventDefault();
                  submitPaymentUri();
                }
              }}
              placeholder="zcash:uregtest1…?amount=1"
              autoComplete="off"
              spellCheck={false}
              className={controlStyles}
            />
            <Button
              type="button"
              onClick={submitPaymentUri}
              loading={parseUri.isPending}
              disabled={paymentUri.trim() === ''}
            >
              Apply
            </Button>
          </div>
        )}
      </Field>

      {payments.error && (
        <PaymentWarning role="alert" className="mb-3">
          <p>Could not load saved payments: {errorMessage(payments.error)}</p>
          <p>Allow site storage and reload before submitting.</p>
        </PaymentWarning>
      )}
      {receipt &&
        (receipt.kind === 'accepted' ? (
          <p role="status" className="text-ink-muted mb-3 text-[12px]">
            {receipt.text}
          </p>
        ) : (
          <PaymentWarning role="alert" className="mb-3">
            <p>{receipt.text}</p>
            {receipt.key && (
              <Button size="sm" className="mt-2" onClick={() => viewPayment(receipt.key)}>
                View payment
              </Button>
            )}
          </PaymentWarning>
        ))}
      {match && (
        <p className="text-ink-muted mb-3 text-[12px]" role="status">
          {matchTerminal
            ? 'Previous payment finished. Clear its saved request in Payments before sending these details again.'
            : `A saved payment has these details (${matchStatus.data?.status ?? 'status unknown'}). Resume it, or change the details for a new payment.`}
        </p>
      )}
      <form onSubmit={(event) => void submit(event)} noValidate>
        <div className="grid gap-x-3 sm:grid-cols-2">
          <SelectField
            control={form.control}
            name="from_account"
            label="From account"
            options={options}
            error={form.formState.errors.from_account?.message}
          />
          <SelectField
            control={form.control}
            name="source_pool"
            label="Source pool"
            options={POOL_OPTIONS}
            error={form.formState.errors.source_pool?.message}
          />
          <SelectField
            control={form.control}
            name="to_account"
            label="Destination"
            options={destinationOptions}
            error={form.formState.errors.to_account?.message}
          />
          {toAccount === ADDRESS_DESTINATION ? (
            <Field label="Destination pool" hint="Chosen from the address.">
              {() => (
                <p className="text-ink-muted py-2 text-[13px]">
                  {addressPool === 'transparent'
                    ? 'Transparent (public)'
                    : addressPool === 'ironwood'
                      ? 'Ironwood (shielded)'
                      : 'Enter a Regtest address'}
                </p>
              )}
            </Field>
          ) : (
            <SelectField
              control={form.control}
              name="destination_pool"
              label="Destination pool"
              options={POOL_OPTIONS}
              error={form.formState.errors.destination_pool?.message}
            />
          )}
        </div>

        {toAccount === ADDRESS_DESTINATION && (
          <Field
            label="Destination address"
            hint="A unified (Ironwood) or transparent Regtest address."
            error={form.formState.errors.to_address?.message}
          >
            {(aria) => (
              <input
                {...aria}
                {...form.register('to_address', {
                  onChange: () => {
                    verifiedAddress.current = null;
                  },
                })}
                autoComplete="off"
                spellCheck={false}
                className={controlStyles}
              />
            )}
          </Field>
        )}

        <Field
          label="Amount (ZEC)"
          hint={
            quote.data !== undefined
              ? `${formatZecAmount(quote.data.max_zatoshi)} spendable after a ${formatZecAmount(quote.data.fee_zatoshi)} network fee.`
              : `${formatZecAmount(available)} available in the ${sourcePool} pool.`
          }
          error={form.formState.errors.amount?.message}
        >
          {(aria) => (
            <div className="flex items-stretch gap-2">
              <input
                {...aria}
                {...form.register('amount')}
                inputMode="decimal"
                autoComplete="off"
                className={controlStyles}
              />
              <Button
                type="button"
                variant="subtle"
                size="sm"
                disabled={quote.data === undefined || quote.data.max_zatoshi === 0n}
                onClick={() => {
                  const max = quote.data?.max_zatoshi;
                  if (max === undefined) return;
                  form.setValue('amount', formatZec(max, { grouping: false }), {
                    shouldDirty: true,
                    shouldValidate: true,
                  });
                  form.clearErrors('amount');
                }}
              >
                Max
              </Button>
            </div>
          )}
        </Field>

        <Field
          label="Memo (optional)"
          hint={
            memoEnabled
              ? `${memoByteLength(memo)}/${MEMO_MAX_BYTES} bytes.`
              : 'Memos are only available for ironwood destinations.'
          }
          error={form.formState.errors.memo?.message}
        >
          {(aria) => (
            <textarea
              {...aria}
              {...form.register('memo')}
              disabled={!memoEnabled}
              placeholder={memoEnabled ? 'Add a private note for the recipient' : undefined}
              rows={2}
              autoComplete="off"
              className={cn(controlStyles, 'disabled:cursor-not-allowed disabled:opacity-50')}
            />
          )}
        </Field>

        <Button
          type="submit"
          variant="primary"
          size="block"
          loading={busy || checkingSendQuote}
          disabled={
            busy ||
            checkingSendQuote ||
            matchTerminal ||
            Boolean(payments.error) ||
            parseUri.isPending ||
            paymentUriNeedsApply
          }
        >
          {checkingSendQuote
            ? 'Checking fee…'
            : busy
              ? send.isPending && submittedHere
                ? 'Submitting payment…'
                : 'Another payment is submitting…'
              : (retryLabel ?? 'Send ZEC')}
          {!send.isPending && <ArrowRight />}
        </Button>
      </form>
    </Dialog>
  );
}

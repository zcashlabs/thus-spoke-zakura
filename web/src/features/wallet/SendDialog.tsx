import { zodResolver } from '@hookform/resolvers/zod';
import { useEffect, useState } from 'react';
import { useForm, useWatch } from 'react-hook-form';
import { ArrowRight } from 'lucide-react';
import { Dialog } from '@/components/ui/Dialog';
import { Button } from '@/components/ui/Button';
import { Field } from '@/components/ui/Field';
import { useToast } from '@/components/ui/toast-context';
import { errorMessage, type Account, type PaymentUri } from '@/lib/api';
import { shortHash } from '@/lib/format';
import { formatZec, formatZecAmount } from '@/lib/money';
import { useParsePaymentUri, useSend } from '@/hooks/mutations';
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

export function SendDialog({
  open,
  onOpenChange,
  accounts,
  defaultAccountId,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  accounts: Account[];
  defaultAccountId?: number;
}) {
  const toast = useToast();
  const send = useSend();
  const parseUri = useParsePaymentUri();
  const [paymentUri, setPaymentUri] = useState('');
  const fromAccountId = defaultAccountId ?? 1;

  const form = useForm<SendInput, unknown, SendValues>({
    resolver: zodResolver(sendSchema),
    defaultValues: {
      from_account: String(fromAccountId),
      to_account: String(accounts.find((account) => account.id !== fromAccountId)?.id ?? 1),
      to_address: '',
      source_pool: 'orchard',
      destination_pool: 'orchard',
      amount: '1',
      memo: '',
    },
  });

  const fromAccount = useWatch({ control: form.control, name: 'from_account' });
  const toAccount = useWatch({ control: form.control, name: 'to_account' });
  const sourcePool = useWatch({ control: form.control, name: 'source_pool' });
  const destinationPool = useWatch({ control: form.control, name: 'destination_pool' });
  const memo = useWatch({ control: form.control, name: 'memo' }) ?? '';
  const memoEnabled = destinationPool === 'orchard';

  // A memo typed for an orchard output must not linger (hidden) once the
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
      : sourcePool === 'orchard'
        ? source.orchard_zatoshi
        : source.transparent_zatoshi;

  // The quote is a dry-run proposal, so its fee reflects real input selection.
  const quote = useSendQuote({
    from_account: Number(fromAccount),
    source_pool: sourcePool,
    destination_pool: destinationPool,
  });

  const applyPaymentUri = (parsed: PaymentUri) => {
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
    parseUri.mutate(paymentUri, { onSuccess: applyPaymentUri });
  };

  const submit = form.handleSubmit(async (values) => {
    if (values.amount > available) {
      form.setError('amount', {
        message: `Account ${values.from_account} holds ${formatZecAmount(available)} in the ${values.source_pool} pool.`,
      });
      return;
    }
    const quoted = quote.data ?? (await quote.refetch()).data;
    if (quoted !== undefined && values.amount > quoted.max_zatoshi) {
      form.setError('amount', {
        message: `The ${formatZecAmount(quoted.fee_zatoshi)} network fee leaves at most ${formatZecAmount(quoted.max_zatoshi)} spendable — Account ${values.from_account} holds ${formatZecAmount(quoted.available_zatoshi)} in the ${values.source_pool} pool.`,
      });
      return;
    }

    send.mutate(
      {
        from_account: values.from_account,
        ...(values.to_account === ADDRESS_DESTINATION
          ? { to_address: values.to_address }
          : { to_account: values.to_account }),
        source_pool: values.source_pool,
        destination_pool: values.destination_pool,
        amount_zatoshi: values.amount,
        ...(values.memo === '' ? {} : { memo: values.memo }),
      },
      {
        onSuccess: (activity) => {
          toast.success(
            activity.to_account === null
              ? `Sent to ${shortHash(activity.to_address ?? '', 14, 6)}`
              : `Sent to Account ${activity.to_account}`,
            'The transaction was mined into a new block.',
          );
          onOpenChange(false);
          form.reset();
          setPaymentUri('');
          parseUri.reset();
        },
        onError: (error) => toast.error('Transfer failed', errorMessage(error)),
      },
    );
  });

  const options = accountOptions(accounts);
  const destinationOptions = [...options, { value: ADDRESS_DESTINATION, label: 'Other address' }];

  return (
    <Dialog
      open={open}
      onOpenChange={onOpenChange}
      eyebrow="NEW TRANSACTION"
      title="Send ZEC"
      description="Sends from a development account to another account or any Regtest address. One block is mined to confirm."
    >
      <Field
        label="Payment request (optional)"
        hint="Paste a zcash: URI to fill in the destination, amount, and memo."
        error={parseUri.isError ? errorMessage(parseUri.error) : undefined}
      >
        {(aria) => (
          <div className="flex gap-2">
            <input
              {...aria}
              value={paymentUri}
              onChange={(event) => setPaymentUri(event.target.value)}
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
          <SelectField
            control={form.control}
            name="destination_pool"
            label="Destination pool"
            options={POOL_OPTIONS}
            error={form.formState.errors.destination_pool?.message}
          />
        </div>

        {toAccount === ADDRESS_DESTINATION && (
          <Field
            label="Destination address"
            hint="A unified (Orchard) or transparent Regtest address."
            error={form.formState.errors.to_address?.message}
          >
            {(aria) => (
              <input
                {...aria}
                {...form.register('to_address')}
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
                  form.setValue('amount', formatZec(max), {
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
              ? `${memoByteLength(memo)}/${MEMO_MAX_BYTES} bytes, encrypted to the recipient.`
              : 'Memos are only available for orchard destinations.'
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

        <Button type="submit" variant="primary" size="block" loading={send.isPending}>
          {send.isPending ? 'Sending…' : 'Send ZEC'}
          {!send.isPending && <ArrowRight />}
        </Button>
      </form>
    </Dialog>
  );
}

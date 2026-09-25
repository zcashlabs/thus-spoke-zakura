import { zodResolver } from '@hookform/resolvers/zod';
import { useForm, useWatch } from 'react-hook-form';
import { ArrowRight } from 'lucide-react';
import { Dialog } from '@/components/ui/Dialog';
import { Button } from '@/components/ui/Button';
import { Field } from '@/components/ui/Field';
import { useToast } from '@/components/ui/toast-context';
import { errorMessage, type Account } from '@/lib/api';
import { formatZec, formatZecAmount } from '@/lib/money';
import { useSend } from '@/hooks/mutations';
import { useSendQuote } from '@/hooks/queries';
import { sendSchema, type SendInput, type SendValues } from './schemas';
import { controlStyles } from '@/components/ui/control-styles';
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
  const fromAccountId = defaultAccountId ?? 1;

  const form = useForm<SendInput, unknown, SendValues>({
    resolver: zodResolver(sendSchema),
    defaultValues: {
      from_account: String(fromAccountId),
      to_account: String(accounts.find((account) => account.id !== fromAccountId)?.id ?? 1),
      source_pool: 'orchard',
      destination_pool: 'orchard',
      amount: '1',
    },
  });

  const fromAccount = useWatch({ control: form.control, name: 'from_account' });
  const sourcePool = useWatch({ control: form.control, name: 'source_pool' });
  const destinationPool = useWatch({ control: form.control, name: 'destination_pool' });
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
        to_account: values.to_account,
        source_pool: values.source_pool,
        destination_pool: values.destination_pool,
        amount_zatoshi: values.amount,
      },
      {
        onSuccess: (activity) => {
          toast.success(
            `Sent to Account ${activity.to_account}`,
            'The transaction was mined into a new block.',
          );
          onOpenChange(false);
          form.reset();
        },
        onError: (error) => toast.error('Transfer failed', errorMessage(error)),
      },
    );
  });

  const options = accountOptions(accounts);

  return (
    <Dialog
      open={open}
      onOpenChange={onOpenChange}
      eyebrow="NEW TRANSACTION"
      title="Send ZEC"
      description="Moves existing funds between development accounts. One block is mined to confirm."
    >
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
            label="Destination account"
            options={options}
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

        <Button type="submit" variant="primary" size="block" loading={send.isPending}>
          {send.isPending ? 'Sending…' : 'Send ZEC'}
          {!send.isPending && <ArrowRight />}
        </Button>
      </form>
    </Dialog>
  );
}

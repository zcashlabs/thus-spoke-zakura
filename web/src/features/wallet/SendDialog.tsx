import { zodResolver } from '@hookform/resolvers/zod';
import { useEffect } from 'react';
import { useForm, useWatch } from 'react-hook-form';
import { ArrowRight } from 'lucide-react';
import { Dialog } from '@/components/ui/Dialog';
import { Button } from '@/components/ui/Button';
import { Field } from '@/components/ui/Field';
import { useToast } from '@/components/ui/toast-context';
import { errorMessage, type Account } from '@/lib/api';
import { formatZecAmount } from '@/lib/money';
import { useSend } from '@/hooks/mutations';
import {
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
  const fromAccountId = defaultAccountId ?? 1;

  const form = useForm<SendInput, unknown, SendValues>({
    resolver: zodResolver(sendSchema),
    defaultValues: {
      from_account: String(fromAccountId),
      to_account: String(accounts.find((account) => account.id !== fromAccountId)?.id ?? 1),
      source_pool: 'orchard',
      destination_pool: 'orchard',
      amount: '1',
      memo: '',
    },
  });

  const fromAccount = useWatch({ control: form.control, name: 'from_account' });
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

  const submit = form.handleSubmit((values) => {
    if (values.amount > available) {
      form.setError('amount', {
        message: `Account ${values.from_account} holds ${formatZecAmount(available)} in the ${values.source_pool} pool.`,
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
        ...(values.memo === '' ? {} : { memo: values.memo }),
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
          hint={`${formatZecAmount(available)} available in the ${sourcePool} pool.`}
          error={form.formState.errors.amount?.message}
        >
          {(aria) => (
            <input
              {...aria}
              {...form.register('amount')}
              inputMode="decimal"
              autoComplete="off"
              className={controlStyles}
            />
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

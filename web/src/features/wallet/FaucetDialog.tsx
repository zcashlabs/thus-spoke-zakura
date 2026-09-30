import { zodResolver } from '@hookform/resolvers/zod';
import { useForm } from 'react-hook-form';
import { Dialog } from '@/components/ui/Dialog';
import { Button } from '@/components/ui/Button';
import { Field } from '@/components/ui/Field';
import { useToast } from '@/components/ui/toast-context';
import { errorMessage, type Account } from '@/lib/api';
import { formatZecAmount } from '@/lib/money';
import { useFaucet } from '@/hooks/mutations';
import { faucetSchema, type FaucetInput, type FaucetValues } from './schemas';
import { controlStyles } from '@/components/ui/control-styles';
import { SelectField } from './fields';
import { POOL_OPTIONS, accountOptions } from './field-options';

export function FaucetDialog({
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
  const faucet = useFaucet();

  const form = useForm<FaucetInput, unknown, FaucetValues>({
    resolver: zodResolver(faucetSchema),
    mode: 'onChange',
    // Defaults to 1 ZEC rather than the 5 ZEC ceiling: the treasury is funded
    // from block rewards, and requests at the maximum are the first to fail
    // once the subsidy has halved.
    defaultValues: { account_id: String(defaultAccountId ?? 1), pool: 'orchard', amount: '1' },
  });

  const submit = form.handleSubmit((values) => {
    faucet.mutate(
      { account_id: values.account_id, pool: values.pool, amount_zatoshi: values.amount },
      {
        onSuccess: (activity) => {
          toast.success(
            `Funded Account ${values.account_id}`,
            `${formatZecAmount(activity.amount_zatoshi)} confirmed.`,
          );
          onOpenChange(false);
          form.reset();
        },
        onError: (error) => toast.error('Faucet request failed', errorMessage(error)),
      },
    );
  });

  return (
    <Dialog
      open={open}
      onOpenChange={onOpenChange}
      eyebrow="DEV TOOLS"
      title="Fund an account"
      description="Creates new test funds from the mining treasury and mines a block to confirm."
    >
      <form onSubmit={(event) => void submit(event)} noValidate>
        <SelectField
          control={form.control}
          name="account_id"
          label="Destination account"
          options={accountOptions(accounts)}
          error={form.formState.errors.account_id?.message}
        />
        <SelectField
          control={form.control}
          name="pool"
          label="Destination pool"
          options={POOL_OPTIONS}
          error={form.formState.errors.pool?.message}
        />
        <Field
          label="Amount (ZEC)"
          hint="Maximum 5 ZEC per request."
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

        <Button
          type="submit"
          variant="primary"
          size="block"
          loading={faucet.isPending}
          disabled={!form.formState.isValid || faucet.isPending}
        >
          {faucet.isPending ? 'Requesting…' : 'Add funds'}
        </Button>
      </form>
    </Dialog>
  );
}

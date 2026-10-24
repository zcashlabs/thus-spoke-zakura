import { zodResolver } from '@hookform/resolvers/zod';
import { useForm } from 'react-hook-form';
import { Pickaxe } from 'lucide-react';
import { Dialog } from '@/components/ui/Dialog';
import { Button } from '@/components/ui/Button';
import { Field } from '@/components/ui/Field';
import { errorMessage } from '@/lib/api';
import { useStartMining } from '@/hooks/mutations';
import { useMiningJob } from '@/hooks/queries';
import { MiningStatus } from './MiningStatus';
import { mineSchema, type MineInput, type MineValues } from './schemas';
import { controlStyles } from '@/components/ui/control-styles';

export function MineDialog({
  open,
  onOpenChange,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const mine = useStartMining();
  const mining = useMiningJob();
  const job = mining.data?.job;
  const active = job?.state === 'mining' || job?.state === 'syncing';
  const disabled = mining.isPending || mining.isError || active || mine.isPending;

  const form = useForm<MineInput, unknown, MineValues>({
    resolver: zodResolver(mineSchema),
    defaultValues: { blocks: '1' },
  });

  const submit = form.handleSubmit((values) => {
    if (disabled) return;
    mine.mutate(values.blocks);
  });

  return (
    <Dialog
      open={open}
      onOpenChange={onOpenChange}
      eyebrow="CHAIN CONTROL"
      title="Mine blocks"
      description="Regtest mines on demand, with no proof-of-work delay."
    >
      {job && <MiningStatus job={job} />}
      {mining.isPending && <p className="text-ink-muted text-[13px]">Checking mining status…</p>}
      {mining.isError && (
        <p role="alert" className="text-negative text-[13px]">
          Unable to check mining status: {errorMessage(mining.error)}
        </p>
      )}
      {mine.isError && (
        <p role="alert" className="text-negative text-[13px]">
          Unable to start mining: {errorMessage(mine.error)}
        </p>
      )}
      {mine.cleanupError && (
        <p role="alert" className="text-negative text-[13px]">
          Mining job accepted, but browser storage cleanup failed: {mine.cleanupError}
        </p>
      )}
      <form onSubmit={(event) => void submit(event)} noValidate>
        <Field
          label="Number of blocks"
          hint="Between 1 and 10,000."
          error={form.formState.errors.blocks?.message}
        >
          {(aria) => (
            <input
              {...aria}
              {...form.register('blocks')}
              disabled={disabled}
              inputMode="numeric"
              autoComplete="off"
              className={controlStyles}
            />
          )}
        </Field>

        <Button
          type="submit"
          variant="primary"
          size="block"
          loading={mine.isPending || active}
          disabled={disabled}
        >
          <Pickaxe />
          {job?.state === 'syncing'
            ? 'Synchronizing…'
            : mine.isPending || active
              ? 'Mining…'
              : 'Mine blocks'}
        </Button>
      </form>
    </Dialog>
  );
}

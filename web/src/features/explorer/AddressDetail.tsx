import { Link } from 'react-router-dom';
import { useParams } from 'react-router-dom';
import { Copy } from 'lucide-react';
import { DataRow, Panel, PanelNote } from '@/components/ui/Panel';
import { CopyButton } from '@/components/ui/CopyButton';
import { Badge } from '@/components/ui/Badge';
import { ErrorState, LoadingState } from '@/components/ui/StateBlock';
import { useAddress } from '@/hooks/queries';
import { errorMessage } from '@/lib/api';
import { formatZecAmount } from '@/lib/money';
import { shortHash } from '@/lib/format';
import { Stat } from '@/components/ui/Stat';
import { BACK_LINK } from './back-link';

export function AddressDetail() {
  const { address = '' } = useParams<{ address: string }>();
  const info = useAddress(address);

  if (info.isPending) return <LoadingState label="Loading address…" />;
  if (info.isError) {
    return (
      <div className="grid gap-4">
        <h1 className="text-2xl font-bold tracking-[-0.02em]">Address</h1>
        <ErrorState
          message={errorMessage(info.error)}
          action={
            <Link to="/explorer" className={BACK_LINK}>
              Back to blocks
            </Link>
          }
        />
      </div>
    );
  }

  const { balance, received } = info.data.balance;
  const spent = received - balance;

  return (
    <div className="grid gap-4">
      <header className="flex flex-wrap items-center justify-between gap-3">
        <div className="flex items-center gap-3">
          <h1 className="text-2xl font-bold tracking-[-0.02em]">Address</h1>
          <code className="text-ink-muted font-mono text-[13px]">
            {shortHash(info.data.address, 12, 8)}
          </code>
          <CopyButton
            value={info.data.address}
            label="Copy address"
            icon={<Copy className="size-4" />}
          />
        </div>
        <Badge>Transparent</Badge>
      </header>

      <section className="grid gap-3 sm:grid-cols-3">
        <Stat
          label="Balance"
          value={formatZecAmount(BigInt(balance))}
          tone={balance > 0 ? 'accent' : 'muted'}
        />
        <Stat label="Total received" value={formatZecAmount(BigInt(received))} />
        <Stat label="Total sent" value={formatZecAmount(BigInt(Math.max(spent, 0)))} />
      </section>

      <Panel eyebrow="DETAILS" title="Address">
        <dl>
          <DataRow label="Address">
            <code className="font-mono">{info.data.address}</code>
          </DataRow>
          <DataRow label="Type">Transparent</DataRow>
        </dl>
        <PanelNote>
          Only transparent addresses have public balances. Shielded funds held by this account are
          not represented here, and no amount of chain data would reveal them.
        </PanelNote>
      </Panel>
    </div>
  );
}

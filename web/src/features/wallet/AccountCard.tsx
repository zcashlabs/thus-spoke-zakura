import { ArrowDownToLine, ArrowRight, Copy } from 'lucide-react';
import { Link } from 'react-router-dom';
import { accountPath } from '@/lib/accounts';
import { shortHash } from '@/lib/format';
import type { Account } from '@/lib/api';
import { CopyButton } from '@/components/ui/CopyButton';
import { ZecAmount } from '@/components/ui/ZecAmount';
import { Button } from '@/components/ui/Button';
import { useWalletActions } from './wallet-actions-context';

export function AccountCard({ account, index = 0 }: { account: Account; index?: number }) {
  const { openFaucet, openSend } = useWalletActions();

  return (
    <article
      className="xp-window animate-rise min-w-0 rounded-xs p-4 sm:p-5"
      style={{ animationDelay: `${index * 45}ms` }}
    >
      <header className="flex min-w-0 items-center gap-2.5">
        <div className="xp-raised bg-raised text-ink-muted grid size-8 shrink-0 place-items-center rounded-xs font-mono text-[12px] font-bold">
          {account.id}
        </div>
        <h3 className="min-w-0 flex-1 truncate text-[13px] font-bold">
          <Link
            to={accountPath(account.id)}
            aria-label={`View Account ${account.id} details`}
            className="hover:underline"
          >
            {account.name}
          </Link>
        </h3>
        <CopyButton
          value={account.unified_address}
          label={`Copy ${account.name} unified address`}
          icon={<Copy className="size-4" />}
        />
      </header>

      <dl className="my-4 grid grid-cols-2 gap-3">
        <div>
          <dt className="text-accent text-[11px] font-bold tracking-[0.1em] uppercase">Ironwood</dt>
          <dd className="mt-0.5">
            <ZecAmount
              zatoshi={account.ironwood_zatoshi}
              className="text-accent-strong text-[13px] font-bold"
            />
          </dd>
        </div>
        <div className="border-line border-l pl-3">
          <dt className="text-ink-muted text-[11px] font-bold tracking-[0.1em] uppercase">
            Transparent
          </dt>
          <dd className="mt-0.5">
            <ZecAmount
              zatoshi={account.transparent_zatoshi}
              className="text-ink text-[13px] font-bold"
            />
          </dd>
        </div>
      </dl>

      <code
        className="text-ink-muted bg-canvas border-line block truncate rounded-xs border px-2.5 py-2 font-mono text-[12px]"
        title={account.unified_address}
      >
        {shortHash(account.unified_address, 17, 6)}
      </code>

      <div className="mt-4 flex min-w-0 flex-wrap gap-2">
        <Button
          variant="subtle"
          size="sm"
          className="flex-1"
          onClick={() => openFaucet(account.id)}
          aria-label={`Fund ${account.name}`}
        >
          <ArrowDownToLine />
          Fund
        </Button>
        <Button
          variant="subtle"
          size="sm"
          className="flex-1"
          onClick={() => openSend(account.id)}
          aria-label={`Send from ${account.name}`}
        >
          Send
          <ArrowRight />
        </Button>
      </div>
    </article>
  );
}

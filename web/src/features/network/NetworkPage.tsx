import { Link } from 'react-router-dom';
import { Copy } from 'lucide-react';
import { DataRow, Panel, PanelNote } from '@/components/ui/Panel';
import { CopyButton } from '@/components/ui/CopyButton';
import { Badge } from '@/components/ui/Badge';
import { ErrorState, LoadingState } from '@/components/ui/StateBlock';
import { SakuraMark } from '@/components/ui/SakuraMark';
import { useStatus } from '@/hooks/queries';
import { errorMessage } from '@/lib/api';
import type { WalletSync } from '@/lib/api/schemas';
import { cn } from '@/lib/cn';
import { Stat } from '@/components/ui/Stat';

/** Ordered as you meet them: the page you are on, then the services behind it. */
const ENDPOINT_ROWS = [
  { key: 'dashboard', label: 'Dashboard' },
  { key: 'zakura_rpc', label: 'Zakura RPC' },
  { key: 'lightwalletd', label: 'lightwalletd' },
  { key: 'p2p', label: 'P2P' },
] as const;

type IndicatorTone = 'positive' | 'warning' | 'negative' | 'neutral';

const DOT_TONE: Record<IndicatorTone, string> = {
  positive: 'bg-positive',
  warning: 'bg-warning',
  negative: 'bg-negative',
  neutral: 'bg-ink-subtle',
};

const TEXT_TONE: Record<IndicatorTone, string> = {
  positive: 'text-positive',
  warning: 'text-warning',
  negative: 'text-negative',
  neutral: 'text-ink-muted',
};

/** A labelled status dot. A lone coloured square is decoration, not
 * information, and is invisible to a screen reader. */
function StatusIndicator({ label, tone }: { label: string; tone: IndicatorTone }) {
  return (
    <div className="flex items-center gap-2">
      <span className={cn('size-2 rounded-full', DOT_TONE[tone])} aria-hidden />
      <span className={cn('text-[11px] font-bold tracking-[0.12em] uppercase', TEXT_TONE[tone])}>
        {label}
      </span>
    </div>
  );
}

const WALLET_LABEL: Record<WalletSync['state'], string> = {
  ready: 'Wallet ready',
  syncing: 'Wallet syncing',
  error: 'Wallet error',
};

const WALLET_TONE: Record<WalletSync['state'], IndicatorTone> = {
  ready: 'positive',
  syncing: 'warning',
  error: 'negative',
};

export function NetworkPage() {
  const status = useStatus();

  if (status.isPending) return <LoadingState label="Loading node status…" />;
  if (status.isError) return <ErrorState message={errorMessage(status.error)} />;

  const node = status.data.node;
  const online = node !== null;
  const endpoints = status.data.endpoints;
  // Older servers omit wallet_sync entirely; render a neutral fallback
  // instead of inventing a readiness state the server never reported.
  const walletState = status.data.wallet_sync?.state;
  const walletLabel = walletState ? WALLET_LABEL[walletState] : 'Wallet status unavailable';
  const walletTone: IndicatorTone = walletState ? WALLET_TONE[walletState] : 'neutral';

  // Node connectivity and wallet readiness are independent: a reachable node
  // can still have a syncing or failed wallet, so "operational" only holds
  // when both agree. A missing wallet_sync is treated as unknown, not ready.
  const headline = !online
    ? 'Waiting for Zakura'
    : walletState === 'ready'
      ? 'All systems operational'
      : walletState === 'syncing'
        ? 'Wallet is syncing'
        : walletState === 'error'
          ? 'Wallet synchronization failed'
          : 'Node connected';

  return (
    <div className="grid gap-4">
      <header className="flex flex-wrap items-center justify-between gap-3">
        <div className="flex items-center gap-3">
          <h1 className="text-2xl font-bold tracking-[-0.02em]">{status.data.instance}</h1>
          <Badge>{status.data.network}</Badge>
        </div>
        <div className="flex flex-wrap items-center gap-x-4 gap-y-2">
          <StatusIndicator
            label={online ? 'Online' : 'Offline'}
            tone={online ? 'positive' : 'neutral'}
          />
          <StatusIndicator label={walletLabel} tone={walletTone} />
        </div>
      </header>

      <section className="xp-window relative flex min-h-28 items-center overflow-hidden rounded-xs px-6 py-5">
        <div className="relative z-10">
          <span className="text-ink-muted block text-[11px] font-bold tracking-[0.12em] uppercase">
            Node health
          </span>
          <strong className="my-1.5 block text-xl font-bold tracking-[-0.02em]">{headline}</strong>
          <small className="text-ink-muted block text-[12px]">
            Auto-mining is {status.data.auto_mine ? 'enabled' : 'disabled'} · blocks are produced on
            demand
          </small>
        </div>
        <SakuraMark className="text-petal animate-bloom pointer-events-none absolute -right-8 -bottom-10 z-0 size-28 opacity-70" />
      </section>

      <section className="grid gap-3 sm:grid-cols-2 xl:grid-cols-4">
        <Stat label="Height" value={node ? node.blocks.toLocaleString() : '—'} tone="accent" />
        <Stat label="Accounts" value={String(status.data.account_count)} />
        <Stat
          label="Verification"
          value={node ? `${Math.round(node.verificationprogress * 100)}%` : '—'}
        />
        <Stat label="Chain" value={node?.chain ?? '—'} />
      </section>

      <Panel eyebrow="CHAIN" title="Node details">
        <dl>
          <DataRow label="Instance">{status.data.instance}</DataRow>
          <DataRow label="Network">{status.data.network}</DataRow>
          <DataRow label="Height">
            {node ? (
              <Link
                to={`/explorer/block/${node.blocks}`}
                className="text-accent-strong font-bold tabular-nums hover:underline"
              >
                {node.blocks.toLocaleString()}
              </Link>
            ) : (
              '—'
            )}
          </DataRow>
          <DataRow label="Best block">
            {node?.bestblockhash ? (
              <Link
                to={`/explorer/block/${node.bestblockhash}`}
                className="text-accent-strong font-mono hover:underline"
              >
                {node.bestblockhash}
              </Link>
            ) : (
              '—'
            )}
          </DataRow>
          <DataRow label="Development accounts">{status.data.account_count}</DataRow>
        </dl>
        <PanelNote>
          This chain is private to your machine and starts from block 0 on every run. It has no
          peers and no relationship to Zcash mainnet or testnet.
        </PanelNote>
      </Panel>

      {endpoints && (
        <Panel eyebrow="HOST NETWORK" title="Runtime endpoints">
          <dl>
            {ENDPOINT_ROWS.map(({ key, label }) => (
              <DataRow key={key} label={label}>
                <span className="flex items-center gap-2">
                  <code className="font-mono">{endpoints[key]}</code>
                  <CopyButton
                    value={endpoints[key]}
                    label={`Copy ${label} endpoint`}
                    icon={<Copy className="size-4" />}
                  />
                </span>
              </DataRow>
            ))}
          </dl>
          <PanelNote>
            Point a wallet, a light client, or your own code at these. They are published on your
            machine only, and change when you run the instance on different ports.
          </PanelNote>
        </Panel>
      )}
    </div>
  );
}

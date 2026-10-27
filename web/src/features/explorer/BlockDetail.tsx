import { Link, useParams } from 'react-router-dom';
import { ChevronLeft, ChevronRight, Copy } from 'lucide-react';
import { DataRow, Panel, PanelNote } from '@/components/ui/Panel';
import { CopyButton } from '@/components/ui/CopyButton';
import { Badge } from '@/components/ui/Badge';
import { DataTable, NumCell, Row, type Column } from '@/components/ui/DataTable';
import { ErrorState, LoadingState } from '@/components/ui/StateBlock';
import { useBlock, useStatus } from '@/hooks/queries';
import { errorMessage } from '@/lib/api';
import { chainTime } from '@/lib/format';
import { cn } from '@/lib/cn';
import { SupplyPanel } from './SupplyPanel';
import { Stat } from '@/components/ui/Stat';
import { BACK_LINK } from './back-link';

/** Prev/next controls read as buttons but must be real links. */
const NAV_LINK =
  'xp-raised xp-press bg-raised text-ink hover:bg-accent-soft inline-flex cursor-pointer items-center gap-1.5 rounded-xs px-3 py-2 text-[12px] font-medium';

interface BlockTx {
  txid?: unknown;
  size?: unknown;
  ironwood?: { actions?: unknown[] } | undefined;
  orchard?: { actions?: unknown[] } | undefined;
  vin?: unknown[];
  vout?: unknown[];
}

const TX_COLUMNS: Column[] = [
  { key: 'n', header: '#', width: '92px' },
  { key: 'txid', header: 'Transaction ID' },
  { key: 'shielded', header: 'Shielded', align: 'right', width: '104px', collapse: true },
  { key: 'size', header: 'Size', align: 'right', width: '104px', collapse: true },
];

export function BlockDetail() {
  const { id = '' } = useParams<{ id: string }>();
  const block = useBlock(id);
  const status = useStatus();

  if (block.isPending) return <LoadingState label={`Loading block ${id}…`} />;
  if (block.isError) {
    return (
      <div className="grid gap-4">
        <h1 className="text-2xl font-bold tracking-[-0.02em]">Block</h1>
        <ErrorState
          message={errorMessage(block.error)}
          action={
            <Link to="/explorer" className={BACK_LINK}>
              Back to blocks
            </Link>
          }
        />
      </div>
    );
  }

  const data = block.data;
  const tip = status.data?.node?.blocks ?? data.height;
  const txs = data.tx.filter((tx): tx is BlockTx => typeof tx === 'object' && tx !== null);

  return (
    <div className="grid gap-4">
      {/* The block is the page, so it owns the heading — and prev/next sit
          where an explorer reader reaches for them. */}
      <header className="flex flex-wrap items-center justify-between gap-3">
        <div className="flex items-center gap-3">
          <h1 className="text-2xl font-bold tracking-[-0.02em]">
            Block <span className="tabular-nums">#{data.height.toLocaleString()}</span>
          </h1>
          <Badge tone={data.confirmations > 0 ? 'positive' : 'warning'}>
            {data.confirmations.toLocaleString()} conf
          </Badge>
        </div>

        <nav className="flex items-center gap-2" aria-label="Adjacent blocks">
          {data.height > 0 && (
            <Link to={`/explorer/block/${data.height - 1}`} className={NAV_LINK}>
              <ChevronLeft className="size-4" />
              Previous
            </Link>
          )}
          {data.height < tip && (
            <Link to={`/explorer/block/${data.height + 1}`} className={NAV_LINK}>
              Next
              <ChevronRight className="size-4" />
            </Link>
          )}
        </nav>
      </header>

      <section className="grid gap-3 sm:grid-cols-2 xl:grid-cols-4">
        <Stat label="Transactions" value={String(data.nTx)} />
        <Stat label="Size" value={`${data.size.toLocaleString()} B`} />
        <Stat label="Chain time" value={chainTime(data.time)} />
        <Stat label="Difficulty" value={(data.difficulty ?? 0).toLocaleString()} />
      </section>

      {/* Transactions are what a block *is*, so they lead. */}
      <Panel eyebrow="CONTENTS" title={`${txs.length} transaction${txs.length === 1 ? '' : 's'}`}>
        <DataTable columns={TX_COLUMNS}>
          {txs.map((tx, index) => {
            const txid = typeof tx.txid === 'string' ? tx.txid : '';
            const actions =
              (tx.ironwood?.actions?.length ?? 0) + (tx.orchard?.actions?.length ?? 0);
            return (
              <Row key={txid || index} index={index} to={`/explorer/tx/${txid}`}>
                <td className="text-ink text-[11px] font-bold tracking-[0.12em] uppercase">
                  {index === 0 ? 'Coinbase' : `#${index}`}
                </td>
                <td>
                  <Link
                    to={`/explorer/tx/${txid}`}
                    className="text-accent-strong font-mono hover:underline"
                  >
                    <span className="block truncate">{txid}</span>
                  </Link>
                </td>
                <NumCell
                  className={cn(
                    'max-sm:hidden',
                    actions > 0 ? 'text-accent-strong font-semibold' : 'text-ink-muted',
                  )}
                >
                  {actions > 0 ? `${actions} actions` : '—'}
                </NumCell>
                <NumCell className="text-ink-muted max-sm:hidden">
                  {typeof tx.size === 'number' ? `${tx.size.toLocaleString()} B` : '—'}
                </NumCell>
              </Row>
            );
          })}
        </DataTable>
      </Panel>

      <SupplyPanel block={data} />

      <Panel eyebrow="DETAILS" title="Block header">
        <dl>
          <DataRow label="Hash">
            <span className="flex items-start gap-2">
              <code className="font-mono">{data.hash}</code>
              <CopyButton
                value={data.hash}
                label="Copy block hash"
                icon={<Copy className="size-4" />}
              />
            </span>
          </DataRow>
          {data.previousblockhash && (
            <DataRow label="Previous block">
              <Link
                to={`/explorer/block/${data.previousblockhash}`}
                className="text-accent-strong font-mono hover:underline"
              >
                {data.previousblockhash}
              </Link>
            </DataRow>
          )}
          {data.merkleroot && (
            <DataRow label="Merkle root">
              <code className="font-mono">{data.merkleroot}</code>
            </DataRow>
          )}
          {data.finalironwoodroot && (
            <DataRow label="Ironwood root">
              <code className="font-mono">{data.finalironwoodroot}</code>
            </DataRow>
          )}
          {/* An empty tree still has a root; show Orchard and Sapling roots only once
              their tree holds notes (`trees` leaves out empty pools). */}
          {data.finalorchardroot && (data.trees?.orchard?.size ?? 0) > 0 && (
            <DataRow label="Orchard root">
              <code className="font-mono">{data.finalorchardroot}</code>
            </DataRow>
          )}
          {data.finalsaplingroot && (data.trees?.sapling?.size ?? 0) > 0 && (
            <DataRow label="Sapling root">
              <code className="font-mono">{data.finalsaplingroot}</code>
            </DataRow>
          )}
          {data.blockcommitments && (
            <DataRow label="Block commitments">
              <code className="font-mono">{data.blockcommitments}</code>
            </DataRow>
          )}
          {data.version !== undefined && <DataRow label="Version">{data.version}</DataRow>}
        </dl>
        <PanelNote>
          Regtest block times are generated by the node, not wall-clock time, so the chain time
          above will not match when you actually mined the block.
        </PanelNote>
      </Panel>
    </div>
  );
}

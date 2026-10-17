import { useState } from 'react';
import { Link } from 'react-router-dom';
import { Button } from '@/components/ui/Button';
import { EmptyState, ErrorState } from '@/components/ui/StateBlock';
import { SkeletonRows } from '@/components/ui/Skeleton';
import { Panel } from '@/components/ui/Panel';
import { Badge } from '@/components/ui/Badge';
import { DataTable, NumCell, Row, type Column } from '@/components/ui/DataTable';
import { useBlocks } from '@/hooks/queries';
import { errorMessage } from '@/lib/api';
import { chainTimeShort } from '@/lib/format';

const COLUMNS: Column[] = [
  { key: 'height', header: 'Height', width: '92px' },
  { key: 'hash', header: 'Block hash' },
  { key: 'txs', header: 'Txs', align: 'right', width: '56px' },
  { key: 'size', header: 'Size', align: 'right', collapse: true, width: '92px' },
  { key: 'time', header: 'Time', align: 'right', collapse: true, width: '96px' },
];

export function BlockList() {
  const [before, setBefore] = useState<number | undefined>(undefined);
  const blocks = useBlocks(before);

  return (
    <Panel
      title="Blocks"
      meta={blocks.data ? <Badge>{blocks.data.blocks.length} shown</Badge> : undefined}
      actions={
        before !== undefined ? (
          <Button variant="subtle" size="sm" onClick={() => setBefore(undefined)}>
            Back to tip
          </Button>
        ) : undefined
      }
    >
      {blocks.isPending && <SkeletonRows rows={6} />}
      {blocks.isError && <ErrorState message={errorMessage(blocks.error)} />}
      {blocks.data &&
        (blocks.data.blocks.length === 0 ? (
          <EmptyState message="No blocks on this chain yet." />
        ) : (
          <>
            <DataTable columns={COLUMNS}>
              {blocks.data.blocks.map((block, index) => (
                <Row key={block.hash} index={index} to={`/explorer/block/${block.height}`}>
                  <td>
                    <Link
                      to={`/explorer/block/${block.height}`}
                      className="text-accent-strong font-mono font-bold tabular-nums hover:underline"
                    >
                      {block.height.toLocaleString()}
                    </Link>
                  </td>
                  <td>
                    <code className="text-ink truncate font-mono" title={block.hash}>
                      {block.hash}
                    </code>
                  </td>
                  <NumCell className="font-semibold">{block.nTx}</NumCell>
                  <NumCell className="text-ink-muted max-sm:hidden">
                    {block.size.toLocaleString()} B
                  </NumCell>
                  <NumCell className="text-ink-muted max-sm:hidden">
                    {chainTimeShort(block.time)}
                  </NumCell>
                </Row>
              ))}
            </DataTable>

            <footer className="border-line bg-raised flex items-center justify-between border-t px-4 py-2">
              <span className="text-ink-muted font-mono text-[11px]">
                {before === undefined
                  ? 'Chain tip'
                  : `First block #${blocks.data.blocks[0]?.height.toLocaleString() ?? before.toLocaleString()}`}
              </span>
              <Button
                variant="subtle"
                size="sm"
                disabled={blocks.data.next_before === null}
                onClick={() => setBefore(blocks.data.next_before ?? undefined)}
              >
                Older blocks
              </Button>
            </footer>
          </>
        ))}
    </Panel>
  );
}

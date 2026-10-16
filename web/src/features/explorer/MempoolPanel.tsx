import { Link } from 'react-router-dom';
import { Pickaxe } from 'lucide-react';
import { Badge } from '@/components/ui/Badge';
import { Button } from '@/components/ui/Button';
import { Panel, PanelNote } from '@/components/ui/Panel';
import { EmptyState, ErrorState, LoadingState } from '@/components/ui/StateBlock';
import { useWalletActions } from '@/features/wallet/wallet-actions-context';
import { useMempool } from '@/hooks/queries';
import { errorMessage } from '@/lib/api';

export function MempoolPanel() {
  const mempool = useMempool();
  const { openMine } = useWalletActions();
  const transactions = mempool.data?.transactions;

  return (
    <Panel
      title="Pending transactions"
      meta={
        transactions && !mempool.isError ? (
          <Badge className="max-sm:hidden">{transactions.length} pending</Badge>
        ) : undefined
      }
      actions={
        <Button
          size="sm"
          onClick={openMine}
          disabled={mempool.isPending || mempool.isError || !transactions?.length}
        >
          <Pickaxe aria-hidden />
          Mine 1 block
        </Button>
      }
    >
      {mempool.isPending ? (
        <LoadingState label="Loading pending transactions…" />
      ) : mempool.isError ? (
        <ErrorState
          message={`Unable to refresh pending transactions. ${errorMessage(mempool.error)}`}
          action={
            <Button size="sm" disabled={mempool.isFetching} onClick={() => void mempool.refetch()}>
              Retry
            </Button>
          }
        />
      ) : transactions?.length ? (
        <ul aria-label="Pending transactions" className="max-h-72 overflow-y-auto">
          {transactions.map((txid) => (
            <li key={txid} className="border-line border-b px-5 py-3 last:border-b-0">
              <Link
                to={`/explorer/tx/${txid}`}
                className="text-accent-strong block truncate font-mono text-[12px] hover:underline"
                title={txid}
              >
                {txid}
              </Link>
            </li>
          ))}
        </ul>
      ) : (
        <EmptyState message="No transactions are waiting to be mined." />
      )}
      <PanelNote>
        Transactions received by the node and waiting for a block. Updates automatically.
      </PanelNote>
    </Panel>
  );
}

import { EyeOff } from 'lucide-react';
import { Link } from 'react-router-dom';
import { Badge } from '@/components/ui/Badge';
import { DataTable, NumCell, Row, type Column } from '@/components/ui/DataTable';
import { PanelNote } from '@/components/ui/Panel';
import { EmptyState, ErrorState, LoadingState } from '@/components/ui/StateBlock';
import { useWalletTransaction } from '@/hooks/queries';
import { errorMessage, type WalletOutput, type WalletTransaction } from '@/lib/api';
import { formatZecAmount } from '@/lib/money';
import { shortHash } from '@/lib/format';
import { accountPath } from '@/lib/accounts';

// Both tables share their first two widths, so Pool sits under From.
const FIRST_WIDTH = '170px';
const SECOND_WIDTH = '160px';
const VALUE_WIDTH = '150px';

const OUTPUT_COLUMNS: Column[] = [
  { key: 'output', header: 'Output', width: FIRST_WIDTH },
  { key: 'from', header: 'From', width: SECOND_WIDTH },
  { key: 'to', header: 'To' },
  { key: 'memo', header: 'Memo' },
  { key: 'value', header: 'Value', align: 'right', width: VALUE_WIDTH },
];

const SPEND_COLUMNS: Column[] = [
  { key: 'account', header: 'Spent by', width: FIRST_WIDTH },
  { key: 'pool', header: 'Pool', width: SECOND_WIDTH },
  { key: 'notes', header: 'Notes' },
  { key: 'value', header: 'Value', align: 'right', width: VALUE_WIDTH },
];

const LINK = 'text-accent-strong font-semibold hover:underline';

const DETAILS_WARNING: Record<
  Exclude<WalletTransaction['details'], 'complete'>,
  { title: string; body: string }
> = {
  pending: {
    title: 'Memos and change are not final yet',
    body: 'The wallet found this transaction by scanning and has not fetched it in full, so a memo can be missing until it does.',
  },
  unavailable: {
    title: 'Memos and change may be missing',
    body: 'The wallet could not fetch the full transaction, so it never read the memos.',
  },
};

function AccountLink({ account }: { account: number }) {
  return (
    <Link to={accountPath(account)} className={LINK}>
      Account {account}
    </Link>
  );
}

/** The sender. A shielded sender outside accounts 1-5 never reveals an address. */
function FromCell({ from }: { from: WalletOutput['from'] }) {
  if (from === null) return <span className="text-ink-muted">External</span>;
  if (from === 'faucet') return <span className="font-semibold">Faucet</span>;
  return <AccountLink account={from} />;
}

/**
 * The recipient: a development account, or the address the wallet recorded.
 * Only transparent addresses have a public explorer page; a unified address
 * outside accounts 1-5 has no public history to link to.
 */
function ToCell({ output }: { output: WalletOutput }) {
  const change = output.change && (
    <Badge className="ml-2" tone="neutral">
      Change
    </Badge>
  );
  if (output.to !== null) {
    return (
      <span title={output.to_address ?? undefined}>
        <AccountLink account={output.to} />
        {change}
      </span>
    );
  }
  if (output.to_address === null) return <span className="text-ink-muted">External</span>;
  const label = shortHash(output.to_address, 14, 8);
  return output.to_address.startsWith('t') ? (
    <Link
      to={`/explorer/address/${output.to_address}`}
      className="text-accent-strong font-mono hover:underline"
      title={output.to_address}
    >
      {label}
    </Link>
  ) : (
    <code className="text-ink font-mono" title={output.to_address}>
      {label}
    </code>
  );
}

function PoolLabel({ pool }: { pool: string }) {
  return (
    <span className="text-ink-muted text-[11px] font-bold tracking-[0.04em] uppercase">{pool}</span>
  );
}

function MemoCell({ memo }: { memo: WalletOutput['memo'] }) {
  if (memo === null) return <span className="text-ink-muted">—</span>;
  if ('text' in memo) {
    return memo.text === '' ? (
      <span className="text-ink-muted italic">Empty text</span>
    ) : (
      <span className="text-ink wrap-anywhere">{memo.text}</span>
    );
  }
  return (
    <span className="wrap-anywhere">
      <span className="text-ink-muted mr-1.5 text-[11px]">Not text:</span>
      <code className="text-ink-muted font-mono">0x{memo.hex}</code>
    </span>
  );
}

function DetailsWarning({ details }: { details: WalletTransaction['details'] }) {
  if (details === 'complete') return null;
  const { title, body } = DETAILS_WARNING[details];
  return (
    <div className="border-warning/30 bg-warning-soft flex items-start gap-3 border-b px-5 py-4">
      <EyeOff className="text-warning mt-0.5 size-5 shrink-0" aria-hidden />
      <div>
        <b className="text-warning block text-[13px]">{title}</b>
        <p className="text-ink-muted mt-1 text-[12px] leading-relaxed">{body}</p>
      </div>
    </div>
  );
}

/** One row per account and pool, so several spent notes read as one line. */
function groupSpends(spends: WalletTransaction['spends']) {
  const groups = new Map<string, { account: number; pool: string; notes: number; value: bigint }>();
  for (const spend of spends) {
    const key = `${spend.account}:${spend.pool}`;
    const group = groups.get(key) ?? {
      account: spend.account,
      pool: spend.pool,
      notes: 0,
      value: 0n,
    };
    group.notes += 1;
    group.value += spend.value_zatoshi;
    groups.set(key, group);
  }
  return [...groups.values()];
}

/**
 * What this environment's wallet recorded about the transaction for accounts
 * 1-5. Unlike the rest of the page, this is local wallet state, not public
 * chain data, so it is read only when asked for.
 */
export function DecryptedActions({ txid, id }: { txid: string; id: string }) {
  const view = useWalletTransaction(txid);
  const data = view.data;
  const involved = data !== undefined && (data.outputs.length > 0 || data.spends.length > 0);

  return (
    <section id={id} aria-label="Decrypted actions" className="border-line border-b">
      {view.isPending && <LoadingState label="Reading the local wallet…" />}
      {view.isError && (
        <div className="p-4">
          <ErrorState message={errorMessage(view.error)} />
        </div>
      )}
      {data && <DetailsWarning details={data.details} />}
      {data && !involved && (
        <EmptyState message="No local account is involved in this transaction." />
      )}

      {data && data.outputs.length > 0 && (
        <DataTable columns={OUTPUT_COLUMNS} fixed>
          {data.outputs.map((output, index) => (
            <Row key={`${output.pool}:${output.index}`} index={index}>
              <td className="tabular-nums">
                <PoolLabel pool={output.pool} /> #{output.index}
              </td>
              <td>
                <FromCell from={output.from} />
              </td>
              <td className="min-w-0 truncate">
                <ToCell output={output} />
              </td>
              <td>
                <MemoCell memo={output.memo} />
              </td>
              <NumCell className="font-semibold">{formatZecAmount(output.value_zatoshi)}</NumCell>
            </Row>
          ))}
        </DataTable>
      )}

      {data && data.spends.length > 0 && (
        <DataTable columns={SPEND_COLUMNS} fixed>
          {groupSpends(data.spends).map((group, index) => (
            <Row key={`${group.account}:${group.pool}`} index={index}>
              <td>
                <AccountLink account={group.account} />
              </td>
              <td>
                <PoolLabel pool={group.pool} />
              </td>
              <td className="text-ink-muted tabular-nums">
                {group.notes} note{group.notes === 1 ? '' : 's'}
              </td>
              <NumCell className="font-semibold">{formatZecAmount(group.value)}</NumCell>
            </Row>
          ))}
        </DataTable>
      )}

      {data && (
        <PanelNote>
          Read from this environment's wallet, not from public chain data. A payment from outside
          accounts 1–5 appears here only after it is mined and the wallet has scanned its block.
        </PanelNote>
      )}
    </section>
  );
}

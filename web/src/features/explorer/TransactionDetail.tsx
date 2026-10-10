import { Link, useParams } from 'react-router-dom';
import { Copy, EyeOff } from 'lucide-react';
import { DataRow, Panel, PanelNote } from '@/components/ui/Panel';
import { CopyButton } from '@/components/ui/CopyButton';
import { Badge } from '@/components/ui/Badge';
import { DataTable, NumCell, Row, type Column } from '@/components/ui/DataTable';
import { ErrorState, LoadingState } from '@/components/ui/StateBlock';
import { SakuraMark } from '@/components/ui/SakuraMark';
import { useTransaction } from '@/hooks/queries';
import { errorMessage, type TxInput } from '@/lib/api';
import { formatZecAmount } from '@/lib/money';
import { chainTime, shortHash } from '@/lib/format';
import { actionSummary, summariseShielding } from './shielding';
import { resolveTransparentInput } from './resolve-input';
import { Stat } from '@/components/ui/Stat';
import { BACK_LINK } from './back-link';

const IO_COLUMNS: Column[] = [
  { key: 'n', header: '#', width: '72px' },
  { key: 'address', header: 'Address' },
  { key: 'value', header: 'Value', align: 'right', width: '160px' },
];

/**
 * A transaction can spend more than one output from the same previous
 * transaction, so the txid alone is not unique across input rows. Coinbase
 * and incomplete inputs fall back to the (fixed) input index instead.
 */
function inputKey(input: TxInput, index: number): string {
  return input.txid !== undefined && input.vout !== undefined
    ? `${input.txid}:${input.vout}`
    : String(index);
}

function PublicInputs({ vin }: { vin: TxInput[] }) {
  // The server resolves every prevout before it hands the transaction over, so
  // refetching each previous transaction here would repeat that work — and each
  // refetch triggers another round of it server-side.
  return (
    <Panel
      eyebrow="PUBLIC INPUTS"
      title={`${vin.length} transparent input${vin.length === 1 ? '' : 's'}`}
    >
      <DataTable columns={IO_COLUMNS}>
        {vin.map((input, index) => {
          const resolved = resolveTransparentInput(input);
          const label = resolved.coinbase
            ? 'Coinbase'
            : (resolved.address ?? (resolved.prevTxid ? shortHash(resolved.prevTxid) : '—'));
          const to = resolved.address
            ? `/explorer/address/${resolved.address}`
            : resolved.prevTxid
              ? `/explorer/tx/${resolved.prevTxid}`
              : undefined;
          return (
            <Row key={inputKey(input, index)} index={index} {...(to ? { to } : {})}>
              <td className="text-ink-muted tabular-nums">{index}</td>
              <td>
                {to ? (
                  <Link to={to} className="text-accent-strong font-mono hover:underline">
                    <span className="block truncate" title={resolved.prevTxid}>
                      {label}
                    </span>
                  </Link>
                ) : (
                  <span className="text-ink block truncate font-mono">{label}</span>
                )}
              </td>
              <NumCell className="font-semibold">
                {resolved.valueZat === undefined ? '—' : formatZecAmount(BigInt(resolved.valueZat))}
              </NumCell>
            </Row>
          );
        })}
      </DataTable>
    </Panel>
  );
}

export function TransactionDetail() {
  const { txid = '' } = useParams<{ txid: string }>();
  const transaction = useTransaction(txid);

  if (transaction.isPending) return <LoadingState label="Loading transaction…" />;
  if (transaction.isError) {
    return (
      <div className="grid gap-4">
        <h1 className="text-2xl font-bold tracking-[-0.02em]">Transaction</h1>
        <ErrorState
          message={errorMessage(transaction.error)}
          action={
            <Link to="/explorer" className={BACK_LINK}>
              Back to blocks
            </Link>
          }
        />
      </div>
    );
  }

  const tx = transaction.data;
  const shielding = summariseShielding(tx);
  const confirmations = tx.confirmations ?? 0;

  return (
    <div className="grid gap-4">
      <header className="flex flex-wrap items-center justify-between gap-3">
        <div className="flex items-center gap-3">
          <h1 className="text-2xl font-bold tracking-[-0.02em]">Transaction</h1>
          <code className="text-ink-muted font-mono text-[13px]">{shortHash(tx.txid, 12, 8)}</code>
          <CopyButton
            value={tx.txid}
            label="Copy transaction ID"
            icon={<Copy className="size-4" />}
          />
        </div>
        <div className="flex items-center gap-2">
          {shielding.fullyShielded && <Badge tone="accent">Fully shielded</Badge>}
          {shielding.mixed && <Badge tone="warning">Partly transparent</Badge>}
          <Badge tone={confirmations > 0 ? 'positive' : 'warning'}>
            {confirmations.toLocaleString()} conf
          </Badge>
        </div>
      </header>

      <section className="grid gap-3 sm:grid-cols-2 xl:grid-cols-4">
        <Stat
          label="Shielded actions"
          value={String(shielding.shieldedActions)}
          tone={shielding.shieldedActions > 0 ? 'accent' : 'muted'}
        />
        <Stat
          label="Block"
          value={tx.height === undefined ? 'Pending' : `#${tx.height.toLocaleString()}`}
        />
        <Stat label="Size" value={tx.size === undefined ? '—' : `${tx.size.toLocaleString()} B`} />
        <Stat label="Chain time" value={chainTime(tx.blocktime)} />
      </section>

      {/* What the transaction discloses publicly is the headline fact about a
          Zcash transaction, so it leads rather than sitting below the header. */}
      <Panel eyebrow="DISCLOSURE" title="What this transaction reveals">
        {shielding.fullyShielded && (
          <div className="border-accent-line bg-accent-soft flex items-start gap-3 border-b px-5 py-4">
            <SakuraMark className="text-accent mt-0.5 size-5 shrink-0" />
            <div>
              <b className="text-accent-strong block text-[13px]">Nothing is public</b>
              <p className="text-ink-muted mt-1 text-[12px] leading-relaxed">
                No transparent inputs, no transparent outputs, and a zero value balance. The sender,
                recipient and amount exist only inside {actionSummary(shielding)}. Your wallet can
                display this transfer because it holds the viewing keys; nobody else can.
              </p>
            </div>
          </div>
        )}

        {shielding.shieldedOnly && !shielding.fullyShielded && (
          <div className="border-accent-line bg-accent-soft flex items-start gap-3 border-b px-5 py-4">
            <SakuraMark className="text-accent mt-0.5 size-5 shrink-0" />
            <div>
              <b className="text-accent-strong block text-[13px]">Only the fee is public</b>
              <p className="text-ink-muted mt-1 text-[12px] leading-relaxed">
                Sender, recipient and amount stay inside {actionSummary(shielding)}. The pool value
                balance of {formatZecAmount(BigInt(Math.abs(shielding.valueBalanceZat)))} is public
                chain data, because the network has to see the fee to verify the transaction.
              </p>
            </div>
          </div>
        )}

        {shielding.mixed && (
          <div className="border-warning/30 bg-warning-soft flex items-start gap-3 border-b px-5 py-4">
            <EyeOff className="text-warning mt-0.5 size-5 shrink-0" aria-hidden />
            <div>
              <b className="text-warning block text-[13px]">Value crosses the shielded boundary</b>
              <p className="text-ink-muted mt-1 text-[12px] leading-relaxed">
                The transparent side below — its addresses and amounts — is public. The shielded
                side is not.
              </p>
            </div>
          </div>
        )}

        {/* Ironwood and transparent always; other pools only when the transaction uses
            them (this chain activates NU6.3 at height 1, so they're usually empty). */}
        <dl>
          <DataRow label="Ironwood actions">{shielding.ironwoodActions}</DataRow>
          {shielding.orchardActions > 0 && (
            <DataRow label="Orchard actions">{shielding.orchardActions}</DataRow>
          )}
          {shielding.saplingSpends > 0 && (
            <DataRow label="Sapling spends">{shielding.saplingSpends}</DataRow>
          )}
          {shielding.saplingOutputs > 0 && (
            <DataRow label="Sapling outputs">{shielding.saplingOutputs}</DataRow>
          )}
          <DataRow label="Transparent inputs">{shielding.transparentInputs}</DataRow>
          <DataRow label="Transparent outputs">{shielding.transparentOutputs}</DataRow>
        </dl>

        {!shielding.shieldedOnly && !shielding.mixed && (
          <PanelNote>
            Fully transparent. Every input, output, address and amount is public chain data.
          </PanelNote>
        )}
      </Panel>

      {tx.vin.length > 0 && <PublicInputs vin={tx.vin} />}

      {tx.vout.length > 0 && (
        <Panel
          eyebrow="PUBLIC OUTPUTS"
          title={`${tx.vout.length} transparent output${tx.vout.length === 1 ? '' : 's'}`}
        >
          <DataTable columns={IO_COLUMNS}>
            {tx.vout.map((out, index) => {
              const address = out.scriptPubKey?.addresses[0];
              return (
                <Row
                  key={out.n}
                  index={index}
                  {...(address ? { to: `/explorer/address/${address}` } : {})}
                >
                  <td className="text-ink-muted tabular-nums">{out.n}</td>
                  <td>
                    {address ? (
                      <Link
                        to={`/explorer/address/${address}`}
                        className="text-accent-strong font-mono hover:underline"
                      >
                        <span className="block truncate">{address}</span>
                      </Link>
                    ) : (
                      <span className="text-ink-muted block truncate font-mono">—</span>
                    )}
                  </td>
                  <NumCell className="font-semibold">
                    {formatZecAmount(BigInt(out.valueZat))}
                  </NumCell>
                </Row>
              );
            })}
          </DataTable>
        </Panel>
      )}

      <Panel eyebrow="DETAILS" title="Transaction header">
        <dl>
          <DataRow label="Transaction ID">
            <code className="font-mono">{tx.txid}</code>
          </DataRow>
          {tx.blockhash && (
            <DataRow label="Block hash">
              <Link
                to={`/explorer/block/${tx.blockhash}`}
                className="text-accent-strong font-mono hover:underline"
              >
                {tx.blockhash}
              </Link>
            </DataRow>
          )}
          {tx.version !== undefined && <DataRow label="Version">{tx.version}</DataRow>}
          {tx.versiongroupid && (
            <DataRow label="Version group">
              <code className="font-mono">{tx.versiongroupid}</code>
            </DataRow>
          )}
          {tx.expiryheight !== undefined && (
            <DataRow label="Expiry height">{tx.expiryheight.toLocaleString()}</DataRow>
          )}
          {tx.locktime !== undefined && <DataRow label="Lock time">{tx.locktime}</DataRow>}
        </dl>
      </Panel>
    </div>
  );
}

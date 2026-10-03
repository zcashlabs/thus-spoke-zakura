/**
 * Routes a query to the right view. Height, transparent address and unified
 * address are decided by shape alone. A 64-character hex string is not: block
 * hashes and transaction ids look identical, so those come back as `hash` for
 * the caller to resolve against the node.
 */
export type QueryTarget = { path: string } | { hash: string } | { error: string };

export function classifyQuery(raw: string): QueryTarget {
  const query = raw.trim();
  if (!query) return { error: 'Enter a block height, block hash, transaction ID, or address.' };
  if (/^[0-9a-fA-F]{64}$/.test(query)) return { hash: query.toLowerCase() };
  if (/^\d+$/.test(query)) return { path: `/explorer/block/${query}` };
  if (/^t[a-zA-Z0-9]{20,}$/.test(query)) return { path: `/explorer/address/${query}` };
  if (/^u(regtest)?1[a-z0-9]+$/i.test(query)) {
    return { error: 'Unified addresses are shielded and have no public chain history.' };
  }
  return { error: 'Not a recognised block height, hash, transaction ID, or transparent address.' };
}

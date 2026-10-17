import type { Status } from './api/schemas';

const BLOCKER_LABELS: Record<string, string> = {
  node_unavailable: 'Waiting for Zakura',
  lightwalletd_unavailable: 'Waiting for lightwalletd',
  wallet_error: 'Wallet synchronization failed',
  wallet_syncing: 'Wallet is syncing',
  wallet_behind: 'Wallet is catching up',
  height_mismatch: 'Services are catching up',
  chain_mismatch: 'Services disagree on the chain',
};

export function environmentReadiness(status: Status): { label: string; ready: boolean } {
  if (!status.node) return { label: 'Waiting for Zakura', ready: false };
  if (status.settled === true) return { label: 'All systems operational', ready: true };
  if (status.settled === false) {
    return {
      label: BLOCKER_LABELS[status.settlement_blocker ?? ''] ?? 'Environment is not ready',
      ready: false,
    };
  }

  // Older servers have no settlement fields. Use their wallet state without
  // claiming that a node connection alone makes the environment ready.
  switch (status.wallet_sync?.state) {
    case 'ready':
      return { label: 'All systems operational', ready: true };
    case 'syncing':
      return { label: 'Wallet is syncing', ready: false };
    case 'error':
      return { label: 'Wallet synchronization failed', ready: false };
    default:
      return { label: 'Node connected', ready: false };
  }
}

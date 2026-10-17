import { describe, expect, it, vi } from 'vitest';
import { render, screen } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import { App } from './App';

vi.mock('@/hooks/useServerEvents', () => ({ useServerEvents: () => undefined }));
vi.mock('@/hooks/queries', () => ({
  useStatus: () => ({
    data: {
      instance: 'default',
      wallet_sync: { state: 'error', last_success_at: 1700000000 },
    },
  }),
}));
vi.mock('@/features/wallet/wallet-actions-context', () => ({
  useWalletActions: () => ({ openFaucet: vi.fn(), openMine: vi.fn() }),
}));
vi.mock('@/components/layout/Shell', () => ({
  Shell: ({ children }: { children: React.ReactNode }) => children,
}));
vi.mock('@/features/network/NetworkPage', () => ({ NetworkPage: () => null }));
vi.mock('@/components/ui/ThemeToggle', () => ({ ThemeToggle: () => null }));
vi.mock('@/features/wallet/PaymentUpdates', () => ({ PaymentUpdates: () => null }));

describe('App wallet freshness', () => {
  it('shows the date as well as the time of a days-old successful update', () => {
    render(
      <MemoryRouter initialEntries={['/network']}>
        <App />
      </MemoryRouter>,
    );
    expect(screen.getByRole('alert')).toHaveTextContent('2023');
  });
});

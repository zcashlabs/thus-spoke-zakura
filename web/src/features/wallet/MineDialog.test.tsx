import { afterEach, describe, expect, it, vi } from 'vitest';
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { renderWithProviders, requestUrl } from '@/test/utils';
import type { MiningJob } from '@/lib/api';
import { MineDialog } from './MineDialog';
import { WalletActionsProvider } from './WalletActions';
import { useWalletActions } from './wallet-actions-context';
import { useQueryClient } from '@tanstack/react-query';
import { queryKeys } from '@/hooks/queries';

const running: MiningJob = {
  id: '00000000-0000-4000-8000-000000000001',
  requested_blocks: 9999,
  completed_blocks: 4966,
  state: 'mining',
  error: null,
  progress_uncertain: false,
};

afterEach(() => {
  vi.restoreAllMocks();
  sessionStorage.clear();
});

function mockJob(job: MiningJob | null) {
  return vi.spyOn(globalThis, 'fetch').mockImplementation(async (input) => {
    await Promise.resolve();
    const url = requestUrl(input);
    if (url.endsWith('/mining/jobs')) return Response.json({ job });
    if (url.endsWith('/accounts')) return Response.json([]);
    throw new Error(`Unexpected request: ${url}`);
  });
}

function OpenMine() {
  const actions = useWalletActions();
  return <button onClick={actions.openMine}>Open mine</button>;
}

function ReplaceJob() {
  const client = useQueryClient();
  return (
    <button
      onClick={() =>
        client.setQueryData(queryKeys.mining, {
          job: { ...running, id: '00000000-0000-4000-8000-000000000002' },
        })
      }
    >
      Start another job
    </button>
  );
}

describe('MineDialog', () => {
  it('dismisses a failed job card until another job starts', async () => {
    mockJob({ ...running, state: 'failed', error: 'Mining failed' });
    renderWithProviders(
      <WalletActionsProvider>
        <ReplaceJob />
      </WalletActionsProvider>,
    );
    const card = await screen.findByRole('region', { name: 'Mining job' });
    await userEvent.click(within(card).getByRole('button', { name: 'Dismiss mining job' }));
    expect(screen.queryByRole('region', { name: 'Mining job' })).not.toBeInTheDocument();
    await userEvent.click(screen.getByRole('button', { name: 'Start another job' }));
    expect(await screen.findByRole('region', { name: 'Mining job' })).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Dismiss mining job' })).not.toBeInTheDocument();
  });
  it('restores a running job on a fresh mount without starting mining and polls completion', async () => {
    let job = running;
    const fetchSpy = vi
      .spyOn(globalThis, 'fetch')
      .mockImplementation(() => Promise.resolve(Response.json({ job })));
    renderWithProviders(<MineDialog open onOpenChange={vi.fn()} />);
    expect(await screen.findByRole('progressbar')).toHaveAttribute('value', '4966');
    expect(screen.getByRole('button', { name: 'Mining…' })).toBeDisabled();
    expect(screen.getByLabelText('Number of blocks')).toBeDisabled();
    job = { ...running, state: 'completed', completed_blocks: 9999 };
    await waitFor(() => expect(screen.getByText(/Mining completed/)).toBeInTheDocument(), {
      timeout: 2500,
    });
    expect(screen.getByRole('button', { name: 'Mine blocks' })).toBeEnabled();
    expect(fetchSpy.mock.calls.every((call) => call[1]?.method !== 'POST')).toBe(true);
  });

  it('recovers progress after unmount and fresh-query remount', async () => {
    mockJob(running);
    const first = renderWithProviders(<MineDialog open onOpenChange={vi.fn()} />);
    await screen.findByRole('progressbar');
    first.unmount();
    renderWithProviders(<MineDialog open onOpenChange={vi.fn()} />);
    expect(await screen.findByRole('progressbar')).toHaveAttribute('value', '4966');
  });

  it.each(['mining', 'syncing', 'failed'] as const)(
    'keeps %s status outside the dismissed dialog and across navigation',
    async (state) => {
      mockJob({ ...running, state, error: state === 'failed' ? 'Mining failed' : null });
      const view = renderWithProviders(
        <WalletActionsProvider>
          <OpenMine />
          <p>Wallet page</p>
        </WalletActionsProvider>,
      );
      await screen.findByRole('progressbar');
      await userEvent.click(screen.getByRole('button', { name: 'Open mine' }));
      expect(within(screen.getByRole('dialog')).getByRole('progressbar')).toHaveAttribute(
        'value',
        '4966',
      );
      if (state !== 'failed') {
        expect(
          within(screen.getByRole('dialog')).getByLabelText('Number of blocks'),
        ).toBeDisabled();
        expect(
          within(screen.getByRole('dialog')).getByRole('button', {
            name: state === 'syncing' ? 'Synchronizing…' : 'Mining…',
          }),
        ).toBeDisabled();
      }
      await userEvent.keyboard('{Escape}');
      await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument());
      expect(screen.getByRole('progressbar')).toHaveAttribute('value', '4966');
      view.rerender(
        <WalletActionsProvider>
          <OpenMine />
          <p>Explorer page</p>
        </WalletActionsProvider>,
      );
      expect(screen.getByRole('progressbar')).toHaveAttribute('value', '4966');
      await userEvent.click(screen.getByRole('button', { name: 'View mining' }));
      expect(screen.getByRole('dialog')).toBeInTheDocument();
    },
  );

  it('disables starting until the initial status is known', () => {
    vi.spyOn(globalThis, 'fetch').mockImplementation(() => new Promise(() => {}));
    renderWithProviders(<MineDialog open onOpenChange={vi.fn()} />);
    expect(screen.getByRole('button', { name: 'Mine blocks' })).toBeDisabled();
    expect(screen.getByLabelText('Number of blocks')).toBeDisabled();
  });

  it('disables starting on status query failure', async () => {
    vi.spyOn(globalThis, 'fetch').mockRejectedValue(new Error('Unavailable'));
    renderWithProviders(<MineDialog open onOpenChange={vi.fn()} />);
    expect(await screen.findByRole('alert')).toHaveTextContent(/mining status/i);
    expect(screen.getByRole('button', { name: 'Mine blocks' })).toBeDisabled();
  });

  it('starts a deliberate new job after completion without closing or announcing completion', async () => {
    let job: MiningJob = { ...running, state: 'completed', completed_blocks: 9999 };
    const onOpenChange = vi.fn();
    const fetchSpy = vi.spyOn(globalThis, 'fetch').mockImplementation(async (_input, init) => {
      await Promise.resolve();
      if (init?.method === 'POST') {
        job = { ...running, requested_blocks: 1, completed_blocks: 0 };
        return Response.json(job, { status: 202 });
      }
      return Response.json({ job });
    });
    renderWithProviders(<MineDialog open onOpenChange={onOpenChange} />);
    await screen.findByRole('progressbar');
    await userEvent.click(screen.getByRole('button', { name: 'Mine blocks' }));
    await waitFor(() => expect(screen.getByRole('button', { name: 'Mining…' })).toBeDisabled());
    expect(fetchSpy.mock.calls.filter((call) => call[1]?.method === 'POST')).toHaveLength(1);
    expect(onOpenChange).not.toHaveBeenCalled();
    expect(screen.queryByText('Mined 1 block')).not.toBeInTheDocument();
  });

  it('reuses the start key after a lost response and dialog remount', async () => {
    const bodies: string[] = [];
    let job: MiningJob | null = null;
    const fetchSpy = vi.spyOn(globalThis, 'fetch').mockImplementation(async (_input, init) => {
      await Promise.resolve();
      if (init?.method === 'POST') {
        bodies.push(typeof init.body === 'string' ? init.body : '{}');
        if (bodies.length === 1) throw new TypeError('response lost');
        job = { ...running, requested_blocks: 1, completed_blocks: 0 };
        return Response.json(job, { status: 202 });
      }
      return Response.json({ job });
    });
    const first = renderWithProviders(<MineDialog open onOpenChange={vi.fn()} />);
    await waitFor(() => expect(screen.getByRole('button', { name: 'Mine blocks' })).toBeEnabled());
    await userEvent.click(screen.getByRole('button', { name: 'Mine blocks' }));
    await screen.findByText(/response lost/);
    first.unmount();
    renderWithProviders(<MineDialog open onOpenChange={vi.fn()} />);
    await waitFor(() => expect(screen.getByRole('button', { name: 'Mine blocks' })).toBeEnabled());
    await userEvent.click(screen.getByRole('button', { name: 'Mine blocks' }));
    await waitFor(() => expect(bodies).toHaveLength(2));
    const keys = bodies.map(
      (body) => (JSON.parse(body) as { idempotency_key: string }).idempotency_key,
    );
    expect(keys[0]).toBe(keys[1]);
    expect(fetchSpy).toHaveBeenCalled();
  });

  it.each(['completed', 'failed'] as const)(
    'starts a new same-count job after a lost response and %s job',
    async (state) => {
      const bodies: string[] = [];
      let job: MiningJob | null = null;
      vi.spyOn(globalThis, 'fetch').mockImplementation(async (_input, init) => {
        await Promise.resolve();
        if (init?.method === 'POST') {
          bodies.push(typeof init.body === 'string' ? init.body : '{}');
          if (bodies.length === 1) {
            job = { ...running, requested_blocks: 1, completed_blocks: 1, state };
            throw new TypeError('response lost');
          }
          const firstKey = (JSON.parse(bodies[0] ?? '{}') as { idempotency_key: string })
            .idempotency_key;
          const currentKey = (JSON.parse(bodies[1] ?? '{}') as { idempotency_key: string })
            .idempotency_key;
          if (currentKey === firstKey) return Response.json(job, { status: 202 });
          job = {
            ...running,
            id: '00000000-0000-4000-8000-000000000002',
            requested_blocks: 1,
            completed_blocks: 0,
          };
          return Response.json(job, { status: 202 });
        }
        return Response.json({ job });
      });

      const first = renderWithProviders(<MineDialog open onOpenChange={vi.fn()} />);
      await waitFor(() =>
        expect(screen.getByRole('button', { name: 'Mine blocks' })).toBeEnabled(),
      );
      await userEvent.click(screen.getByRole('button', { name: 'Mine blocks' }));
      await screen.findByText(/response lost/);
      first.unmount();

      renderWithProviders(<MineDialog open onOpenChange={vi.fn()} />);
      await screen.findByText(state === 'completed' ? /Mining completed/ : /Mining failed/);
      await userEvent.click(screen.getByRole('button', { name: 'Mine blocks' }));
      await waitFor(() => expect(bodies).toHaveLength(2));
      await screen.findByText('Mining blocks…');
      const keys = bodies.map(
        (body) => (JSON.parse(body) as { idempotency_key: string }).idempotency_key,
      );
      expect(keys[1]).not.toBe(keys[0]);
    },
  );
});

import { afterEach, describe, expect, it, vi } from 'vitest';
import { screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { App } from './App';
import { renderWithWalletActions, requestUrl } from '@/test/utils';

vi.mock('@/hooks/useServerEvents', () => ({ useServerEvents: vi.fn() }));

const initialInnerWidth = window.innerWidth;

afterEach(() => {
  vi.restoreAllMocks();
  Object.defineProperty(window, 'innerWidth', { configurable: true, value: initialInnerWidth });
});

describe('App mobile actions', () => {
  it.each([390, 640])('allows mining from the header at %i px', async (width) => {
    Object.defineProperty(window, 'innerWidth', { configurable: true, value: width });
    const startedJobs: unknown[] = [];
    vi.spyOn(globalThis, 'fetch').mockImplementation(async (input, init) => {
      await Promise.resolve();
      const url = requestUrl(input);
      if (url.endsWith('/status')) {
        return Response.json({
          instance: 'default',
          node: null,
          account_count: 5,
          auto_mine: true,
          network: 'regtest',
        });
      }
      if (url.endsWith('/accounts') || url.includes('/activity')) return Response.json([]);
      if (url.endsWith('/mining/jobs') && init?.method === 'POST') {
        const body = typeof init.body === 'string' ? init.body : '{}';
        startedJobs.push(JSON.parse(body));
        return Response.json(
          {
            id: '00000000-0000-4000-8000-000000000001',
            requested_blocks: 1,
            completed_blocks: 0,
            state: 'mining',
            error: null,
            progress_uncertain: false,
          },
          { status: 202 },
        );
      }
      if (url.endsWith('/mining/jobs')) return Response.json({ job: null });
      throw new Error(`Unexpected request: ${url}`);
    });

    renderWithWalletActions(<App />);

    const mineAction = screen.getByRole('button', { name: 'Mine' });
    expect(mineAction).not.toHaveClass('max-sm:hidden');
    await userEvent.click(mineAction);
    const dialog = await screen.findByRole('dialog', { name: 'Mine blocks' });
    await waitFor(() => expect(screen.getByRole('button', { name: 'Mine blocks' })).toBeEnabled());
    await userEvent.click(screen.getByRole('button', { name: 'Mine blocks' }));

    await waitFor(() => expect(startedJobs).toHaveLength(1));
    expect(dialog).toBeInTheDocument();
    expect(startedJobs[0]).toMatchObject({ blocks: 1 });
  });
});

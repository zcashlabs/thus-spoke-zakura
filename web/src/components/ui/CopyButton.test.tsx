import { afterEach, describe, expect, it, vi } from 'vitest';
import { screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { renderWithProviders } from '@/test/utils';
import { CopyButton } from './CopyButton';

afterEach(() => vi.restoreAllMocks());

function clipboard(writeText: ((text: string) => Promise<void>) | undefined) {
  Object.defineProperty(navigator, 'clipboard', {
    configurable: true,
    value: writeText ? { writeText } : undefined,
  });
}

function renderCopy() {
  renderWithProviders(
    <CopyButton value="uregtest1example" label="Copy address" icon={<span>Copy</span>} />,
  );
}

describe('CopyButton', () => {
  it('shows and announces a rejected clipboard write, then lets a retry succeed', async () => {
    const writeText = vi
      .fn<(text: string) => Promise<void>>()
      .mockRejectedValueOnce(new DOMException('Permission denied', 'NotAllowedError'))
      .mockResolvedValueOnce(undefined);
    const user = userEvent.setup();
    clipboard(writeText);
    renderCopy();
    const button = screen.getByRole('button', { name: 'Copy address' });

    await user.click(button);

    expect(await screen.findByText('Could not copy')).toBeInTheDocument();
    const announcement = await screen.findByRole('status');
    expect(announcement).toHaveAttribute('aria-live', 'assertive');
    await waitFor(() => expect(announcement).toHaveTextContent('Could not copy'));
    expect(screen.getAllByText(/uregtest1example/).length).toBeGreaterThan(0);
    expect(screen.queryByText('Copied')).not.toBeInTheDocument();
    expect(button).toHaveTextContent('Copy');

    await user.click(button);

    expect(writeText).toHaveBeenCalledTimes(2);
    expect(writeText).toHaveBeenLastCalledWith('uregtest1example');
    await waitFor(() => expect(button).toHaveTextContent('Copied'));
  });

  it('reports a browser without clipboard access', async () => {
    const user = userEvent.setup();
    clipboard(undefined);
    renderCopy();

    await user.click(screen.getByRole('button', { name: 'Copy address' }));

    expect(await screen.findByText('Could not copy')).toBeInTheDocument();
    expect(screen.queryByText('Copied')).not.toBeInTheDocument();
  });

  it('keeps confirming successful copies', async () => {
    const user = userEvent.setup();
    const writeText = vi.fn<(text: string) => Promise<void>>().mockResolvedValue(undefined);
    clipboard(writeText);
    renderCopy();
    const button = screen.getByRole('button', { name: 'Copy address' });

    await user.click(button);

    expect(writeText).toHaveBeenCalledWith('uregtest1example');
    await waitFor(() => expect(button).toHaveTextContent('Copied'));
    expect(screen.queryByText('Could not copy')).not.toBeInTheDocument();
  });
});

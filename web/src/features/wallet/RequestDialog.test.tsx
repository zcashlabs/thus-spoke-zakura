import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import type { SVGProps } from 'react';
import { renderWithProviders, requestUrl, testAccounts } from '@/test/utils';
import { RequestDialog } from './RequestDialog';

vi.mock('qrcode.react', () => ({
  QRCodeSVG: ({
    value,
    title,
    size,
    ...props
  }: SVGProps<SVGSVGElement> & { value: string; title?: string; size?: number }) => (
    <svg {...props} data-value={value} width={size} height={size}>
      {title && <title>{title}</title>}
    </svg>
  ),
}));

const URI = 'zcash:uregtest1account3?amount=1.23456789&memo=bWVtbw';

function mockRequest(
  implementation: typeof fetch = (input) => {
    if (!requestUrl(input).endsWith('/zip321/request')) {
      throw new Error(`Unexpected request: ${requestUrl(input)}`);
    }
    return Promise.resolve(
      new Response(JSON.stringify({ uri: URI }), {
        status: 200,
        headers: { 'content-type': 'application/json' },
      }),
    );
  },
) {
  return vi.spyOn(globalThis, 'fetch').mockImplementation(implementation);
}

async function selectAccount(name: string) {
  Object.assign(Element.prototype, {
    hasPointerCapture: () => false,
    releasePointerCapture: () => undefined,
    scrollIntoView: () => undefined,
  });
  await userEvent.click(screen.getByLabelText('Receiving account'));
  await userEvent.click(await screen.findByRole('option', { name }));
}

describe('RequestDialog', () => {
  let fetchMock: ReturnType<typeof mockRequest>;
  let writeText: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    fetchMock = mockRequest();
    writeText = vi.fn(() => Promise.resolve());
    Object.defineProperty(navigator, 'clipboard', {
      configurable: true,
      value: { writeText },
    });
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  it('uses one generated URI for the text, QR code, and copy button', async () => {
    renderWithProviders(<RequestDialog open onOpenChange={vi.fn()} accounts={testAccounts} />);
    await selectAccount('Account 3');
    await userEvent.type(screen.getByLabelText('Amount (ZEC, optional)'), '1.23456789');
    await userEvent.type(screen.getByLabelText('Memo (optional)'), 'memo');
    await userEvent.click(screen.getByRole('button', { name: 'Generate request' }));

    await waitFor(() => expect(fetchMock).toHaveBeenCalledOnce());
    const init = fetchMock.mock.calls[0]?.[1];
    expect(JSON.parse(typeof init?.body === 'string' ? init.body : '{}')).toEqual({
      account_id: 3,
      amount_zatoshi: 123_456_789,
      memo: 'memo',
    });
    const output = await screen.findByRole('region', { name: 'Generated payment request' });
    expect(output.querySelector('code')).toHaveTextContent(URI);
    const qr = screen.getByRole('img', { name: 'Payment request QR code' });
    expect(qr).toHaveAttribute('data-value', URI);
    expect(qr.querySelector('title')).toHaveTextContent(URI);

    await userEvent.click(screen.getByRole('button', { name: 'Copy payment request' }));
    expect(writeText).toHaveBeenCalledOnce();
    expect(writeText).toHaveBeenCalledWith(URI);
  });

  it('allows the amount and memo to be omitted', async () => {
    renderWithProviders(<RequestDialog open onOpenChange={vi.fn()} accounts={testAccounts} />);
    const generate = screen.getByRole('button', { name: 'Generate request' });
    await waitFor(() => expect(generate).toBeEnabled());
    await userEvent.click(generate);

    await waitFor(() => expect(fetchMock).toHaveBeenCalledOnce());
    const init = fetchMock.mock.calls[0]?.[1];
    expect(JSON.parse(typeof init?.body === 'string' ? init.body : '{}')).toEqual({
      account_id: 1,
    });
  });

  it('rejects an amount with more than eight decimal places', async () => {
    renderWithProviders(<RequestDialog open onOpenChange={vi.fn()} accounts={testAccounts} />);
    await userEvent.type(screen.getByLabelText('Amount (ZEC, optional)'), '0.000000001');

    expect(
      await screen.findByText('Enter a number with up to 8 decimal places.'),
    ).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Generate request' })).toBeDisabled();
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it('rejects an amount above the 21,000,000 ZEC supply', async () => {
    renderWithProviders(<RequestDialog open onOpenChange={vi.fn()} accounts={testAccounts} />);
    await userEvent.type(screen.getByLabelText('Amount (ZEC, optional)'), '21000001');

    expect(await screen.findByText('Amount must not exceed 21,000,000 ZEC.')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Generate request' })).toBeDisabled();
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it('never exposes a stale request while generation is pending', async () => {
    let resolveSecond: ((response: Response) => void) | undefined;
    fetchMock.mockRestore();
    fetchMock = mockRequest((_input, init) => {
      const body = JSON.parse(typeof init?.body === 'string' ? init.body : '{}') as {
        memo?: string;
      };
      if (body.memo === undefined) {
        return Promise.resolve(
          new Response(JSON.stringify({ uri: 'zcash:uregtest1account1' }), {
            status: 200,
            headers: { 'content-type': 'application/json' },
          }),
        );
      }
      return new Promise<Response>((resolve) => {
        resolveSecond = resolve;
      });
    });
    renderWithProviders(<RequestDialog open onOpenChange={vi.fn()} accounts={testAccounts} />);
    const generate = screen.getByRole('button', { name: 'Generate request' });
    await waitFor(() => expect(generate).toBeEnabled());
    await userEvent.click(generate);
    expect(
      (await screen.findByRole('region', { name: 'Generated payment request' })).querySelector(
        'code',
      ),
    ).toHaveTextContent('zcash:uregtest1account1');

    const memo = screen.getByLabelText('Memo (optional)');
    await userEvent.type(memo, 'first');
    expect(
      screen.queryByRole('region', { name: 'Generated payment request' }),
    ).not.toBeInTheDocument();
    await userEvent.click(generate);
    expect(screen.getByRole('button', { name: 'Generating…' })).toBeDisabled();
    await userEvent.type(memo, ' changed');
    resolveSecond?.(
      new Response(JSON.stringify({ uri: 'zcash:uregtest1account1?memo=stale' }), {
        status: 200,
        headers: { 'content-type': 'application/json' },
      }),
    );

    await waitFor(() =>
      expect(screen.getByRole('button', { name: 'Generate request' })).toBeEnabled(),
    );
    expect(
      screen.queryByRole('region', { name: 'Generated payment request' }),
    ).not.toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Copy payment request' })).not.toBeInTheDocument();
  });
});

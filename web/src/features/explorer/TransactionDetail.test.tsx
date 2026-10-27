import { describe, expect, it, vi, afterEach } from 'vitest';
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { MemoryRouter, Route, Routes } from 'react-router-dom';
import { renderWithProviders, requestUrl } from '@/test/utils';
import { TransactionDetail } from './TransactionDetail';

const TXID = 'd9'.repeat(32);
const PREV_TXID = 'ab'.repeat(32);
const INPUT_ADDRESS = 'tmAg2wTARgKBHA9mFouqR727cU98MJmfjSq';

function json(body: unknown): Promise<Response> {
  return Promise.resolve(
    new Response(JSON.stringify(body), {
      status: 200,
      headers: { 'content-type': 'application/json' },
    }),
  );
}

afterEach(() => {
  vi.restoreAllMocks();
});

function walletView(overrides: Record<string, unknown> = {}) {
  return {
    txid: TXID,
    status: 'confirmed',
    mined_height: 107,
    confirmations: 1,
    fee_zatoshi: null,
    details: 'complete',
    outputs: [],
    spends: [],
    ...overrides,
  };
}

function renderTx(
  body: unknown,
  extras: Record<string, unknown> = {},
  wallet: unknown = walletView(),
) {
  const fetchSpy = vi.spyOn(globalThis, 'fetch').mockImplementation((input) => {
    const url = requestUrl(input);
    if (url.endsWith('/wallet')) return json(wallet);
    if (url.includes(`/transactions/${TXID}`)) return json(body);
    for (const [id, payload] of Object.entries(extras)) {
      if (url.includes(`/transactions/${id}`)) return json(payload);
    }
    return Promise.resolve(
      new Response(JSON.stringify({ error: { message: 'unexpected', status: 404 } }), {
        status: 404,
        headers: { 'content-type': 'application/json' },
      }),
    );
  });

  renderWithProviders(
    <MemoryRouter initialEntries={[`/explorer/tx/${TXID}`]}>
      <Routes>
        <Route path="/explorer/tx/:txid" element={<TransactionDetail />} />
      </Routes>
    </MemoryRouter>,
  );
  return fetchSpy;
}

describe('TransactionDetail', () => {
  it('lists transparent inputs with address and value', async () => {
    renderTx({
      txid: TXID,
      vin: [
        {
          txid: PREV_TXID,
          vout: 0,
          valueZat: 100_000_000,
          scriptPubKey: { addresses: [INPUT_ADDRESS] },
        },
      ],
      vout: [
        {
          n: 0,
          valueZat: 90_000_000,
          scriptPubKey: { addresses: ['tmBnC1iW276Njs86Lfp7y7qwUit55wG5bDm'] },
        },
      ],
      vShieldedSpend: [],
      vShieldedOutput: [],
      orchard: { actions: [{}, {}] },
    });

    await waitFor(() => expect(screen.getByText('1 transparent input')).toBeInTheDocument());
    expect(screen.getByText(INPUT_ADDRESS)).toHaveAttribute('title', INPUT_ADDRESS);
    expect(screen.getByText('1 ZEC')).toBeInTheDocument();
  });

  it('renders two inputs spending different outputs of the same previous transaction', async () => {
    // Both rows render their content regardless of key collisions, so only a
    // console warning actually catches a regression back to keying on txid
    // alone, which is the key that collided for these two inputs.
    const consoleError = vi.spyOn(console, 'error').mockImplementation(() => {});
    const otherAddress = 'tmBnC1iW276Njs86Lfp7y7qwUit55wG5bDm';
    renderTx({
      txid: TXID,
      vin: [
        {
          txid: PREV_TXID,
          vout: 0,
          valueZat: 100_000_000,
          scriptPubKey: { addresses: [INPUT_ADDRESS] },
        },
        {
          txid: PREV_TXID,
          vout: 2,
          valueZat: 25_000_000,
          scriptPubKey: { addresses: [otherAddress] },
        },
      ],
      vout: [{ n: 0, valueZat: 120_000_000, scriptPubKey: { addresses: [] } }],
      vShieldedSpend: [],
      vShieldedOutput: [],
    });

    await waitFor(() => expect(screen.getByText('2 transparent inputs')).toBeInTheDocument());
    expect(screen.getByText(INPUT_ADDRESS)).toBeInTheDocument();
    expect(screen.getByText('1 ZEC')).toBeInTheDocument();
    expect(screen.getByText(otherAddress)).toBeInTheDocument();
    expect(screen.getByText('0.25 ZEC')).toBeInTheDocument();

    const duplicateKeyWarning = consoleError.mock.calls.some(([message]) =>
      String(message).includes('two children with the same key'),
    );
    expect(duplicateKeyWarning).toBe(false);
  });

  it('labels a coinbase input instead of inventing an address', async () => {
    renderTx({
      txid: TXID,
      vin: [{ coinbase: '03' }],
      vout: [{ n: 0, valueZat: 0, scriptPubKey: { addresses: [] } }],
      vShieldedSpend: [],
      vShieldedOutput: [],
    });

    await waitFor(() => expect(screen.getByText('1 transparent input')).toBeInTheDocument());
    expect(screen.getByText('Coinbase')).toBeInTheDocument();
  });

  it('renders the address and value the server attached to the input', async () => {
    const fetchSpy = renderTx({
      txid: TXID,
      // The server copies the spent output onto each vin before serving.
      vin: [
        {
          txid: PREV_TXID,
          vout: 0,
          valueZat: 100_000_000,
          scriptPubKey: { addresses: [INPUT_ADDRESS] },
        },
      ],
      vout: [
        {
          n: 0,
          valueZat: 90_000_000,
          scriptPubKey: { addresses: ['tmBnC1iW276Njs86Lfp7y7qwUit55wG5bDm'] },
        },
      ],
      vShieldedSpend: [],
      vShieldedOutput: [],
    });

    expect(await screen.findByText(INPUT_ADDRESS)).toBeInTheDocument();
    expect(screen.getByText('1 ZEC')).toBeInTheDocument();

    // Refetching the previous transaction would repeat the server's own
    // prevout resolution, and each refetch triggers another round of it.
    const fetched = fetchSpy.mock.calls.map(([input]) => requestUrl(input as RequestInfo));
    expect(fetched.filter((url) => url.includes(`/transactions/${PREV_TXID}`))).toEqual([]);
  });

  it('warns when previous output details are unavailable', async () => {
    renderTx({
      txid: TXID,
      transparent_inputs_complete: false,
      vin: [{ txid: PREV_TXID, vout: 0, prevout_unavailable: true }],
      vout: [{ n: 0, valueZat: 90_000_000, scriptPubKey: { addresses: [] } }],
      vShieldedSpend: [],
      vShieldedOutput: [],
    });

    expect(await screen.findByText('Some input details are unavailable')).toBeInTheDocument();
    expect(screen.getByTitle(PREV_TXID)).toHaveTextContent(PREV_TXID.slice(0, 8));
    expect(screen.getByText(/could not resolve one or more previous outputs/i)).toBeInTheDocument();
    expect(screen.getByText('Previous output details unavailable')).toBeInTheDocument();
    expect(screen.getByText('Unavailable')).toBeInTheDocument();
    expect(
      screen.getByText(
        'Fully transparent, but some public input addresses and amounts could not be retrieved.',
      ),
    ).toBeInTheDocument();
    expect(
      screen.queryByText(
        'Fully transparent. Every input, output, address and amount is public chain data.',
      ),
    ).not.toBeInTheDocument();
  });

  it('does not call a shielded transfer fully transparent when it pays a fee', async () => {
    // Splitting "shielded" into zero-balance and fee-paying cases left this
    // footer keyed off the wrong flag, so a fee-paying Orchard transfer was
    // labelled fully transparent.
    renderTx({
      txid: TXID,
      vin: [],
      vout: [],
      vShieldedSpend: [],
      vShieldedOutput: [],
      orchard: { actions: [{}, {}], valueBalanceZat: 10_000 },
    });

    expect(await screen.findByText('Pool value balances are public')).toBeInTheDocument();
    expect(screen.getByText('Fee').nextElementSibling).toHaveTextContent(
      '0.0001 ZEC (10,000 zatoshi)',
    );
    expect(screen.queryByText(/Fully transparent/i)).not.toBeInTheDocument();
  });

  it('shows an Ironwood transfer as shielded, not fully transparent', async () => {
    // After NU6.3 every shielded send is an Ironwood bundle with an empty
    // Orchard bundle; counting only Orchard labelled it fully transparent.
    renderTx({
      txid: TXID,
      version: 6,
      vin: [],
      vout: [],
      vShieldedSpend: [],
      vShieldedOutput: [],
      orchard: { actions: [], valueBalanceZat: 0 },
      ironwood: { actions: [{}, {}], valueBalanceZat: 10_000 },
    });

    expect(await screen.findByText('Pool value balances are public')).toBeInTheDocument();
    expect(screen.getByText(/stay inside 2 Ironwood actions/)).toBeInTheDocument();
    expect(screen.queryByText(/Fully transparent/i)).not.toBeInTheDocument();
    // Pools the transaction doesn't use aren't listed.
    expect(screen.getByText('Ironwood actions')).toBeInTheDocument();
    expect(screen.queryByText('Orchard actions')).not.toBeInTheDocument();
    expect(screen.queryByText(/^Sapling/)).not.toBeInTheDocument();
  });

  it('lists Orchard and Sapling only when the transaction uses them', async () => {
    // External regtest clients can still make Sapling transactions after NU6.3.
    renderTx({
      txid: TXID,
      vin: [],
      vout: [],
      vShieldedSpend: [{}],
      vShieldedOutput: [{}, {}],
      orchard: { actions: [{}], valueBalanceZat: 0 },
    });

    expect(await screen.findByText('Orchard actions')).toBeInTheDocument();
    expect(screen.getByText('Sapling spends')).toBeInTheDocument();
    expect(screen.getByText('Sapling outputs')).toBeInTheDocument();
  });

  it('says pool values are public without listing them when a cross-pool transfer nets to zero', async () => {
    renderTx({
      txid: TXID,
      vin: [],
      vout: [],
      ironwood: { actions: [{}], valueBalanceZat: 50_000_000 },
      orchard: { actions: [{}], valueBalanceZat: -50_000_000 },
    });

    expect(await screen.findByText('Pool value balances are public')).toBeInTheDocument();
    expect(screen.queryByText('Fully shielded')).not.toBeInTheDocument();
    expect(screen.queryByText('Nothing is public')).not.toBeInTheDocument();
    expect(screen.queryByText(/public value balance/)).not.toBeInTheDocument();
  });

  it('explains an unknown fee when a transparent prevout is missing', async () => {
    renderTx({
      txid: TXID,
      vin: [{ txid: PREV_TXID, vout: 0 }],
      vout: [{ n: 0, valueZat: 9_000 }],
    });
    expect(await screen.findByText('Fee')).toBeInTheDocument();
    expect(screen.getByText('Fee').nextElementSibling).toHaveTextContent('Unknown');
    expect(screen.getByText('Fee').nextElementSibling).toHaveTextContent(
      'transparent input value is unavailable',
    );
  });

  it('says coinbase has no fee', async () => {
    renderTx({ txid: TXID, vin: [{ coinbase: '03' }], vout: [{ n: 0, valueZat: 100_000_000 }] });
    expect(await screen.findByText('Fee')).toBeInTheDocument();
    expect(screen.getByText('Fee').nextElementSibling).toHaveTextContent('No fee (coinbase)');
  });
});

describe('TransactionDetail decrypted actions', () => {
  const tx = {
    txid: TXID,
    vin: [],
    vout: [],
    vShieldedSpend: [],
    vShieldedOutput: [],
    orchard: { actions: [{}, {}] },
  };

  async function decrypt() {
    await userEvent.click(await screen.findByRole('button', { name: 'Decrypt actions' }));
  }

  function walletRequests(fetchSpy: ReturnType<typeof renderTx>) {
    return fetchSpy.mock.calls
      .map(([input]) => requestUrl(input))
      .filter((url) => url.endsWith('/wallet'));
  }

  it('reads the local wallet only after Decrypt actions is pressed', async () => {
    const fetchSpy = renderTx(tx);

    expect(await screen.findByRole('button', { name: 'Decrypt actions' })).toHaveAttribute(
      'aria-expanded',
      'false',
    );
    expect(walletRequests(fetchSpy)).toHaveLength(0);

    await decrypt();
    expect(
      await screen.findByText('No local account is involved in this transaction.'),
    ).toBeInTheDocument();
    expect(walletRequests(fetchSpy)).toEqual([
      expect.stringContaining(`/api/v1/transactions/${TXID}/wallet`),
    ]);

    await userEvent.click(screen.getByRole('button', { name: 'Hide decrypted actions' }));
    expect(
      screen.queryByText('No local account is involved in this transaction.'),
    ).not.toBeInTheDocument();
  });

  it('offers no Decrypt button for a transaction without shielded actions', async () => {
    renderTx({ txid: TXID, vin: [], vout: [{ n: 0, valueZat: 1 }] });

    expect(await screen.findByText('Ironwood actions')).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Decrypt actions' })).not.toBeInTheDocument();
  });

  it('shows the outputs, memos, change and spent notes the wallet recorded', async () => {
    renderTx(
      tx,
      {},
      walletView({
        fee_zatoshi: 10_000,
        outputs: [
          {
            pool: 'ironwood',
            index: 0,
            value_zatoshi: 10_000_000,
            from: 1,
            to: 2,
            to_address: 'uregtest1accounttwoaddress000000',
            change: false,
            memo: { text: 'order 42' },
          },
          {
            pool: 'ironwood',
            index: 1,
            value_zatoshi: 189_990_000,
            from: 1,
            to: 1,
            to_address: null,
            change: true,
            memo: null,
          },
          {
            pool: 'ironwood',
            index: 2,
            value_zatoshi: 1,
            from: 'faucet',
            to: 3,
            to_address: null,
            change: false,
            memo: { hex: 'ff01' },
          },
          {
            pool: 'ironwood',
            index: 3,
            value_zatoshi: 2,
            from: 1,
            to: null,
            to_address: 'uregtest1external',
            change: false,
            memo: { text: '' },
          },
          {
            pool: 'transparent',
            index: 0,
            value_zatoshi: 3,
            from: 1,
            to: null,
            to_address: 'tmExternalTransparentAddress',
            change: false,
            memo: null,
          },
        ],
        spends: [
          { account: 1, pool: 'ironwood', value_zatoshi: 100_000_000 },
          { account: 1, pool: 'ironwood', value_zatoshi: 100_000_000 },
        ],
      }),
    );
    await decrypt();

    const decrypted = await screen.findByRole('region', { name: 'Decrypted actions' });
    await within(decrypted).findByText('order 42');
    const cells = (row: HTMLElement) => within(row).getAllByRole('cell');
    const payment = within(decrypted).getByText('order 42').closest('tr')!;
    // From and To name accounts 1-5 and link to their account pages.
    expect(within(cells(payment)[1]!).getByRole('link', { name: 'Account 1' })).toHaveAttribute(
      'href',
      '/wallet/accounts/1',
    );
    expect(within(cells(payment)[2]!).getByRole('link', { name: 'Account 2' })).toHaveAttribute(
      'href',
      '/wallet/accounts/2',
    );
    expect(within(payment).queryByText('Change')).not.toBeInTheDocument();
    const change = within(decrypted).getByText('Change').closest('tr')!;
    expect(within(cells(change)[2]!).getByRole('link', { name: 'Account 1' })).toBeInTheDocument();
    const faucet = within(decrypted).getByText('0xff01').closest('tr')!;
    expect(within(cells(faucet)[1]!).getByText('Faucet')).toBeInTheDocument();
    expect(within(faucet).getByText('Not text:')).toBeInTheDocument();
    // A unified address outside the accounts has no public page, so it is plain text.
    const external = within(decrypted).getByText('Empty text').closest('tr')!;
    expect(within(cells(external)[2]!).getByText('uregtest1external')).toBeInTheDocument();
    expect(within(cells(external)[2]!).queryByRole('link')).not.toBeInTheDocument();
    // A transparent address links to its public explorer page.
    const transparent = within(decrypted).getByText('tmExternalTran…tAddress').closest('tr')!;
    expect(
      within(transparent).getByRole('link', { name: 'tmExternalTran…tAddress' }),
    ).toHaveAttribute('href', '/explorer/address/tmExternalTransparentAddress');

    // Two notes from the same account and pool read as one row.
    const spent = within(decrypted).getByText('2 notes').closest('tr')!;
    expect(within(spent).getByRole('link', { name: 'Account 1' })).toHaveAttribute(
      'href',
      '/wallet/accounts/1',
    );
    expect(within(spent).getByText('ironwood')).toBeInTheDocument();
    expect(within(spent).getByText('2 ZEC')).toBeInTheDocument();
    // The public fee row above already shows the fee.
    expect(within(decrypted).queryByText('Fee')).not.toBeInTheDocument();
  });

  it.each([
    ['pending', 'Memos and change are not final yet'],
    ['unavailable', 'Memos and change may be missing'],
  ])('warns when details are %s', async (details, warning) => {
    renderTx(
      tx,
      {},
      walletView({
        details,
        outputs: [
          {
            pool: 'ironwood',
            index: 0,
            value_zatoshi: 10_000_000,
            from: null,
            to: 2,
            to_address: null,
            change: false,
            memo: null,
          },
        ],
      }),
    );
    await decrypt();

    expect(await screen.findByText(warning)).toBeInTheDocument();
    expect(screen.getByText('External')).toBeInTheDocument();
    expect(screen.getByRole('link', { name: 'Account 2' })).toBeInTheDocument();
  });

  it('shows a wallet read failure in place of the decrypted actions', async () => {
    vi.spyOn(globalThis, 'fetch').mockImplementation((input) =>
      requestUrl(input).endsWith('/wallet')
        ? Promise.resolve(
            new Response(JSON.stringify({ error: { message: 'wallet is busy', status: 500 } }), {
              status: 500,
              headers: { 'content-type': 'application/json' },
            }),
          )
        : json(tx),
    );
    renderWithProviders(
      <MemoryRouter initialEntries={[`/explorer/tx/${TXID}`]}>
        <Routes>
          <Route path="/explorer/tx/:txid" element={<TransactionDetail />} />
        </Routes>
      </MemoryRouter>,
    );
    await decrypt();

    // A 500's server text is replaced by the dashboard's generic message.
    expect(await screen.findByText(/The environment hit an unexpected error/)).toBeInTheDocument();
    expect(screen.getByText('What this transaction reveals')).toBeInTheDocument();
  });
});

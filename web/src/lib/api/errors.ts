/** An error carrying the HTTP status and the server's message. */
export class ApiError extends Error {
  readonly status: number;
  readonly raw: string;

  constructor(status: number, message: string) {
    super(humanise(status, message));
    this.name = 'ApiError';
    this.status = status;
    this.raw = message;
  }
}

/**
 * The server surfaces some failures as a 500 carrying a wallet-SDK string.
 * Those are accurate but unreadable, and they give the reader no idea whether
 * the situation is recoverable, so the known ones are translated here. The
 * original text stays available on `ApiError.raw` for bug reports.
 */
const TRANSLATIONS: Array<[RegExp, string]> = [
  [
    /invalid Bech32|parse error|invalid address/i,
    'That does not look like a valid transparent address for this network.',
  ],
  [
    /insufficient balance|insufficient funds|treasury exhausted/i,
    'The faucet treasury is out of funds for this environment. Try a smaller amount, or restart the environment to reset the chain.',
  ],
  [
    /note commitment tree|scan.*required|SyncRequired/i,
    'The wallet is still catching up with the chain. Give it a moment and try again.',
  ],
  [/lightwalletd rejected/i, 'The node rejected this transaction. Check the amount and try again.'],
];

function humanise(status: number, message: string): string {
  if (status >= 500) {
    for (const [pattern, replacement] of TRANSLATIONS) {
      if (pattern.test(message)) return replacement;
    }
    if (status === 503 && message.trim()) return message;
    return 'The environment hit an unexpected error. Check the service logs for details.';
  }
  return message;
}

/**
 * 400/422 reject validation or a proposal before broadcasting a new payment.
 * A retained wallet journal can still preserve the claim, so look up the key
 * before releasing it. A 409 identifies a conflicting request and is retained.
 */
export function isPaymentRejection(error: unknown): boolean {
  return error instanceof ApiError && (error.status === 400 || error.status === 422);
}

/** Narrows an unknown thrown value to a displayable message. */
export function errorMessage(error: unknown): string {
  if (error instanceof ApiError) return error.message;
  if (error instanceof Error) return error.message;
  return 'Something went wrong.';
}

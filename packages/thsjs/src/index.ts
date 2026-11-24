import { spawn } from 'node:child_process';
import type { ChildProcessByStdio } from 'node:child_process';
import type { Readable } from 'node:stream';
import { json } from 'node:stream/consumers';
import { request as httpRequest } from 'node:http';
import { randomUUID } from 'node:crypto';
import { setTimeout as delay } from 'node:timers/promises';
import { fileURLToPath } from 'node:url';

export interface TestTHSOptions {
  /** Installed THS launcher with native support; defaults to `ths` on PATH. */
  readonly executable?: string;
  readonly runtimeDir?: string;
  /** Timeout in milliseconds for startup, each operation, and shutdown. Default: 180000. */
  readonly timeoutMs?: number;
}
export interface TestTHSConfig {
  readonly instance: string;
  readonly dashboard: string;
  readonly rpc: string;
  readonly lightwalletd: string;
  readonly p2p: string;
  readonly network: 'regtest';
  readonly tls: false;
  readonly genesisHash: string;
}
export interface TestTHSFunding {
  readonly address: string;
  readonly amountZatoshi: bigint;
  readonly idempotencyKey: string;
  readonly txid: string;
  readonly status: 'broadcast' | 'confirmed' | 'unconfirmed';
}
export interface TestTHSClient {
  /** Starts a fresh isolated native instance; concurrent calls share the same result. */
  start(): Promise<TestTHSConfig>;
  config(): TestTHSConfig;
  produceBlock(count: number): Promise<void>;
  reorg(depth: number, options?: { readonly keepTransactions?: boolean }): Promise<void>;
  rollback(depth: number): Promise<void>;
  /** Sends one treasury payment; confirmations remain under the test's control. */
  fundAddress(address: string, amountZatoshi: bigint,
    options?: { readonly idempotencyKey?: string }): Promise<TestTHSFunding>;
  /** Calls the node directly; does not retry or wait for wallet sync. */
  rpc(method: string, params?: readonly unknown[]): Promise<unknown>;
  waitUntilSettled(): Promise<void>;
  /** Cancels pending work and waits for this instance's services to stop. */
  stop(): Promise<void>;
}
type ProcessResult = { code: number | null; signal: string | null };
type Run = {
  child: ChildProcessByStdio<null, Readable, Readable>;
  abort: AbortController;
  closed: Promise<ProcessResult>;
  result?: ProcessResult;
  error?: Error;
  stderr: string;
  stopping: boolean;
  signalled?: boolean;
  cleanup?: Promise<void>;
  starting: Promise<TestTHSConfig>;
  config?: TestTHSConfig;
};
function object(value: unknown): Record<string, unknown> {
  if (!value || typeof value !== 'object' || Array.isArray(value)) throw Error('Invalid THS response.');
  return value as Record<string, unknown>;
}
function integer(value: number, max: number): void {
  if (!Number.isSafeInteger(value) || value < 1 || value > max) throw Error(`Expected an integer from 1 to ${max}.`);
}
function endpoint(value: unknown): string {
  if (typeof value !== 'string' || !/^http:\/\/127\.0\.0\.1:[1-9][0-9]*\/?$/.test(value)) {
    throw Error('THS endpoints must be HTTP on 127.0.0.1 with an explicit port.');
  }
  const url = new URL(value);
  if (Number(url.port || 80) > 65535) throw Error('Invalid THS port.');
  return value.replace(/\/$/, '');
}
function endpoints(value: unknown): Omit<TestTHSConfig, 'instance' | 'genesisHash'> {
  const data = object(value);
  if (data.network !== 'regtest' || data.tls !== false) throw Error('THS must use plaintext Regtest.');
  if (typeof data.p2p !== 'string') throw Error('Invalid THS p2p endpoint.');
  endpoint(`http://${data.p2p}`);
  return { dashboard: endpoint(data.dashboard),
    rpc: endpoint(data.rpc),
    lightwalletd: endpoint(data.lightwalletd),
    p2p: data.p2p,
    network: 'regtest',
    tls: false };
}

/** Starts a fresh Regtest instance using an installed native THS runtime. Requires Node.js. */
export function createTestTHSClient(options: TestTHSOptions = {}): TestTHSClient {
  const { executable = 'ths', runtimeDir, timeoutMs = 180_000 } = options;
  integer(timeoutMs, 2147483647);
  if (typeof executable !== 'string' || !executable.length
    || (runtimeDir !== undefined && (typeof runtimeDir !== 'string' || !runtimeDir.length))) {
    throw Error('Invalid THS executable or runtime directory.');
  }
  let run: Run | undefined;

  function alive(current: Run): void {
    if (current.error) throw current.error;
    if (current.stopping || current.abort.signal.aborted) throw Error('THS startup or operation cancelled.');
    if (current.result || current.child.exitCode !== null || current.child.signalCode !== null) {
      throw Error('THS launcher exited.');
    }
  }
  function config(): TestTHSConfig {
    if (!run?.config) throw Error('THS is not ready.');
    alive(run);
    return run.config;
  }
  async function request(current: Run, url: string, signal: AbortSignal, body?: unknown): Promise<unknown> {
    alive(current);
    const payload = body === undefined ? undefined : JSON.stringify(body);
    // node:http keeps shutdown connection errors catchable, unlike affected Node fetch versions.
    const data: unknown = await new Promise((resolve, reject) => {
      const req = httpRequest(url, { signal, agent: false,
        ...(payload === undefined ? {} : { method: 'POST', headers: { 'content-type': 'application/json' } }) },
      response => {
        if (response.statusCode === undefined || response.statusCode < 200 || response.statusCode >= 300) {
          response.destroy();
          reject(Error(`THS API returned HTTP ${response.statusCode}.`));
          return;
        }
        json(response).then(resolve, reject);
      });
      req.once('error', reject);
      req.end(payload);
    });
    alive(current);
    return data;
  }
  async function status(current: Run, dashboard: string, instance: string, signal: AbortSignal): Promise<boolean> {
    const data = object(await request(current, `${dashboard}/api/v1/status`, signal));
    if (data.instance !== instance || data.network !== 'Regtest') throw Error('THS instance identity mismatch.');
    if (data.node !== null) {
      const chain = object(data.node).chain;
      if (chain !== 'test' && chain !== 'regtest') throw Error('THS node is not Regtest.');
    }
    const wallet = object(data.wallet_sync);
    if (wallet.state === 'error') throw Error('THS wallet sync failed.');
    return data.settled === true && data.node !== null && wallet.state === 'ready';
  }
  async function settle(current: Run, dashboard: string, instance: string, signal: AbortSignal): Promise<void> {
    while (!await status(current, dashboard, instance, signal)) await delay(100, undefined, { signal });
  }
  async function nodeRpc(current: Run, url: string, signal: AbortSignal,
    method: string, params: readonly unknown[]): Promise<unknown> {
    const id = randomUUID();
    const response = object(await request(current, url, signal, { jsonrpc: '2.0', id, method, params }));
    if (response.id !== id) throw Error('THS RPC response ID mismatch.');
    if (response.error != null) {
      const error = object(response.error);
      throw Error(`THS RPC failed (${String(error.code)}): ${String(error.message)}`);
    }
    if (!Object.hasOwn(response, 'result')) throw Error('THS RPC response omitted its result.');
    return response.result;
  }
  function cleanup(current: Run): Promise<void> {
    if (current.cleanup) return current.cleanup;
    current.stopping = true;
    current.abort.abort();
    current.cleanup = (async () => {
      // Signal only our live child; a saved process ID could later belong to another process.
      if (!current.result && current.child.exitCode === null && current.child.signalCode === null) {
        current.signalled = current.child.kill('SIGTERM');
      }
      let timer: ReturnType<typeof setTimeout> | undefined;
      try {
        await Promise.race([current.closed, new Promise<never>((_, reject) => {
          timer = setTimeout(() => reject(Error('THS shutdown timed out; owned-resource cleanup is unconfirmed.')),
            timeoutMs);
        })]);
      } catch (error) {
        delete current.cleanup;
        throw error;
      } finally {
        clearTimeout(timer);
      }
      if (current.result?.code !== 0 && !(current.signalled && current.result?.code === 143)
        && current.child.pid !== undefined) {
        throw current.error ?? Error(`THS launcher exited with ${current.result?.code ?? current.result?.signal}.`);
      }
    })();
    return current.cleanup;
  }
  function start(): Promise<TestTHSConfig> {
    if (run) {
      if (run.stopping) return Promise.reject(Error('THS is stopping; await stop() before restarting.'));
      try {
        alive(run);
      } catch (error) {
        return Promise.reject(error);
      }
      return run.starting;
    }
    const instance = `thsjs-${randomUUID().replaceAll('-', '')}`;
    const args = ['--name', instance, '--json', 'start',
      ...(runtimeDir === undefined ? [] : ['--runtime-dir', runtimeDir]),
      '--timeout', String(Math.ceil(timeoutMs / 1000)), '--no-open', '--no-auto-mine'];
    // Let the supervisor finish cleanup even if the test runner's process group is killed.
    const child = spawn(process.execPath,
      [fileURLToPath(new URL('./supervisor.js', import.meta.url)), executable, ...args],
      { stdio: ['ignore', 'pipe', 'pipe', 'ipc'], detached: true }) as ChildProcessByStdio<null, Readable, Readable>;
    const abort = new AbortController();
    let finish!: (value: ProcessResult) => void;
    const closed = new Promise<ProcessResult>((resolve) => {
      finish = resolve;
    });
    const current: Run = { child,
      abort,
      closed,
      stderr: '',
      stopping: false,
      starting: Promise.resolve(undefined as never) };
    run = current;
    child.stderr.setEncoding('utf8');
    child.stderr.on('data', (chunk: string) => {
      current.stderr = (current.stderr + chunk).slice(-8192);
    });
    child.once('error', (error) => {
      current.error = error;
    });
    child.once('close', (code, signal) => {
      current.result = { code, signal };
      if (!current.stopping || (code !== 0 && !(current.signalled && code === 143))) {
        current.error ??= Error(`THS launcher exited (${code ?? signal}). ${current.stderr.trim()}`);
      }
      finish(current.result);
    });
    current.starting = (async () => {
      const signal = AbortSignal.any([abort.signal, AbortSignal.timeout(timeoutMs)]);
      try {
        const data = await new Promise<unknown>((resolve, reject) => {
          let output = '';
          const release = () => {
            child.stdout.off('data', onData);
            signal.removeEventListener('abort', onAbort);
          };
          const onAbort = () => {
            release();
            reject(Error('THS startup cancelled or timed out.'));
          };
          const onData = (chunk: string) => {
            output += chunk;
            if (output.length > 65536) {
              release();
              reject(Error('THS startup output exceeded 64 KiB.'));
              return;
            }
            const newline = output.indexOf('\n');
            if (newline < 0) return;
            release();
            try {
              resolve(JSON.parse(output.slice(0, newline)));
            } catch (error) {
              reject(error);
            }
          };
          child.stdout.setEncoding('utf8');
          child.stdout.on('data', onData);
          signal.addEventListener('abort', onAbort, { once: true });
          void closed.then(() => {
            release();
            reject(current.error ?? Error('THS exited before ready.'));
          });
        });
        const values = endpoints(data);
        await settle(current, values.dashboard, instance, signal);
        // Zakura names Regtest 'test' in JSON-RPC; the exact genesis below distinguishes it from Testnet.
        const chain = object(await nodeRpc(current, values.rpc, signal, 'getblockchaininfo', [])).chain;
        if (chain !== 'test' && chain !== 'regtest') throw Error('THS RPC is not Regtest.');
        const genesisHash = await nodeRpc(current, values.rpc, signal, 'getblockhash', [0]);
        if (genesisHash !== '029f11d80ef9765602235e1bc9727e3eb6ba20839319f761fee920d63401e327') {
          throw Error('Invalid THS Regtest genesis hash.');
        }
        alive(current);
        current.config = Object.freeze({ ...values, instance, genesisHash });
        return current.config;
      } catch (error) {
        try {
          await cleanup(current);
        } catch (cleanupError) {
          throw new AggregateError([error, cleanupError],
            `THS startup failed: ${String(error)}; ${String(cleanupError)}`, { cause: cleanupError });
        } finally {
          if (current.result && run === current) run = undefined;
        }
        throw error;
      }
    })();
    return current.starting;
  }
  async function mutate(path: string, body: unknown): Promise<void> {
    const values = config();
    const current = run!;
    const signal = AbortSignal.any([current.abort.signal, AbortSignal.timeout(timeoutMs)]);
    await settle(current, values.dashboard, values.instance, signal);
    await request(current, `${values.dashboard}/api/v1/${path}`, signal, body);
    await settle(current, values.dashboard, values.instance, signal);
  }
  return {
    start,
    config,
    async produceBlock(count) {
      integer(count, 10_000);
      await mutate('mine', { blocks: count });
    },
    async reorg(depth, options = {}) {
      integer(depth, 1000);
      const { keepTransactions = false } = options;
      if (typeof keepTransactions !== 'boolean') throw Error('keepTransactions must be a boolean.');
      await mutate('reorg', { depth, keep_txs: keepTransactions });
    },
    async rollback(depth) {
      integer(depth, 1000);
      await mutate('rollback', { depth });
    },
    async fundAddress(address, amountZatoshi, options = {}) {
      if (typeof address !== 'string' || !address.trim()) throw Error('A Regtest address is required.');
      if (typeof amountZatoshi !== 'bigint' || amountZatoshi < 1n || amountZatoshi > 500_000_000n) {
        throw Error('Funding requires a bigint amount from 1 to 500000000 zatoshi (5 ZEC).');
      }
      const { idempotencyKey = randomUUID() } = options;
      if (typeof idempotencyKey !== 'string' || !/^[!-~]{8,128}$/.test(idempotencyKey)) {
        throw Error('Funding idempotency keys must contain 8-128 visible ASCII characters.');
      }
      const values = config();
      const current = run!;
      const signal = AbortSignal.any([current.abort.signal, AbortSignal.timeout(timeoutMs)]);
      try {
        await settle(current, values.dashboard, values.instance, signal);
        const url = `${values.dashboard}/api/v1/faucet/address`;
        let payment = await request(current, url, signal,
          { address, amount_zatoshi: Number(amountZatoshi), idempotency_key: idempotencyKey });
        while (true) {
          const data = object(payment);
          if (data.address !== address || data.amount_zatoshi !== Number(amountZatoshi)
            || typeof data.status !== 'string') throw Error('Invalid THS funding response.');
          if (data.status === 'failed') throw Error(`THS funding failed: ${String(data.last_error)}`);
          if (data.status === 'broadcast' || data.status === 'confirmed' || data.status === 'unconfirmed') {
            if (typeof data.txid !== 'string' || !/^[a-f0-9]{64}$/.test(data.txid)) {
              throw Error('Invalid THS funding response transaction ID.');
            }
            await status(current, values.dashboard, values.instance, signal);
            return { address, amountZatoshi, idempotencyKey, txid: data.txid, status: data.status };
          }
          await delay(100, undefined, { signal });
          await status(current, values.dashboard, values.instance, signal);
          payment = await request(current, `${url}?idempotency_key=${encodeURIComponent(idempotencyKey)}`, signal);
        }
      } catch (error) {
        throw Error(`THS funding failed (idempotency key ${idempotencyKey}): ${String(error)}`, { cause: error });
      }
    },
    async rpc(method, params = []) {
      if (typeof method !== 'string' || !method.trim() || !Array.isArray(params)) {
        throw Error('THS RPC requires a method name and an array of parameters.');
      }
      const parameters = structuredClone(params);
      const values = config();
      const current = run!;
      const signal = AbortSignal.any([current.abort.signal, AbortSignal.timeout(timeoutMs)]);
      await status(current, values.dashboard, values.instance, signal);
      return nodeRpc(current, values.rpc, signal, method, parameters);
    },
    async waitUntilSettled() {
      const values = config();
      const current = run!;
      const signal = AbortSignal.any([current.abort.signal, AbortSignal.timeout(timeoutMs)]);
      await settle(current, values.dashboard, values.instance, signal);
    },
    async stop() {
      const current = run;
      if (!current) return;
      try {
        await cleanup(current);
      } finally {
        if (current.result && run === current) run = undefined;
      }
    },
  };
}

/** Runs a test callback in a fresh native sandbox and awaits cleanup, including when the callback throws. */
export async function withTestTHS<T>(callback: (ths: TestTHSClient) => T | Promise<T>,
  options: TestTHSOptions & { readonly signal?: AbortSignal } = {}): Promise<T> {
  const { signal, ...clientOptions } = options;
  signal?.throwIfAborted();
  const ths = createTestTHSClient(clientOptions);
  let onAbort: (() => void) | undefined;
  const cancelled = signal && new Promise<never>((_, reject) => {
    onAbort = () => reject(signal.reason);
    signal.addEventListener('abort', onAbort, { once: true });
  });
  let result: { value: T } | { error: unknown };
  try {
    const work = (async () => {
      await ths.start();
      signal?.throwIfAborted();
      return callback(ths);
    })();
    // Cancellation must reach cleanup even when application code ignores the signal.
    result = { value: await (cancelled ? Promise.race([work, cancelled]) : work) };
  } catch (error) {
    result = { error };
  } finally {
    if (onAbort) signal?.removeEventListener('abort', onAbort);
  }
  try {
    await ths.stop();
  } catch (error) {
    if ('error' in result) {
      throw new AggregateError([result.error, error], 'Test and THS cleanup both failed.', { cause: error });
    }
    throw error;
  }
  if ('error' in result) throw result.error;
  return result.value;
}

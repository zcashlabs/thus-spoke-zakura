#!/usr/bin/env node
import { createServer } from 'node:http';
import { writeFileSync } from 'node:fs';
const args = process.argv.slice(2);
const value = flag => args[args.indexOf(flag) + 1];
const runtimeDir = value('--runtime-dir');
const { mode, startupMarker } = runtimeDir.startsWith('{') ? JSON.parse(runtimeDir) : { mode: runtimeDir };
const instance = value('--name');
if (mode === 'exit') { console.error('fixture service failed'); process.exit(7); }
const calls = [];
const funding = new Map();
let pending = 0, height = 200, identity = instance, stopped = false;
const server = createServer(async (req, res) => {
  let body = ''; for await (const chunk of req) body += chunk;
  res.setHeader('content-type', 'application/json');
  if (mode === 'redirect' && req.url === '/api/v1/status') {
    res.writeHead(302, { location: 'http://example.com/' }); res.end(); return;
  }
  if (mode === 'hang-http' && req.url === '/api/v1/status') return;
  if (req.url === '/fixture') {
    if (req.method === 'POST') identity = 'foreign-instance';
    res.end(JSON.stringify({ args, calls, pending, height, funded: [...funding.values()].reduce((sum, job) => sum + job.amount_zatoshi, 0) })); return;
  }
  if (req.url === '/api/v1/status') {
    res.end(JSON.stringify({ instance: mode === 'identity' ? 'foreign' : identity,
      network: mode === 'network' ? 'Mainnet' : 'Regtest', settled: mode !== 'unsettled' && pending-- <= 0,
      node: { chain: 'test', blocks: height }, wallet_sync: { state: mode === 'wallet-error' ? 'error' : 'ready' } })); return;
  }
  if (req.url === '/') {
    const request = JSON.parse(body);
    if (!['getblockhash', 'getblockchaininfo'].includes(request.method)) {
      calls.push(['rpc', request.method, request.params]);
      if (request.method === 'hang') return;
      if (request.method === 'fail') { res.end(JSON.stringify({ id: request.id, error: { code: -8, message: 'fixture RPC rejected' } })); return; }
      if (request.method === 'bad-id') { res.end(JSON.stringify({ id: 'foreign', result: 1 })); return; }
      if (request.method === 'missing-result') { res.end(JSON.stringify({ id: request.id })); return; }
      if (request.method === 'generate') { height += request.params[0]; pending = 2; }
      res.end(JSON.stringify({ id: request.id, result: height })); return;
    }
    res.end(JSON.stringify({ jsonrpc: '2.0', id: request.id, result: request.method === 'getblockhash'
      ? mode === 'genesis' ? '00'.repeat(32) : '029f11d80ef9765602235e1bc9727e3eb6ba20839319f761fee920d63401e327'
      : { chain: mode === 'rpc-network' ? 'main' : 'test' } })); return;
  }
  if (req.url?.startsWith('/api/v1/faucet/address')) {
    let key;
    if (req.method === 'POST') {
      const request = JSON.parse(body); key = request.idempotency_key;
      calls.push([req.url, request]);
      const old = funding.get(key);
      if (old && (old.address !== request.address || old.amount_zatoshi !== request.amount_zatoshi)) {
        res.writeHead(409); res.end('{}'); return;
      }
      if (!old) funding.set(key, { address: request.address, amount_zatoshi: request.amount_zatoshi, txid: '', block_hash: null, status: 'preparing', last_error: null });
      if (mode === 'lost-funding-response' && !old) { req.socket.destroy(); return; }
      res.writeHead(202);
    } else {
      key = new URL(req.url, 'http://127.0.0.1').searchParams.get('idempotency_key');
      const job = funding.get(key);
      if (!job) { res.writeHead(404); res.end('{}'); return; }
      if (mode === 'hang-funding') return;
      job.status = mode === 'failed-funding' ? 'failed' : 'broadcast';
      job.last_error = mode === 'failed-funding' ? 'fixture funding rejected' : null;
      job.txid = 'ab'.repeat(32);
      if (mode === 'wrong-funding') job.amount_zatoshi++;
      if (mode === 'bad-funding-txid') job.txid = 'invalid';
    }
    res.end(JSON.stringify(funding.get(key))); return;
  }
  calls.push([req.url, JSON.parse(body)]);
  if (mode === 'hang-mutation') return;
  pending = 2;
  if (req.url === '/api/v1/mine') height += JSON.parse(body).blocks;
  if (mode === 'api-error') { res.writeHead(500); res.end('{"error":"fixture mutation failed"}'); return; }
  res.end('{}');
});
server.listen(0, '127.0.0.1', () => {
  const port = server.address().port;
  const endpoint = `http://127.0.0.1:${port}`;
  const endpoints = { dashboard: endpoint, rpc: endpoint, lightwalletd: endpoint, p2p: `127.0.0.1:${port}`,
    network: 'regtest', tls: false };
  if (mode === 'tls') endpoints.tls = true;
  if (mode === 'url-path') endpoints.rpc += '/not-rpc';
  if (mode === 'zero-port') endpoints.rpc = 'http://127.0.0.1:0';
  if (mode === 'invalid-json') { process.stdout.write('{invalid json}\n'); return; }
  if (mode === 'oversized') { process.stdout.write('x'.repeat(65537)); return; }
  if (mode === 'public') endpoints.rpc = 'http://example.com:80';
  if (mode === 'credentials') endpoints.dashboard = `http://user:pass@127.0.0.1:${port}`;
  if (mode === 'bad-p2p') endpoints.p2p = '0.0.0.0:1234';
  if (mode === 'no-output') return;
  // Signal that cancellation is safe without declaring startup complete.
  if (startupMarker) { writeFileSync(startupMarker, 'ready'); return; }
  const data = JSON.stringify(endpoints) + '\n';
  if (mode === 'split') { process.stdout.write(data.slice(0, 30)); setTimeout(() => process.stdout.write(data.slice(30)), 30); }
  else if (mode === 'output-exit') { process.stdout.write(data, () => process.exit(6)); }
  else process.stdout.write(data);
  if (mode === 'late-term-exit') setTimeout(() => process.exit(143), 250);
  if (mode === 'late-exit') setTimeout(() => { console.error('late service failure'); process.exit(9); }, 250);
});
process.on('SIGTERM', () => {
  if (mode === 'ignore-stop' && !stopped) { stopped = true; return; }
  if (mode === 'cancel-error') console.error('Native processes stopped; failure logs retained at /fixture/logs\nError: interrupted');
  server.closeAllConnections();
  server.close(() => setTimeout(() => process.exit(mode === 'cancel-error' ? 1 : mode === 'term-code' ? 143 : 0), ['slow-stop', 'cancel-error'].includes(mode) ? 150 : 0));
});

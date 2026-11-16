import { spawn, type ChildProcess } from 'node:child_process';

// The worker's IPC connection closes even after SIGKILL; its signal handlers are not ours to replace.
let child: ChildProcess | undefined;
function stop(): void {
  if (child && child.exitCode === null && child.signalCode === null) child.kill('SIGTERM');
}
process.on('disconnect', stop);
process.on('SIGTERM', stop);
process.on('SIGINT', stop);

const [executable, ...args] = process.argv.slice(2);
if (!executable) throw Error('Missing THS executable.');
if (process.connected) {
  child = spawn(executable, args, { stdio: 'inherit' });
  child.on('error', error => { console.error(error); });
  child.once('close', (code, signal) => {
    // A signal-killed launcher has not confirmed service cleanup, even if we requested shutdown.
    if (signal) console.error(`THS launcher killed by ${signal}; owned-resource cleanup is unconfirmed.`);
    process.exitCode = code ?? 1;
    if (process.connected) process.disconnect();
  });
} else {
  process.exitCode = 1;
  if (process.connected) process.disconnect();
}

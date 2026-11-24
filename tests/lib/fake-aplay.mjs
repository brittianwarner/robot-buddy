// Stand-in for ALSA `aplay` used by tests/lib/stack.mjs. The robot runs it from PATH
// with raw PCM on stdin. It reports every stdin chunk, with its arrival time and first
// byte (the fake speech provider fills each response with its request's tag), to the
// stack over the unix socket in BENCH_APLAY_SOCK. Like aplay, it keeps "playing" what it
// buffered after stdin closes and exits only when that audio would have finished.
// A SIGKILL (the robot's barge-in) closes the socket, which the bench timestamps.
import net from 'node:net';

const now = () => performance.timeOrigin + performance.now();
const started = now();
const args = process.argv.slice(2);
const flag = (name, fallback) => {
  const i = args.indexOf(name);
  return i >= 0 && args[i + 1] ? Number(args[i + 1]) : fallback;
};
const rate = flag('-r', 24000);
const channels = flag('-c', 1);
const bytesPerMs = (rate * channels * 2) / 1000;

const socket = net.createConnection({ path: process.env.BENCH_APLAY_SOCK });
socket.on('error', () => process.exit(1));
const send = (message) => socket.write(`${JSON.stringify(message)}\n`);
send({ type: 'start', t: started, pid: process.pid, rate, channels });

let clock = 0;
let total = 0;
process.on('SIGTERM', () => {
  send({ type: 'term', t: now(), total });
  socket.end(() => process.exit(143));
});
process.stdin.on('data', (chunk) => {
  const t = now();
  total += chunk.length;
  clock = Math.max(clock, t) + chunk.length / bytesPerMs;
  send({ type: 'chunk', t, n: chunk.length, b: chunk[0] });
});
process.stdin.on('end', () => {
  send({ type: 'eof', t: now(), total });
  setTimeout(
    () => {
      send({ type: 'end', t: now(), total });
      socket.end(() => process.exit(0));
    },
    Math.max(0, clock - now()),
  );
});

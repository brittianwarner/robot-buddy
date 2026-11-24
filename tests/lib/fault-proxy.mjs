// A TCP proxy between the robot and the engine that can fail on cue. The server hands the
// robot this address as its Rivet endpoint, so every robot action (HTTP) and its event
// connection (WebSocket) pass through here; the server itself talks to the engine directly.
//
// Faults, each armed by the test:
//   holdPushes()       queue every byte the engine sends on WebSocket connections
//   holdNext('replay') queue the response to the next request whose bytes contain the
//                      marker (actions are CBOR, so a command's type is plain bytes)
//   dropNext('submit') forward the next such request, then discard its response and cut
//                      the connection: the action ran, the robot never hears the ack
//   release()          flush everything queued and stop holding
// `seen` lists each fault as it fires and each WebSocket the robot opens.
//
// Requests are matched within one TCP read plus the tail of the previous one; robot
// actions are small enough that their type arrives whole.
import net from 'node:net';

const now = () => performance.timeOrigin + performance.now();

export async function startFaultProxy({ listen, upstream }) {
  const conns = new Set();
  const seen = [];
  const state = { pushes: false, hold: null, drop: null };
  const proxy = {
    seen,
    holdPushes() {
      state.pushes = true;
    },
    holdNext(marker) {
      return new Promise((resolve) => {
        state.hold = { marker: Buffer.from(marker), resolve };
      });
    },
    dropNext(marker) {
      return new Promise((resolve) => {
        state.drop = { marker: Buffer.from(marker), resolve };
      });
    },
    release() {
      state.pushes = false;
      state.hold = null;
      for (const c of conns) c.flush();
    },
    close() {
      for (const c of conns) c.destroy();
      server.close();
    },
  };

  const server = net.createServer((client) => {
    const up = net.createConnection({ host: '127.0.0.1', port: upstream });
    const conn = { ws: null, held: false, queue: [], drop: false, tail: Buffer.alloc(0) };
    conn.flush = () => {
      conn.held = false;
      for (const chunk of conn.queue.splice(0)) if (!client.destroyed) client.write(chunk);
    };
    conn.destroy = () => {
      client.destroy();
      up.destroy();
    };
    conns.add(conn);
    const done = () => {
      conns.delete(conn);
      client.destroy();
      up.destroy();
    };
    client.on('close', done);
    up.on('close', done);
    client.on('error', done);
    up.on('error', done);

    client.on('data', (chunk) => {
      if (conn.ws === null) {
        conn.ws = /upgrade:\s*websocket/i.test(chunk.toString('latin1'));
        if (conn.ws) seen.push({ t: now(), fault: null, event: 'websocket' });
      }
      if (!conn.ws) {
        const window = Buffer.concat([conn.tail, chunk]);
        conn.tail = chunk.subarray(Math.max(0, chunk.length - 64));
        if (state.hold && window.includes(state.hold.marker)) {
          conn.held = true;
          seen.push({ t: now(), fault: 'hold', marker: state.hold.marker.toString() });
          state.hold.resolve(now());
          state.hold = null;
        } else if (state.drop && window.includes(state.drop.marker)) {
          conn.drop = true;
          seen.push({ t: now(), fault: 'drop', marker: state.drop.marker.toString() });
          state.drop.resolve(now());
          state.drop = null;
        }
      }
      up.write(chunk);
    });
    up.on('data', (chunk) => {
      if (conn.drop) {
        conn.destroy();
        return;
      }
      if (conn.held || (conn.ws && state.pushes)) {
        conn.queue.push(chunk);
        return;
      }
      client.write(chunk);
    });
  });
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(listen, '127.0.0.1', resolve);
  });
  return proxy;
}

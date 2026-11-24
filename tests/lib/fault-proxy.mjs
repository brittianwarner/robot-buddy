// A TCP proxy between the robot and the engine that can fail on cue. The server hands the
// robot this address as its Rivet endpoint, so the robot's actor connection (one
// WebSocket, which carries its conversation actions and pushed events) and its bot
// actions (HTTP) pass through here; the server itself talks to the engine directly.
//
// Faults, each armed by the test:
//   holdPushes()       queue every byte the engine sends on WebSocket connections (pushed
//                      events and action responses alike: a silent, half-open socket)
//   holdNext('replay') queue everything the engine sends on the connection of the next
//                      request whose bytes contain the marker (actions are CBOR, so a
//                      command's type is plain bytes)
//   dropNext('submit') forward the next such request, then cut the connection at the next
//                      byte the engine sends back: the action ran, the robot never hears
//                      the ack
//   release()          flush everything queued and stop holding
// `seen` lists each fault as it fires and each WebSocket the robot opens.
//
// HTTP requests are matched within one TCP read plus the tail of the previous one. On a
// WebSocket each client frame is unmasked and matched whole; robot actions fit one frame.
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
    const conn = { ws: null, held: false, queue: [], drop: false, tail: Buffer.alloc(0), frames: null };
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

    const match = (bytes) => {
      if (state.hold && bytes.includes(state.hold.marker)) {
        conn.held = true;
        seen.push({ t: now(), fault: 'hold', marker: state.hold.marker.toString() });
        state.hold.resolve(now());
        state.hold = null;
      } else if (state.drop && bytes.includes(state.drop.marker)) {
        conn.drop = true;
        seen.push({ t: now(), fault: 'drop', marker: state.drop.marker.toString() });
        state.drop.resolve(now());
        state.drop = null;
      }
    };
    client.on('data', (chunk) => {
      if (conn.ws === null) {
        const text = chunk.toString('latin1');
        conn.ws = /upgrade:\s*websocket/i.test(text);
        if (conn.ws) {
          seen.push({ t: now(), fault: null, event: 'websocket' });
          const end = text.indexOf('\r\n\r\n');
          conn.frames = end >= 0 ? chunk.subarray(end + 4) : Buffer.alloc(0);
          for (const payload of clientFrames(conn)) match(payload);
        }
      } else if (conn.ws) {
        conn.frames = Buffer.concat([conn.frames, chunk]);
        for (const payload of clientFrames(conn)) match(payload);
      }
      if (!conn.ws) {
        match(Buffer.concat([conn.tail, chunk]));
        conn.tail = chunk.subarray(Math.max(0, chunk.length - 64));
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
  /** Take each whole client frame off `conn.frames`, returning the unmasked payloads. */
  function* clientFrames(conn) {
    for (;;) {
      const b = conn.frames;
      if (b.length < 2) return;
      let len = b[1] & 0x7f;
      let at = 2;
      if (len === 126) {
        if (b.length < 4) return;
        len = b.readUInt16BE(2);
        at = 4;
      } else if (len === 127) {
        if (b.length < 10) return;
        len = Number(b.readBigUInt64BE(2));
        at = 10;
      }
      const masked = (b[1] & 0x80) !== 0;
      const key = masked ? b.subarray(at, at + 4) : null;
      if (masked) at += 4;
      if (b.length < at + len) return;
      const payload = Buffer.from(b.subarray(at, at + len));
      if (key) for (let i = 0; i < payload.length; i++) payload[i] ^= key[i & 3];
      conn.frames = b.subarray(at + len);
      yield payload;
    }
  }

  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(listen, '127.0.0.1', resolve);
  });
  return proxy;
}

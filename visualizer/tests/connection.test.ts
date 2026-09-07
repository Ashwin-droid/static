import { test } from 'node:test';
import assert from 'node:assert/strict';
import { connectStatic, type ConnectionStatus, type SocketLike } from '../src/connection.ts';

class MockSocket implements SocketLike {
  readyState = 0;
  bufferedAmount = 0;
  onopen: SocketLike['onopen'] = null;
  onmessage: SocketLike['onmessage'] = null;
  onclose: SocketLike['onclose'] = null;
  onerror: SocketLike['onerror'] = null;
  sent: string[] = [];
  closed = false;
  send(data: string) { this.sent.push(data); }
  close() { this.closed = true; this.readyState = 3; this.onclose?.(); }
  open() { this.readyState = 1; this.onopen?.(); }
}
function fixture() {
  const sockets: MockSocket[] = [];
  const timers = new Map<number, () => void>();
  let counter = 0;
  const statuses: ConnectionStatus[] = [];
  const received: unknown[] = [];
  const client = connectStatic({
    url: 'wss://127.0.0.1:1234/ws?token=secret', bands: () => 30,
    onStatus: status => statuses.push(status), onMessage: value => received.push(value),
    socket: () => { const s = new MockSocket(); sockets.push(s); return s; },
    schedule: cb => { timers.set(++counter, cb); return counter as unknown as ReturnType<typeof setTimeout>; },
    cancel: id => { timers.delete(id as unknown as number); },
  });
  const nextTimer = () => { const [id, cb] = timers.entries().next().value!; timers.delete(id); cb(); };
  return { sockets, timers, statuses, received, client, nextTimer };
}

test('configures on open, parses messages and applies send backpressure', () => {
  const f = fixture(), socket = f.sockets[0];
  assert.equal(f.client.send({ type: 'transport', action: 'toggle' }), false);
  socket.open();
  assert.deepEqual(JSON.parse(socket.sent[0]), { type: 'configure', bands: 30 });
  socket.onmessage?.({ data: 'invalid' });
  socket.onmessage?.({ data: JSON.stringify({ type: 'error', message: 'No song selected' }) });
  assert.equal(f.received.length, 1);
  socket.bufferedAmount = 256_001;
  assert.equal(f.client.send({ type: 'transport', action: 'next' }), false);
  f.client.dispose();
});

test('disconnect detaches handlers and disposal cancels retry without leaking sockets', () => {
  const f = fixture(), socket = f.sockets[0];
  socket.open();
  socket.close();
  assert.equal(socket.onmessage, null);
  assert.equal(socket.onopen, null);
  assert.equal(f.timers.size, 1);
  f.client.dispose();
  assert.equal(f.timers.size, 0);
  assert.equal(f.client.send({ type: 'configure', bands: 8 }), false);
});

test('a replacement socket resends config and stops after bounded failed retries', () => {
  const f = fixture();
  for (let i = 0; i < 9; i++) {
    f.sockets[i].onerror?.();
    if (i < 8) f.nextTimer();
  }
  assert.equal(f.sockets.length, 9);
  assert.equal(f.statuses.at(-1), 'unavailable');
  assert.equal(f.timers.size, 0);
  assert.ok(f.sockets.every(socket => socket.closed && socket.onclose === null));
  f.client.dispose();
});

test('connection timeout closes stalled socket and schedules reconnect', () => {
  const f = fixture();
  f.nextTimer();
  assert.equal(f.sockets[0].closed, true);
  assert.equal(f.statuses.at(-1), 'disconnected');
  assert.equal(f.timers.size, 1);
  f.nextTimer();
  assert.equal(f.sockets.length, 2);
  f.client.dispose();
  assert.equal(f.timers.size, 0);
});

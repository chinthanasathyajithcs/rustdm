import assert from 'node:assert/strict';
import test from 'node:test';
import { HOST_NAME, describeError, openSession, request } from '../lib/native.js';

test('a host reply passes through, and the host name and message are sent', async () => {
  const calls = [];
  const reply = { id: 2, ok: true, result: { download_id: 'dl_5' } };
  const result = await request({ type: 'ping', id: 2 }, {
    send: async (host, message) => {
      calls.push([host, message]);
      return reply;
    },
  });
  assert.deepEqual(result, reply);
  assert.deepEqual(calls, [[HOST_NAME, { type: 'ping', id: 2 }]]);
});

test('a refusal from the host passes through unchanged', async () => {
  const refusal = { id: 2, ok: false, error: { kind: 'duplicate', code: -32005, message: 'dup' } };
  assert.deepEqual(await request({}, { send: async () => refusal }), refusal);
});

test('a host that never answers times out', async () => {
  const start = Date.now();
  const result = await request({}, { send: () => new Promise(() => {}), timeoutMs: 30 });
  assert.equal(result.ok, false);
  assert.equal(result.error.kind, 'timeout');
  assert.ok(Date.now() - start < 2000);
});

test('a missing host is told apart from other failures', async () => {
  const missing = await request({}, {
    send: async () => {
      throw new Error('Specified native messaging host not found.');
    },
  });
  assert.equal(missing.error.kind, 'host_missing');

  const forbidden = await request({}, {
    send: async () => {
      throw new Error('Access to the specified native messaging host is forbidden.');
    },
  });
  assert.equal(forbidden.error.kind, 'host_forbidden');

  const exited = await request({}, {
    send: async () => {
      throw new Error('Native host has exited.');
    },
  });
  assert.equal(exited.error.kind, 'host_error');
});

test('an empty reply is reported, not treated as success', async () => {
  const result = await request({}, { send: async () => undefined });
  assert.deepEqual(result.ok, false);
  assert.equal(result.error.kind, 'no_reply');
});

test('errors explain how to fix them', () => {
  assert.match(describeError({ kind: 'host_missing' }), /rdm browser install/);
  assert.match(describeError({ kind: 'not_running', message: 'x' }), /rdm daemon start/);
  assert.match(describeError({ kind: 'rejected', message: 'disk full' }), /disk full/);
  assert.equal(describeError({ kind: 'odd', message: 'plain' }), 'plain');
  assert.ok(describeError(undefined).length > 0);
});

// A fake native port: records what is posted and lets the test answer.
function fakePort() {
  const port = {
    posted: [],
    disconnected: false,
    messageListeners: [],
    disconnectListeners: [],
    postMessage: (message) => {
      if (port.disconnected) throw new Error('Attempting to use a disconnected port object');
      port.posted.push(message);
    },
    onMessage: { addListener: (fn) => port.messageListeners.push(fn) },
    onDisconnect: { addListener: (fn) => port.disconnectListeners.push(fn) },
    disconnect: () => {
      port.disconnected = true;
    },
    reply: (message) => port.messageListeners.forEach((fn) => fn(message)),
    drop: () => {
      port.disconnected = true;
      port.disconnectListeners.forEach((fn) => fn(port));
    },
  };
  return port;
}

function session(options = {}) {
  const port = fakePort();
  const hosts = [];
  const s = openSession({
    connect: (host) => {
      hosts.push(host);
      return port;
    },
    lastError: () => options.lastError,
    timeoutMs: options.timeoutMs,
  });
  return { port, hosts, s };
}

test('a session numbers messages and matches replies', async () => {
  const { port, hosts, s } = session();
  const first = s.request({ type: 'status', download_id: 'a' });
  const second = s.request({ type: 'status', download_id: 'b' });
  assert.deepEqual(hosts, [HOST_NAME]);
  assert.deepEqual(port.posted.map((m) => m.id), [1, 2]);
  port.reply({ id: 2, ok: true, result: { download_id: 'b' } });
  port.reply({ id: 1, ok: true, result: { download_id: 'a' } });
  assert.equal((await first).result.download_id, 'a');
  assert.equal((await second).result.download_id, 'b');
});

test('a reply with an unknown id is ignored', async () => {
  const { port, s } = session();
  const pending = s.request({ type: 'status' });
  port.reply({ id: 99, ok: true, result: {} });
  port.reply('not an object');
  port.reply({ id: 1, ok: true, result: { n: 1 } });
  assert.equal((await pending).result.n, 1);
});

test('a reply without ok becomes no_reply', async () => {
  const { port, s } = session();
  const pending = s.request({ type: 'status' });
  port.reply({ id: 1 });
  assert.equal((await pending).error.kind, 'no_reply');
});

test('a disconnect fails waiting requests and calls onLost once', async () => {
  const { port, s } = session({ lastError: { message: 'Specified native messaging host not found.' } });
  let lost = 0;
  s.onLost(() => lost++);
  const pending = s.request({ type: 'add' });
  port.drop();
  port.drop();
  const reply = await pending;
  assert.equal(reply.ok, false);
  assert.equal(reply.error.kind, 'host_missing');
  assert.equal(lost, 1);
});

test('close does not call onLost', async () => {
  const { port, s } = session();
  let lost = 0;
  s.onLost(() => lost++);
  const pending = s.request({ type: 'add' });
  s.close();
  port.drop();
  assert.equal(port.disconnected, true);
  assert.equal(lost, 0);
  assert.equal((await pending).ok, false);
});

test('a session request times out', async () => {
  const { s } = session({ timeoutMs: 10 });
  const reply = await s.request({ type: 'status' });
  assert.equal(reply.error.kind, 'timeout');
});

test('a request after a disconnect fails at once', async () => {
  const { port, s } = session();
  port.drop();
  const reply = await s.request({ type: 'status' });
  assert.equal(reply.ok, false);
  assert.equal(reply.error.kind, 'host_error');
});

test('a connect that throws gives a failing session', async () => {
  const s = openSession({
    connect: () => {
      throw new Error('Access to the specified native messaging host is forbidden.');
    },
    lastError: () => undefined,
  });
  assert.equal((await s.request({ type: 'status' })).error.kind, 'host_forbidden');
});

test('not_found reads as already in the list', () => {
  assert.equal(describeError({ kind: 'not_found', message: 'x' }), "Already in rustdm's list.");
});

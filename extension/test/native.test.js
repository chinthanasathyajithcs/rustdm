import assert from 'node:assert/strict';
import test from 'node:test';
import { HOST_NAME, describeError, request } from '../lib/native.js';

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

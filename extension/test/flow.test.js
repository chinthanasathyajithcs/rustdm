import assert from 'node:assert/strict';
import test from 'node:test';
import { CHOOSING_LIMIT_MS, LOOKUP_LIMIT_MS, POLL_MS, flowStep, startFlow } from '../lib/flow.js';

const T0 = 1_000_000;
const ok = (result) => ({ ok: true, result });
const fail = (kind, extra = {}) => ({ ok: false, error: { kind, message: 'x', ...extra } });
const choices = [
  { id: '1080-mp4', label: '1080p', container: 'MP4', height: 1080, size_bytes: 100, size_is_estimate: false },
  { id: '720-mp4', label: '720p', container: 'MP4', height: 720, size_bytes: 50, size_is_estimate: false },
];
const choosing = { download_id: 'dl_1', status: 'choosing', filename: 'x', video: { title: 'Angel One', duration_secs: 60, choices, default: 1 } };

// Runs events in order; returns the last state and every effect.
function run(kind, events) {
  let { state, effects } = startFlow(kind, T0);
  const all = [...effects];
  for (const event of events) {
    const next = flowStep(state, event);
    state = next.state;
    all.push(...next.effects);
  }
  return { state, effects: all, views: all.filter((e) => e.type === 'view').map((e) => e.view) };
}

test('the flow starts with add and looking', () => {
  assert.equal(POLL_MS, 500);
  assert.equal(LOOKUP_LIMIT_MS, 60_000);
  assert.deepEqual(startFlow('page', T0).effects, [{ type: 'add' }, { type: 'view', view: { phase: 'looking' } }]);
});

test('add, then lookup, then the list, then chosen', () => {
  const { effects, views, state } = run('page', [
    { type: 'add_reply', reply: ok({ download_id: 'dl_1' }), now: T0 + 10 },
    { type: 'status_reply', reply: ok({ download_id: 'dl_1', status: 'active', filename: 'watch' }), now: T0 + 20 },
    { type: 'tick', now: T0 + 520 },
    { type: 'status_reply', reply: ok(choosing), now: T0 + 530 },
    { type: 'choose', choiceId: '720-mp4' },
    { type: 'choose_reply', reply: ok({ download_id: 'dl_1', status: 'queued', filename: 'Angel One.mp4', video: { title: 'Angel One', choices: [], chosen: { label: '720p', container: 'MP4' } } }), now: T0 + 600 },
  ]);
  assert.deepEqual(effects.filter((e) => e.type === 'status'), [
    { type: 'status', downloadId: 'dl_1' },
    { type: 'status', downloadId: 'dl_1' },
  ]);
  assert.deepEqual(effects.find((e) => e.type === 'choose'), { type: 'choose', downloadId: 'dl_1', choiceId: '720-mp4' });
  assert.deepEqual(views, [
    { phase: 'looking' },
    { phase: 'choosing', durationSecs: 60, choices, defaultIndex: 1 },
    { phase: 'started', filename: 'Angel One.mp4' },
  ]);
  assert.equal(effects.at(-1).type, 'stop');
  assert.equal(state.phase, 'started');
});

test('a plain file is started at once', () => {
  const { views } = run('file', [
    { type: 'add_reply', reply: ok({ download_id: 'dl_2' }), now: T0 },
    { type: 'status_reply', reply: ok({ download_id: 'dl_2', status: 'queued', filename: 'movie.mp4' }), now: T0 },
  ]);
  assert.deepEqual(views.at(-1), { phase: 'started', filename: 'movie.mp4' });
});

test('a page that turns out to be a file starts when bytes arrive', () => {
  const still = run('page', [
    { type: 'add_reply', reply: ok({ download_id: 'dl_3' }), now: T0 },
    { type: 'status_reply', reply: ok({ status: 'active', filename: 'f.zip', progress: 0 }), now: T0 },
  ]);
  assert.equal(still.state.phase, 'looking');
  const moving = run('page', [
    { type: 'add_reply', reply: ok({ download_id: 'dl_3' }), now: T0 },
    { type: 'status_reply', reply: ok({ status: 'active', filename: 'f.zip', progress: 0.1 }), now: T0 },
  ]);
  assert.equal(moving.state.phase, 'started');
});

test('a stream with a chosen quality is started', () => {
  const { state } = run('stream', [
    { type: 'add_reply', reply: ok({ download_id: 'dl_4' }), now: T0 },
    { type: 'status_reply', reply: ok({ status: 'active', filename: 'hls.mp4', video: { title: 'hls', choices: [] } }), now: T0 },
  ]);
  assert.equal(state.phase, 'started');
});

test('paused, retrying and completed count as started', () => {
  for (const status of ['paused', 'pausing', 'retrying', 'completed']) {
    const { state } = run('page', [
      { type: 'add_reply', reply: ok({ download_id: 'dl_5' }), now: T0 },
      { type: 'status_reply', reply: ok({ status, filename: 'a' }), now: T0 },
    ]);
    assert.equal(state.phase, 'started', status);
  }
});

test('a failed download shows its error', () => {
  const { views, effects } = run('page', [
    { type: 'add_reply', reply: ok({ download_id: 'dl_6' }), now: T0 },
    { type: 'status_reply', reply: ok({ status: 'failed', filename: 'a', error: 'No video found on this page.' }), now: T0 },
  ]);
  assert.deepEqual(views.at(-1), { phase: 'failed', message: 'No video found on this page.' });
  assert.equal(effects.at(-1).type, 'stop');
});

test('a failed download without a message still says something', () => {
  const { views } = run('page', [
    { type: 'add_reply', reply: ok({ download_id: 'dl_6' }), now: T0 },
    { type: 'status_reply', reply: ok({ status: 'failed', filename: 'a' }), now: T0 },
  ]);
  assert.equal(views.at(-1).phase, 'failed');
  assert.ok(views.at(-1).message.length > 0);
});

test('cancelled in rustdm', () => {
  const { views } = run('page', [
    { type: 'add_reply', reply: ok({ download_id: 'dl_7' }), now: T0 },
    { type: 'status_reply', reply: ok({ status: 'cancelled', filename: 'a' }), now: T0 },
  ]);
  assert.deepEqual(views.at(-1), { phase: 'note', message: 'Cancelled in rustdm.' });
});

test('sixty seconds of looking gives up politely', () => {
  const lookup = ok({ status: 'active', filename: 'watch' });
  const before = run('page', [
    { type: 'add_reply', reply: ok({ download_id: 'dl_8' }), now: T0 },
    { type: 'status_reply', reply: lookup, now: T0 },
    { type: 'tick', now: T0 + LOOKUP_LIMIT_MS - 1 },
  ]);
  assert.equal(before.effects.at(-1).type, 'status');
  const after = run('page', [
    { type: 'add_reply', reply: ok({ download_id: 'dl_8' }), now: T0 },
    { type: 'status_reply', reply: lookup, now: T0 },
    { type: 'tick', now: T0 + LOOKUP_LIMIT_MS },
  ]);
  assert.deepEqual(after.views.at(-1), { phase: 'note', message: 'Still looking it up; pick the quality in rustdm.' });
  assert.equal(after.effects.at(-1).type, 'stop');
});

test('the list is not bound by the lookup limit', () => {
  const { state } = run('page', [
    { type: 'add_reply', reply: ok({ download_id: 'dl_8' }), now: T0 },
    { type: 'status_reply', reply: ok(choosing), now: T0 },
    { type: 'tick', now: T0 + 5 * LOOKUP_LIMIT_MS },
  ]);
  assert.equal(state.phase, 'choosing');
});

test('a choice made in rustdm turns the card to started', () => {
  const { views } = run('page', [
    { type: 'add_reply', reply: ok({ download_id: 'dl_9' }), now: T0 },
    { type: 'status_reply', reply: ok(choosing), now: T0 },
    { type: 'tick', now: T0 + POLL_MS },
    { type: 'status_reply', reply: ok({ status: 'queued', filename: 'Angel One.mp4', video: { title: 'Angel One', choices: [], chosen: { label: '1080p', container: 'MP4' } } }), now: T0 + POLL_MS },
  ]);
  assert.deepEqual(views.at(-1), { phase: 'started', filename: 'Angel One.mp4' });
});

test('no second status while one is waiting', () => {
  const { effects } = run('page', [
    { type: 'add_reply', reply: ok({ download_id: 'dl_10' }), now: T0 },
    { type: 'tick', now: T0 + POLL_MS },
    { type: 'tick', now: T0 + 2 * POLL_MS },
  ]);
  assert.equal(effects.filter((e) => e.type === 'status').length, 1);
});

test('the connection lost after add', () => {
  const { views, effects } = run('page', [
    { type: 'add_reply', reply: ok({ download_id: 'dl_11' }), now: T0 },
    { type: 'native_lost' },
  ]);
  assert.deepEqual(views.at(-1), { phase: 'failed', message: "Lost the connection to rustdm. The download is still in rustdm's list." });
  assert.equal(effects.at(-1).type, 'stop');
});

test('a failed add shows how to fix it', () => {
  const { views } = run('page', [{ type: 'add_reply', reply: fail('host_missing'), now: T0 }]);
  assert.match(views.at(-1).message, /rdm browser install/);
  const lost = run('page', [{ type: 'native_lost' }]);
  assert.equal(lost.state.phase, 'sending', 'before add replies, the reply carries the failure');
});

test('a duplicate continues with the existing id', () => {
  const { effects } = run('page', [{ type: 'add_reply', reply: fail('duplicate', { existing_id: 'dl_7' }), now: T0 }]);
  assert.deepEqual(effects.at(-1), { type: 'status', downloadId: 'dl_7' });
});

test('a duplicate without an id is already in the list', () => {
  const { views } = run('page', [{ type: 'add_reply', reply: fail('duplicate'), now: T0 }]);
  assert.deepEqual(views.at(-1), { phase: 'note', message: "Already in rustdm's list." });
});

test('a duplicate of a dashboard download is already in the list', () => {
  const { views } = run('page', [
    { type: 'add_reply', reply: fail('duplicate', { existing_id: 'dl_7' }), now: T0 },
    { type: 'status_reply', reply: fail('not_found'), now: T0 },
  ]);
  assert.deepEqual(views.at(-1), { phase: 'note', message: "Already in rustdm's list." });
});

test('add ok then not_found is already in the list', () => {
  const { views } = run('page', [
    { type: 'add_reply', reply: ok({ download_id: 'dl_12' }), now: T0 },
    { type: 'status_reply', reply: fail('not_found'), now: T0 },
  ]);
  assert.deepEqual(views.at(-1), { phase: 'note', message: "Already in rustdm's list." });
});

test('another status failure is shown in plain words', () => {
  const { views } = run('page', [
    { type: 'add_reply', reply: ok({ download_id: 'dl_12' }), now: T0 },
    { type: 'status_reply', reply: fail('not_running'), now: T0 },
  ]);
  assert.match(views.at(-1).message, /rdm daemon start/);
});

test('a refused choice is shown', () => {
  const { views } = run('page', [
    { type: 'add_reply', reply: ok({ download_id: 'dl_13' }), now: T0 },
    { type: 'status_reply', reply: ok(choosing), now: T0 },
    { type: 'choose', choiceId: '720-mp4' },
    { type: 'choose', choiceId: '1080-mp4' },
    { type: 'choose_reply', reply: fail('rejected', { message: "isn't waiting for a quality" }), now: T0 },
  ]);
  assert.equal(views.at(-1).phase, 'failed');
  assert.match(views.at(-1).message, /waiting for a quality/);
});

test('a second click while choosing is ignored', () => {
  const { effects } = run('page', [
    { type: 'add_reply', reply: ok({ download_id: 'dl_13' }), now: T0 },
    { type: 'status_reply', reply: ok(choosing), now: T0 },
    { type: 'choose', choiceId: '720-mp4' },
    { type: 'choose', choiceId: '1080-mp4' },
  ]);
  assert.equal(effects.filter((e) => e.type === 'choose').length, 1);
});

test('a choice outside the list is ignored', () => {
  const early = run('page', [{ type: 'choose', choiceId: '720-mp4' }]);
  assert.equal(early.effects.filter((e) => e.type === 'choose').length, 0);
});

test('a view is sent only when the phase changes', () => {
  const { views } = run('page', [
    { type: 'add_reply', reply: ok({ download_id: 'dl_14' }), now: T0 },
    { type: 'status_reply', reply: ok(choosing), now: T0 },
    { type: 'tick', now: T0 + POLL_MS },
    { type: 'status_reply', reply: ok(choosing), now: T0 + POLL_MS },
  ]);
  assert.equal(views.filter((v) => v.phase === 'choosing').length, 1);
});

test('final phases ignore events', () => {
  const done = run('file', [
    { type: 'add_reply', reply: ok({ download_id: 'dl_15' }), now: T0 },
    { type: 'status_reply', reply: ok({ status: 'queued', filename: 'a' }), now: T0 },
  ]);
  for (const event of [
    { type: 'tick', now: T0 + POLL_MS },
    { type: 'native_lost' },
    { type: 'status_reply', reply: ok({ status: 'failed', error: 'x' }), now: T0 },
  ]) {
    const next = flowStep(done.state, event);
    assert.deepEqual(next.effects, []);
    assert.equal(next.state.phase, 'started');
  }
});

test('a choosing reply without rows keeps looking', () => {
  const { state } = run('page', [
    { type: 'add_reply', reply: ok({ download_id: 'dl_16' }), now: T0 },
    { type: 'status_reply', reply: ok({ status: 'choosing', filename: 'a', video: { title: 't', choices: [] } }), now: T0 },
  ]);
  assert.equal(state.phase, 'looking');
});

test('no checking while the tab is hidden, and it picks up again when shown', () => {
  const base = [
    { type: 'add_reply', reply: ok({ download_id: 'dl_20' }), now: T0 },
    { type: 'status_reply', reply: ok({ status: 'active', filename: 'w' }), now: T0 },
    { type: 'hidden', hidden: true },
  ];
  const hidden = run('page', [...base, { type: 'tick', now: T0 + POLL_MS }, { type: 'tick', now: T0 + 2 * POLL_MS }]);
  assert.equal(hidden.effects.filter((e) => e.type === 'status').length, 1, 'only the first, before hiding');
  const shown = run('page', [...base, { type: 'hidden', hidden: false }, { type: 'tick', now: T0 + POLL_MS }]);
  assert.equal(shown.effects.at(-1).type, 'status');
});

test('a list left open gives up after ten minutes', () => {
  assert.equal(CHOOSING_LIMIT_MS, 10 * 60_000);
  const opened = [
    { type: 'add_reply', reply: ok({ download_id: 'dl_21' }), now: T0 },
    { type: 'status_reply', reply: ok(choosing), now: T0 + 1000 },
  ];
  const before = run('page', [...opened, { type: 'tick', now: T0 + 1000 + CHOOSING_LIMIT_MS - 1 }]);
  assert.equal(before.state.phase, 'choosing');
  const after = run('page', [...opened, { type: 'tick', now: T0 + 1000 + CHOOSING_LIMIT_MS }]);
  assert.deepEqual(after.views.at(-1), { phase: 'note', message: 'Still waiting for a quality; pick it in rustdm.' });
  assert.equal(after.effects.at(-1).type, 'stop');
});

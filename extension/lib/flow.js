// The steps of one card (spec 9.3), as a pure state machine: events in,
// a new state and a list of effects out. The background performs the effects.

import { describeError } from './native.js';

export const POLL_MS = 500;
export const LOOKUP_LIMIT_MS = 60_000;
// A quality list left open this long stops asking rustdm.
export const CHOOSING_LIMIT_MS = 10 * 60_000;

export const LOST = "Lost the connection to rustdm. The download is still in rustdm's list.";
export const ALREADY = "Already in rustdm's list.";
export const STILL_LOOKING = 'Still looking it up; pick the quality in rustdm.';
export const CANCELLED = 'Cancelled in rustdm.';
export const STILL_CHOOSING = 'Still waiting for a quality; pick it in rustdm.';

const ALWAYS_STARTED = new Set(['completed', 'paused', 'pausing', 'retrying']);
const RUNNING = new Set(['queued', 'active']);
const FINAL = new Set(['started', 'failed', 'note']);

// The phase the card shows: "sending" looks the same as "looking".
const shownPhase = (phase) => (phase === 'sending' ? 'looking' : phase);

// The list of a lookup as the card shows it.
function choosingView(video) {
  const duration = video.duration_secs;
  return {
    phase: 'choosing',
    choices: video.choices,
    defaultIndex: video.default ?? 0,
    ...(duration === undefined ? {} : { durationSecs: duration }),
  };
}

/**
 * @param kind 'file' | 'stream' | 'page' (see lib/source.js)
 * @param now ms
 * @param opts `{choiceId}` adds with that quality (the bar's click);
 *   `{answer}` shows a lookup's list at once, adding only on a pick (▾).
 */
export function startFlow(kind, now, opts = {}) {
  const state = {
    kind,
    phase: 'sending',
    downloadId: null,
    lookingSince: now,
    choosingSince: null,
    waiting: false,
    choosing: false,
    hidden: false,
  };
  if (opts.answer) {
    const listed = { ...state, phase: 'choosing', choosingSince: now };
    return { state: listed, effects: [{ type: 'view', view: choosingView(opts.answer) }] };
  }
  const add = opts.choiceId ? { type: 'add', choiceId: opts.choiceId } : { type: 'add' };
  return { state, effects: [add, { type: 'view', view: { phase: 'looking' } }] };
}

// A move to `phase`, with its view only when the card shows something new.
function moveTo(state, phase, view, extra = {}) {
  const next = { ...state, ...extra, phase };
  const effects = shownPhase(state.phase) === shownPhase(phase) ? [] : [{ type: 'view', view }];
  if (FINAL.has(phase)) effects.push({ type: 'stop' });
  return { state: next, effects };
}

const note = (state, message) => moveTo(state, 'note', { phase: 'note', message });
const failed = (state, message) => moveTo(state, 'failed', { phase: 'failed', message });
const started = (state, result) => moveTo(state, 'started', { phase: 'started', filename: result?.filename ?? '' });

function poll(state) {
  return { state: { ...state, waiting: true }, effects: [{ type: 'status', downloadId: state.downloadId }] };
}

// "Started" (ruling E2): a lookup reports `active` with no video yet.
function isStarted(result, kind) {
  if (ALWAYS_STARTED.has(result.status)) return true;
  if (!RUNNING.has(result.status)) return false;
  return Boolean(result.video) || result.progress > 0 || kind === 'file';
}

function onAddReply(state, { reply, now }) {
  const existing = reply.error?.kind === 'duplicate' ? reply.error.existing_id : undefined;
  if (!reply.ok && reply.error?.kind === 'duplicate' && !existing) return note(state, ALREADY);
  if (!reply.ok && !existing) return failed(state, describeError(reply.error));
  const downloadId = reply.ok ? reply.result?.download_id : existing;
  return poll({ ...state, phase: 'looking', downloadId, lookingSince: now });
}

function onStatusReply(state, { reply, now }) {
  const ready = { ...state, waiting: false };
  if (!reply.ok) {
    return reply.error?.kind === 'not_found' ? note(ready, ALREADY) : failed(ready, describeError(reply.error));
  }
  const result = reply.result ?? {};
  if (result.status === 'failed') return failed(ready, result.error || describeError({}));
  if (result.status === 'cancelled') return note(ready, CANCELLED);
  if (isStarted(result, state.kind)) return started(ready, result);
  const rows = result.video?.choices ?? [];
  if (result.status === 'choosing' && rows.length > 0 && state.phase !== 'choosing') {
    return moveTo({ ...ready, choosingSince: now }, 'choosing', choosingView(result.video));
  }
  return { state: ready, effects: [] };
}

function onTick(state, { now }) {
  if (state.hidden) return { state, effects: [] };
  if (state.phase === 'looking' && now - state.lookingSince >= LOOKUP_LIMIT_MS) return note(state, STILL_LOOKING);
  if (state.phase === 'choosing' && now - state.choosingSince >= CHOOSING_LIMIT_MS) return note(state, STILL_CHOOSING);
  const asking = state.phase === 'looking' || state.phase === 'choosing';
  if (asking && state.downloadId && !state.waiting) return poll(state);
  return { state, effects: [] };
}

function onChoose(state, { choiceId }) {
  if (state.phase !== 'choosing' || state.choosing) return { state, effects: [] };
  // A list from a lookup: nothing is in rustdm yet, so the pick adds it.
  if (!state.downloadId) {
    return {
      state: { ...state, phase: 'sending' },
      effects: [{ type: 'add', choiceId }, { type: 'view', view: { phase: 'looking' } }],
    };
  }
  return {
    state: { ...state, choosing: true },
    effects: [{ type: 'choose', downloadId: state.downloadId, choiceId }],
  };
}

function onChooseReply(state, { reply }) {
  if (!state.choosing) return { state, effects: [] };
  return reply.ok ? started(state, reply.result) : failed(state, describeError(reply.error));
}

const HANDLERS = {
  add_reply: (state, event) => (state.phase === 'sending' ? onAddReply(state, event) : { state, effects: [] }),
  status_reply: onStatusReply,
  tick: onTick,
  choose: onChoose,
  choose_reply: onChooseReply,
  hidden: (state, { hidden }) => ({ state: { ...state, hidden: Boolean(hidden) }, effects: [] }),
  native_lost: (state) => (state.downloadId ? failed(state, LOST) : { state, effects: [] }),
};

/** One event in, `{state, effects}` out. Final phases ignore everything. */
export function flowStep(state, event) {
  const handler = HANDLERS[event?.type];
  if (FINAL.has(state.phase) || !handler) return { state, effects: [] };
  return handler(state, event);
}

// content/hover-logic.js is a classic script (content scripts are not
// modules), so it is run here in a fresh context and read from globalThis.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';

const source = readFileSync(new URL('../content/hover-logic.js', import.meta.url), 'utf8');
const context = { WeakSet };
vm.runInNewContext(source, context);
const h = context.rdmHover;

const rect = (left, top, width, height) => ({ left, top, width, height, right: left + width, bottom: top + height });
const viewport = { width: 1280, height: 720 };
const BAR = { width: 180, height: 30 };
const memory = () => ({ dismissed: new WeakSet(), played: new WeakSet() });

function steps(events, mem = memory()) {
  let bar = h.createBar();
  for (const event of events) bar = h.barStep(bar, event, mem);
  return bar;
}

test('the constants match the spec', () => {
  assert.deepEqual({ ...h.MIN_HOVER }, { width: 200, height: 112 });
  assert.deepEqual({ ...h.MIN_PLAY }, { width: 320, height: 180 });
  assert.equal(h.HIDE_AFTER_MS, 1500);
  assert.equal(h.PLAY_SHOW_MS, 4000);
});

test('a point inside and outside a rectangle', () => {
  const r = rect(10, 10, 100, 50);
  assert.equal(h.containsPoint(r, 10, 10), true);
  assert.equal(h.containsPoint(r, 110, 60), true);
  assert.equal(h.containsPoint(r, 111, 30), false);
  assert.equal(h.containsPoint(r, 50, 9), false);
});

test('a small video is not picked', () => {
  const pick = (w, ht) => h.pickHovered([{ video: 'v', rect: rect(0, 0, w, ht), visible: true }], 5, 5);
  assert.equal(pick(199, 112), null);
  assert.equal(pick(200, 111), null);
  assert.equal(pick(200, 112).video, 'v');
});

test('the topmost video wins', () => {
  const candidates = [
    { video: 'top', rect: rect(0, 0, 400, 300), visible: true },
    { video: 'under', rect: rect(0, 0, 800, 600), visible: true },
  ];
  assert.equal(h.pickHovered(candidates, 100, 100).video, 'top');
  assert.equal(h.pickHovered(candidates, 500, 500).video, 'under');
});

test('an invisible video is skipped', () => {
  const candidates = [
    { video: 'hidden', rect: rect(0, 0, 400, 300), visible: false },
    { video: 'shown', rect: rect(0, 0, 400, 300), visible: true },
  ];
  assert.equal(h.pickHovered(candidates, 100, 100).video, 'shown');
});

test('the bar sits above the video', () => {
  assert.deepEqual({ ...h.barPlacement(rect(100, 200, 640, 360), viewport, BAR) }, { left: 560, top: 166 });
});

test('the bar goes inside the corner at the top of the screen', () => {
  assert.deepEqual({ ...h.barPlacement(rect(100, 10, 640, 360), viewport, BAR) }, { left: 552, top: 18 });
  assert.deepEqual({ ...h.barPlacement(rect(0, 0, 1280, 720), viewport, BAR) }, { left: 1092, top: 8 });
});

test('the bar stays on the screen', () => {
  const placed = h.barPlacement(rect(-200, 300, 2000, 400), viewport, BAR);
  assert.equal(placed.left, viewport.width - BAR.width);
  const left = h.barPlacement(rect(-900, 300, 1000, 400), viewport, BAR);
  assert.equal(left.left, 0);
});

test('the card goes below the bar, or above it when there is no room', () => {
  const card = { width: 300, height: 260 };
  assert.deepEqual({ ...h.cardPlacement(rect(560, 166, 180, 30), viewport, card) }, { left: 440, top: 200 });
  assert.deepEqual({ ...h.cardPlacement(rect(560, 600, 180, 30), viewport, card) }, { left: 440, top: 336 });
  const tight = h.cardPlacement(rect(5, 300, 180, 30), { width: 400, height: 400 }, card);
  assert.equal(tight.left, 0);
  assert.ok(tight.top >= 0 && tight.top + card.height <= 400);
});

test('the bar hides 1500 ms after the pointer leaves', () => {
  const v = {};
  const away = [{ type: 'hover', video: v, now: 0 }, { type: 'away', now: 100 }];
  assert.equal(steps([...away, { type: 'tick', now: 1599 }]).video, v);
  assert.equal(steps([...away, { type: 'tick', now: 1600 }]).video, null);
  assert.equal(steps([...away, { type: 'away', now: 1000 }, { type: 'tick', now: 1600 }]).video, null, 'a later away does not push it back');
  assert.equal(steps([...away, { type: 'hover', video: v, now: 1000 }, { type: 'tick', now: 5000 }]).video, v, 'coming back cancels it');
});

test('the bar stays while the card is open', () => {
  const v = {};
  const open = [{ type: 'hover', video: v, now: 0 }, { type: 'card', open: true, now: 10 }, { type: 'away', now: 20 }, { type: 'tick', now: 9000 }];
  assert.equal(steps(open).video, v);
  const closed = [...open, { type: 'card', open: false, now: 9000 }];
  assert.equal(steps([...closed, { type: 'tick', now: 10_499 }]).video, v);
  assert.equal(steps([...closed, { type: 'tick', now: 10_500 }]).video, null);
});

test('the first play with sound shows it for 4 s, once', () => {
  const v = {};
  const mem = memory();
  let bar = steps([{ type: 'played', video: v, now: 0 }, { type: 'tick', now: 3999 }], mem);
  assert.equal(bar.video, v);
  bar = h.barStep(bar, { type: 'tick', now: 4000 }, mem);
  assert.equal(bar.video, null);
  bar = h.barStep(bar, { type: 'played', video: v, now: 5000 }, mem);
  assert.equal(bar.video, null, 'only once per video');
  assert.equal(h.barStep(bar, { type: 'hover', video: v, now: 6000 }, mem).video, v, 'hover still works');
});

test('muted, silent or small play never shows', () => {
  const big = rect(0, 0, 320, 180);
  assert.equal(h.shouldShowOnPlay({ muted: false, volume: 0.5, rect: big, visible: true }), true);
  assert.equal(h.shouldShowOnPlay({ muted: true, volume: 0.5, rect: big, visible: true }), false);
  assert.equal(h.shouldShowOnPlay({ muted: false, volume: 0, rect: big, visible: true }), false);
  assert.equal(h.shouldShowOnPlay({ muted: false, volume: 1, rect: rect(0, 0, 319, 180), visible: true }), false);
  assert.equal(h.shouldShowOnPlay({ muted: false, volume: 1, rect: rect(0, 0, 320, 179), visible: true }), false);
  assert.equal(h.shouldShowOnPlay({ muted: false, volume: 1, rect: big, visible: false }), false);
});

test('hovering during the 4 s keeps it', () => {
  const v = {};
  const bar = steps([{ type: 'played', video: v, now: 0 }, { type: 'hover', video: v, now: 1000 }, { type: 'tick', now: 4500 }]);
  assert.equal(bar.video, v);
  assert.equal(bar.mode, 'hover');
});

test('the close button hides that video only', () => {
  const a = {};
  const b = {};
  const mem = memory();
  let bar = steps([{ type: 'hover', video: a, now: 0 }, { type: 'dismiss', video: a }], mem);
  assert.equal(bar.video, null);
  bar = h.barStep(bar, { type: 'hover', video: a, now: 10 }, mem);
  assert.equal(bar.video, null);
  bar = h.barStep(bar, { type: 'played', video: a, now: 10 }, mem);
  assert.equal(bar.video, null);
  bar = h.barStep(bar, { type: 'hover', video: b, now: 20 }, mem);
  assert.equal(bar.video, b);
});

test('one bar at a time, the most recent video wins', () => {
  const a = {};
  const b = {};
  const bar = steps([{ type: 'hover', video: a, now: 0 }, { type: 'played', video: b, now: 10 }]);
  assert.equal(bar.video, b);
  assert.equal(bar.mode, 'play');
});

test('a removed or scrolled away video hides the bar', () => {
  const a = {};
  const b = {};
  assert.equal(steps([{ type: 'hover', video: a, now: 0 }, { type: 'gone', video: b }]).video, a);
  assert.equal(steps([{ type: 'hover', video: a, now: 0 }, { type: 'gone', video: a }]).video, null);
});

test('barStep does not change the bar it is given', () => {
  const bar = Object.freeze(h.createBar());
  const next = h.barStep(bar, { type: 'hover', video: {}, now: 0 }, memory());
  assert.equal(bar.video, null);
  assert.notEqual(next, bar);
});

test('fullscreen: a container takes the bar, a bare video does not', () => {
  const root = { tagName: 'HTML' };
  assert.equal(h.fullscreenParent({ tagName: 'DIV' }, root).tagName, 'DIV');
  assert.equal(h.fullscreenParent({ tagName: 'VIDEO' }, root), root);
  assert.equal(h.fullscreenParent(null, root), root);
});

test('byte sizes read like the dashboard', () => {
  assert.equal(h.formatBytes(0), '0 B');
  assert.equal(h.formatBytes(1023), '1023 B');
  assert.equal(h.formatBytes(1536), '1.5 KiB');
  assert.equal(h.formatBytes(6710886), '6.4 MiB');
  assert.equal(h.formatBytes(1024 ** 5 * 2), '2048.0 TiB');
});

test('a row shows the estimate mark and 4K', () => {
  const row = h.choiceRow({ id: 'x', label: '2160p', container: 'MP4', height: 2160, size_bytes: 1073741824, size_is_estimate: true });
  assert.deepEqual({ ...row }, { label: '2160p', container: 'MP4', size: '~1.0 GiB (4K)' });
  assert.equal(h.choiceRow({ label: '720p', container: 'WebM', height: 720, size_bytes: 1536, size_is_estimate: false }).size, '1.5 KiB');
});

test('a null size is blank', () => {
  assert.equal(h.choiceRow({ label: 'Audio only', container: 'M4A', height: null, size_bytes: null, size_is_estimate: false }).size, '');
  assert.equal(h.choiceRow({ label: '4K', container: 'MP4', height: 2160, size_bytes: null }).size, '(4K)');
});

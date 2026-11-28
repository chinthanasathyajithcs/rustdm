// content/hover-logic.js is a classic script (content scripts are not
// modules), so it is run here in a fresh context and read from globalThis.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';

const source = readFileSync(new URL('../content/hover-logic.js', import.meta.url), 'utf8');
const context = { WeakSet, URL };
vm.runInNewContext(source, context);
const h = context.rdmHover;

const rect = (left, top, width, height) => ({ left, top, width, height, right: left + width, bottom: top + height });
const viewport = { width: 1280, height: 720 };
const BAR = { width: 180, height: 30 };
const memory = () => ({ dismissed: new WeakSet(), played: new WeakSet() });

// A hover that shows the bar: the pointer is on the video, then still there HOVER_DELAY_MS later.
const hoverAt = (video, now) => [
  { type: 'hover', video, now },
  { type: 'hover', video, now: now + 150 },
];

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

test('the bar sits inside the video, in its top-right corner', () => {
  assert.deepEqual({ ...h.barPlacement(rect(100, 200, 640, 360), viewport, BAR) }, { left: 552, top: 208 });
  assert.deepEqual({ ...h.barPlacement(rect(100, 10, 640, 360), viewport, BAR) }, { left: 552, top: 18 });
  assert.deepEqual({ ...h.barPlacement(rect(0, 0, 1280, 720), viewport, BAR) }, { left: 1092, top: 8 });
});

test('the bar stays on the screen', () => {
  const placed = h.barPlacement(rect(-200, 300, 2000, 400), viewport, BAR);
  assert.equal(placed.left, viewport.width - BAR.width);
  const left = h.barPlacement(rect(-900, 300, 1000, 400), viewport, BAR);
  assert.equal(left.left, 0);
  const scrolledUp = h.barPlacement(rect(100, -100, 640, 360), viewport, BAR);
  assert.equal(scrolledUp.top, 0, 'the top of the video is off screen: the bar waits at the top of the screen, still over the video');
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
  const away = [...hoverAt(v, 0), { type: 'away', now: 100 }];
  assert.equal(steps([...away, { type: 'tick', now: 1599 }]).video, v);
  assert.equal(steps([...away, { type: 'tick', now: 1600 }]).video, null);
  assert.equal(steps([...away, { type: 'away', now: 1000 }, { type: 'tick', now: 1600 }]).video, null, 'a later away does not push it back');
  assert.equal(steps([...away, ...hoverAt(v, 1000), { type: 'tick', now: 5000 }]).video, v, 'coming back cancels it');
});

test('the bar stays while the card is open', () => {
  const v = {};
  const open = [...hoverAt(v, 0), { type: 'card', open: true, now: 10 }, { type: 'away', now: 20 }, { type: 'tick', now: 9000 }];
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
  for (const event of hoverAt(v, 6000)) bar = h.barStep(bar, event, mem);
  assert.equal(bar.video, v, 'hover still works');
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
  const bar = steps([{ type: 'played', video: v, now: 0 }, ...hoverAt(v, 1000), { type: 'tick', now: 4500 }]);
  assert.equal(bar.video, v);
  assert.equal(bar.mode, 'hover');
});

test('the close button hides that video only', () => {
  const a = {};
  const b = {};
  const mem = memory();
  let bar = steps([...hoverAt(a, 0), { type: 'dismiss', video: a }], mem);
  assert.equal(bar.video, null);
  for (const event of hoverAt(a, 10)) bar = h.barStep(bar, event, mem);
  assert.equal(bar.video, null);
  bar = h.barStep(bar, { type: 'played', video: a, now: 10 }, mem);
  assert.equal(bar.video, null);
  for (const event of hoverAt(b, 20)) bar = h.barStep(bar, event, mem);
  assert.equal(bar.video, b);
});

test('one bar at a time, the most recent video wins', () => {
  const a = {};
  const b = {};
  const bar = steps([...hoverAt(a, 0), { type: 'played', video: b, now: 10 }]);
  assert.equal(bar.video, b);
  assert.equal(bar.mode, 'play');
});

test('a removed or scrolled away video hides the bar', () => {
  const a = {};
  const b = {};
  assert.equal(steps([...hoverAt(a, 0), { type: 'gone', video: b }]).video, a);
  assert.equal(steps([...hoverAt(a, 0), { type: 'gone', video: a }]).video, null);
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

test('a duration reads like the dashboard', () => {
  assert.equal(h.formatDuration(60), '1:00');
  assert.equal(h.formatDuration(5), '0:05');
  assert.equal(h.formatDuration(3723), '1:02:03');
});

test('visibility is asked only of a video under the pointer', () => {
  const asked = [];
  const candidate = (name, r) => ({ video: name, rect: r, visible: () => (asked.push(name), true) });
  const found = h.pickHovered(
    [candidate('elsewhere', rect(500, 500, 400, 300)), candidate('small', rect(0, 0, 100, 50)), candidate('here', rect(0, 0, 400, 300))],
    20,
    20,
  );
  assert.equal(found.video, 'here');
  assert.deepEqual(asked, ['here']);
});

test('a pointer only passing over a video shows nothing', () => {
  const v = {};
  assert.equal(h.HOVER_DELAY_MS, 150);
  assert.equal(steps([{ type: 'hover', video: v, now: 0 }]).video, null);
  assert.equal(steps([{ type: 'hover', video: v, now: 0 }, { type: 'hover', video: v, now: 149 }]).video, null);
  assert.equal(steps([{ type: 'hover', video: v, now: 0 }, { type: 'away', now: 100 }, { type: 'hover', video: v, now: 200 }]).video, null, 'leaving starts the wait again');
  assert.equal(steps([{ type: 'hover', video: v, now: 0 }, { type: 'hover', video: v, now: 150 }]).video, v);
});

test('moving to another video waits before the bar moves there', () => {
  const a = {};
  const b = {};
  const moving = [...hoverAt(a, 0), { type: 'hover', video: b, now: 500 }];
  assert.equal(steps(moving).video, a);
  assert.equal(steps([...moving, { type: 'hover', video: b, now: 650 }]).video, b);
});

test('while the card is open the bar stays on its video', () => {
  const a = {};
  const b = {};
  const bar = steps([...hoverAt(a, 0), { type: 'card', open: true, now: 200 }, ...hoverAt(b, 300)]);
  assert.equal(bar.video, a);
  assert.equal(bar.pending, null);
});

test('the gap between the bar and the video counts as the bar', () => {
  const grown = h.inflate(rect(560, 166, 180, 30), 6);
  assert.deepEqual({ ...grown }, { left: 554, top: 160, right: 746, bottom: 202, width: 192, height: 42 });
  assert.equal(h.containsPoint(grown, 600, 199), true);
});

// A tiny stand-in for page elements: what videoLink reads (closest, parentElement, querySelectorAll).
function el({ link = null, parent = null, links = [] } = {}) {
  const node = {
    parentElement: parent,
    closest: (selector) => (selector === 'a[href]' ? link : null),
    querySelectorAll: () => links.map((href) => ({ getAttribute: () => href })),
  };
  return node;
}
const anchor = (href) => ({ getAttribute: (name) => (name === 'href' ? href : null) });

test('a preview inside a link uses that link', () => {
  const video = el({ link: anchor('/clips/42?ref=home') });
  assert.equal(h.videoLink(video, 'https://news.example.com/'), 'https://news.example.com/clips/42?ref=home');
});

test('a link that is not a web page is ignored', () => {
  assert.equal(h.videoLink(el({ link: anchor('javascript:void(0)') }), 'https://e.com/'), null);
});

test('a video in no link on another site has none', () => {
  const card = el({ links: ['https://e.com/a', 'https://e.com/b'] });
  assert.equal(h.videoLink(el({ parent: card }), 'https://e.com/'), null);
});

test('a YouTube preview uses the one video its card links to', () => {
  const card = el({ links: ['/watch?v=dQw4w9WgXcQ&pp=abc', '/watch?v=dQw4w9WgXcQ', '/@channel'] });
  const inner = el({ parent: card });
  assert.equal(h.videoLink(el({ parent: inner }), 'https://www.youtube.com/'), 'https://www.youtube.com/watch?v=dQw4w9WgXcQ');
  const short = el({ links: ['/shorts/abcdefghijk'] });
  assert.equal(h.videoLink(el({ parent: short }), 'https://m.youtube.com/feed/subscriptions'), 'https://www.youtube.com/watch?v=abcdefghijk');
});

test('a YouTube box naming two videos names none', () => {
  const column = el({ links: ['/watch?v=aaaaaaaaaaa', '/watch?v=bbbbbbbbbbb'] });
  const card = el({ parent: column, links: [] });
  assert.equal(h.videoLink(el({ parent: card }), 'https://www.youtube.com/'), null);
});

// The hover lookup (spec 1f).
test('lookup is due after 600 ms on the video', () => {
  assert.equal(h.LOOKUP_REST_MS, 600);
  assert.equal(h.lookupDue({ restingSince: 1000, onBar: false, now: 1599, asked: false }), false);
  assert.equal(h.lookupDue({ restingSince: 1000, onBar: false, now: 1600, asked: false }), true);
});

test('lookup is due after 200 ms on the bar', () => {
  assert.equal(h.BAR_LOOKUP_REST_MS, 200);
  assert.equal(h.lookupDue({ restingSince: 1000, onBar: true, now: 1199, asked: false }), false);
  assert.equal(h.lookupDue({ restingSince: 1000, onBar: true, now: 1200, asked: false }), true);
});

test('a quick pass is never due', () => {
  assert.equal(h.lookupDue({ restingSince: null, onBar: false, now: 5000, asked: false }), false);
});

test('due only once', () => {
  assert.equal(h.lookupDue({ restingSince: 1000, onBar: false, now: 9000, asked: true }), false);
});

const looked = (choice) => ({ title: 'A', choices: [choice, { id: 'audio-m4a', label: 'Audio only' }], default: 0 });

test('bar label with size', () => {
  const label = h.barLabel(looked({ id: '1080-mp4', label: '1080p', size_bytes: 52 * 1024 * 1024, size_is_estimate: false }));
  assert.equal(label, 'Download 1080p · 52.0 MiB');
  const guess = h.barLabel(looked({ id: '1080-mp4', label: '1080p', size_bytes: 52 * 1024 * 1024, size_is_estimate: true }));
  assert.equal(guess, 'Download 1080p · ~52.0 MiB');
});

test('bar label without size', () => {
  assert.equal(h.barLabel(looked({ id: '720-mp4', label: '720p', size_bytes: null })), 'Download 720p');
  assert.equal(h.barLabel(null), 'Download this video');
  assert.equal(h.barLabel({ choices: [], default: 0 }), 'Download this video');
});

test('audio only label', () => {
  const answer = { choices: [{ id: 'audio-m4a', label: 'Audio only' }], default: 0 };
  assert.equal(h.barLabel(answer), 'Download Audio only');
});

test('an answer from another page address is not used', () => {
  const entry = { page: 'https://www.youtube.com/watch?v=A', found: { choices: [{ id: '1080-mp4', label: '1080p' }], default: 0 } };
  assert.equal(h.answerOn(entry, 'https://www.youtube.com/watch?v=A'), entry);
  assert.equal(h.answerOn(entry, 'https://www.youtube.com/watch?v=B'), null);
  assert.equal(h.answerOn(undefined, 'https://www.youtube.com/watch?v=A'), null);
});

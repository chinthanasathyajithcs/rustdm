import assert from 'node:assert/strict';
import test from 'node:test';
import { otherSources, pickSource, resolveSource } from '../lib/source.js';

const entry = (over) => ({ id: 'f1', url: 'https://cdn.e.com/a.mp4', kind: 'file', host: 'cdn.e.com', name: 'a.mp4', frameId: 0, seenAt: 1, ...over });
// Newest first, as addCaught keeps them.
const caught = [
  entry({ id: 'f2', name: 'b.mp4', url: 'https://cdn.e.com/b.mp4', frameId: 0 }),
  entry({ id: 'h1', kind: 'hls', name: 'master.m3u8', url: 'https://cdn.e.com/master.m3u8', frameId: 0 }),
  entry({ id: 'x1', kind: 'hls', name: 'ad.m3u8', host: 'ads.net', url: 'https://ads.net/ad.m3u8', frameId: 3 }),
];
const sender = (over = {}) => ({
  id: 'rustdm-extension-id',
  url: 'https://e.com/watch?v=1',
  frameId: 0,
  tab: { id: 7, url: 'https://e.com/watch?v=1', title: '  Angel One ', incognito: false },
  ...over,
});

test('an http currentSrc wins', () => {
  assert.deepEqual(pickSource({ currentSrc: 'https://cdn.e.com/v.mp4', frameId: 0, caught }), {
    type: 'element',
    url: 'https://cdn.e.com/v.mp4',
  });
});

test('a blob uses the same frame\'s HLS before its file', () => {
  assert.deepEqual(pickSource({ currentSrc: 'blob:https://e.com/9f', frameId: 0, caught }), { type: 'caught', id: 'h1' });
  const filesOnly = caught.filter((e) => e.kind === 'file');
  assert.deepEqual(pickSource({ currentSrc: 'blob:https://e.com/9f', frameId: 0, caught: filesOnly }), { type: 'caught', id: 'f2' });
});

test('another frame\'s entry is not used', () => {
  assert.deepEqual(pickSource({ currentSrc: '', frameId: 5, caught }), { type: 'page' });
});

test('nothing caught means this page', () => {
  assert.deepEqual(pickSource({ currentSrc: undefined, frameId: 0, caught: [] }), { type: 'page' });
});

test('other sources lists the rest and this page', () => {
  assert.deepEqual(otherSources({ type: 'caught', id: 'h1' }, caught), [
    { key: 'f2', label: 'b.mp4 · cdn.e.com' },
    { key: 'x1', label: 'ad.m3u8 · ads.net' },
    { key: 'page', label: 'This page' },
  ]);
  assert.deepEqual(otherSources({ type: 'page' }, [caught[0]]), [{ key: 'f2', label: 'b.mp4 · cdn.e.com' }]);
  assert.deepEqual(otherSources({ type: 'page' }, []), []);
  assert.equal(otherSources({ type: 'element', url: 'https://cdn.e.com/v.mp4' }, []).length, 1);
});

test('refused without a tab or in incognito', () => {
  assert.deepEqual(resolveSource({ type: 'page' }, sender({ tab: undefined }), caught), { ok: false });
  const incognito = sender();
  incognito.tab.incognito = true;
  assert.deepEqual(resolveSource({ type: 'page' }, incognito, caught), { ok: false });
});

test('a caught id from another tab is refused', () => {
  assert.deepEqual(resolveSource({ type: 'caught', id: 'zz' }, sender(), caught), { ok: false });
  assert.deepEqual(resolveSource({ type: 'caught' }, sender(), caught), { ok: false });
});

test('a non-http element is refused', () => {
  assert.deepEqual(resolveSource({ type: 'element', url: 'blob:https://e.com/1' }, sender(), caught), { ok: false });
  assert.deepEqual(resolveSource({ type: 'element', url: 'javascript:alert(1)' }, sender(), caught), { ok: false });
  assert.deepEqual(resolveSource({ type: 'nonsense' }, sender(), caught), { ok: false });
});

test('an element is a file with the frame as referer', () => {
  assert.deepEqual(resolveSource({ type: 'element', url: 'https://cdn.e.com/v.webm' }, sender(), caught), {
    ok: true,
    url: 'https://cdn.e.com/v.webm',
    referer: 'https://e.com/watch?v=1',
    kind: 'file',
    title: 'Angel One',
    cardTitle: 'Angel One',
  });
});

test('an element m3u8 is a stream', () => {
  const resolved = resolveSource({ type: 'element', url: 'https://cdn.e.com/hls/Index.M3U8?sig=1' }, sender(), caught);
  assert.equal(resolved.kind, 'stream');
});

test('a caught entry gives its url and kind', () => {
  const hls = resolveSource({ type: 'caught', id: 'h1' }, sender(), caught);
  assert.equal(hls.url, 'https://cdn.e.com/master.m3u8');
  assert.equal(hls.kind, 'stream');
  assert.equal(hls.referer, 'https://e.com/watch?v=1');
  assert.equal(resolveSource({ type: 'caught', id: 'f2' }, sender(), caught).kind, 'file');
});

test('a frame page source uses the frame url and the tab title', () => {
  const frame = sender({ url: 'https://www.youtube.com/embed/x', frameId: 4 });
  frame.tab.title = 'Blog post';
  const resolved = resolveSource({ type: 'page' }, frame, caught);
  assert.equal(resolved.url, 'https://www.youtube.com/embed/x');
  assert.equal(resolved.kind, 'page');
  assert.equal(resolved.title, 'Blog post');
  assert.equal('referer' in resolved, false);
});

test('a page that is not http is refused', () => {
  assert.deepEqual(resolveSource({ type: 'page' }, sender({ url: 'about:blank', frameId: 2 }), caught), { ok: false });
});

test('the card title falls back to the host', () => {
  const untitled = sender();
  untitled.tab.title = '   ';
  const resolved = resolveSource({ type: 'page' }, untitled, caught);
  assert.equal(resolved.cardTitle, 'e.com');
  assert.equal('title' in resolved, false);
});

test('the main page uses the tab address, which follows in-page navigation', () => {
  // YouTube moves from the home page to a Short without loading a new page:
  // the sender's address stays the first one, the tab's is current.
  const moved = sender({ url: 'https://www.youtube.com/', frameId: 0 });
  moved.tab.url = 'https://www.youtube.com/shorts/abc123';
  const page = resolveSource({ type: 'page' }, moved, caught);
  assert.equal(page.url, 'https://www.youtube.com/shorts/abc123');
  const element = resolveSource({ type: 'element', url: 'https://cdn.e.com/v.mp4' }, moved, caught);
  assert.equal(element.referer, 'https://www.youtube.com/shorts/abc123');
});

test('a frame keeps its own address', () => {
  const frame = sender({ url: 'https://www.youtube.com/embed/x', frameId: 4 });
  frame.tab.url = 'https://blog.example.com/post#comments';
  assert.equal(resolveSource({ type: 'page' }, frame, caught).url, 'https://www.youtube.com/embed/x');
});

import { isFeedPage, refinePage } from '../lib/source.js';

test('feed pages: YouTube outside a video page, and many videos on a feed address', () => {
  assert.equal(isFeedPage('https://www.youtube.com/', 1), true);
  assert.equal(isFeedPage('https://www.youtube.com/@channel/videos', 1), true);
  assert.equal(isFeedPage('https://www.youtube.com/results?search_query=x', 5), true);
  assert.equal(isFeedPage('https://www.youtube.com/watch?v=abcdefghijk&list=RD1', 9), false);
  assert.equal(isFeedPage('https://www.youtube.com/shorts/abcdefghijk', 3), false);
  assert.equal(isFeedPage('https://www.youtube-nocookie.com/embed/abcdefghijk', 1), false);
  assert.equal(isFeedPage('https://x.com/home', 4), true);
  assert.equal(isFeedPage('https://x.com/home', 1), false, 'one video on the page: it is that video');
  assert.equal(isFeedPage('https://site.example/', 3), true);
  assert.equal(isFeedPage('https://site.example/news/story-1.html', 3), false);
});

test('the page source becomes the video page when one is known', () => {
  const youtube = 'https://www.youtube.com/';
  assert.deepEqual(refinePage({ address: youtube, playerId: 'abcdefghijk', link: null, videos: 8 }), {
    type: 'link',
    url: 'https://www.youtube.com/watch?v=abcdefghijk',
  });
  assert.deepEqual(refinePage({ address: youtube, playerId: '', link: 'https://www.youtube.com/watch?v=zzzzzzzzzzz', videos: 8 }), {
    type: 'link',
    url: 'https://www.youtube.com/watch?v=zzzzzzzzzzz',
  });
  assert.deepEqual(refinePage({ address: youtube, playerId: '', link: null, videos: 8 }), { type: 'feed' });
  assert.deepEqual(refinePage({ address: 'https://blog.example/post', playerId: '', link: null, videos: 1 }), { type: 'page' });
  assert.deepEqual(refinePage({ address: 'https://e.com/', playerId: 'not an id!', link: 'javascript:x', videos: 1 }), { type: 'page' });
});

test('a video page source is looked up like a page, without a referer', () => {
  const resolved = resolveSource({ type: 'link', url: 'https://www.youtube.com/watch?v=abcdefghijk' }, sender(), caught);
  assert.equal(resolved.ok, true);
  assert.equal(resolved.url, 'https://www.youtube.com/watch?v=abcdefghijk');
  assert.equal(resolved.kind, 'page');
  assert.equal('referer' in resolved, false);
  assert.deepEqual(resolveSource({ type: 'link', url: 'file:///etc/passwd' }, sender(), caught), { ok: false });
});

test('a feed is refused with its own reason', () => {
  assert.deepEqual(resolveSource({ type: 'feed' }, sender(), caught), { ok: false, reason: 'feed' });
});

test('a video page source offers the caught streams but not the feed page', () => {
  assert.deepEqual(otherSources({ type: 'link', url: 'https://www.youtube.com/watch?v=abcdefghijk' }, []), []);
  assert.deepEqual(otherSources({ type: 'feed' }, [caught[0]]), [{ key: 'f2', label: 'b.mp4 · cdn.e.com' }]);
});

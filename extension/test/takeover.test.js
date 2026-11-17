import assert from 'node:assert/strict';
import test from 'node:test';
import { POST_WINDOW_MS, recordPost, shouldTakeOver, MAX_RECENT_POSTS } from '../lib/takeover.js';

const NOW = 1_000_000;
const on = { enabled: true, granted: true };
const zip = () => ({
  url: 'https://example.com/get?id=1',
  finalUrl: 'https://cdn.example.com/x.zip',
  mime: 'application/zip',
  totalBytes: 50 * 1024 * 1024,
  incognito: false,
});
const decide = (item, settings = on, posts = new Map()) => shouldTakeOver(item, settings, posts, NOW);

test('a large file download is taken over', () => {
  assert.deepEqual(decide(zip()), { take: true, reason: 'take' });
});

test('decision table: each rule leaves the download with the browser', () => {
  const cases = [
    ['switch off', zip(), { enabled: false, granted: true }, 'disabled'],
    ['no <all_urls> grant', zip(), { enabled: true, granted: false }, 'no-permission'],
    ['blob url', { ...zip(), finalUrl: 'blob:https://example.com/abc' }, on, 'not-http'],
    ['data url', { ...zip(), finalUrl: 'data:text/plain,hi' }, on, 'not-http'],
    ['file url', { ...zip(), finalUrl: 'file:///tmp/x' }, on, 'not-http'],
    ['incognito', { ...zip(), incognito: true }, on, 'incognito'],
    ['started by an extension', { ...zip(), byExtensionId: 'abc' }, on, 'by-extension'],
    ['html page', { ...zip(), mime: 'text/html; charset=utf-8' }, on, 'html'],
    ['xhtml page', { ...zip(), mime: 'Application/XHTML+xml' }, on, 'html'],
    ['under 1 MiB', { ...zip(), totalBytes: 1024 * 1024 - 1 }, on, 'small'],
    ['tiny', { ...zip(), totalBytes: 10 }, on, 'small'],
  ];
  for (const [name, item, settings, reason] of cases) {
    assert.deepEqual(decide(item, settings), { take: false, reason }, name);
  }
});

test('1 MiB exactly, unknown size and a missing mime type are taken over', () => {
  assert.equal(decide({ ...zip(), totalBytes: 1024 * 1024 }).take, true);
  assert.equal(decide({ ...zip(), totalBytes: -1 }).take, true);
  assert.equal(decide({ ...zip(), totalBytes: 0 }).take, true);
  assert.equal(decide({ ...zip(), mime: undefined }).take, true);
});

test('finalUrl falls back to url when the browser gives none', () => {
  const { finalUrl, ...item } = zip();
  assert.equal(decide(item).take, true);
  assert.equal(decide({ ...item, url: 'ftp://example.com/x' }).take, false);
});

test('a URL POSTed in the last 30 seconds is left alone, an older one is not', () => {
  const item = zip();
  const posts = new Map([[item.finalUrl, NOW - 1000]]);
  assert.deepEqual(decide(item, on, posts), { take: false, reason: 'recent-post' });
  assert.equal(decide(item, on, new Map([[item.url, NOW - 1000]])).take, false);
  assert.equal(decide(item, on, new Map([[item.finalUrl, NOW - POST_WINDOW_MS - 1]])).take, true);
  assert.equal(decide(item, on, new Map([['https://other.example/', NOW]])).take, true);
});

test('recordPost keeps the newest entries and drops old ones', () => {
  const posts = new Map();
  recordPost(posts, 'https://a/', 0);
  recordPost(posts, 'https://b/', POST_WINDOW_MS + 10);
  assert.deepEqual([...posts.keys()], ['https://b/']);

  const many = new Map();
  for (let i = 0; i < MAX_RECENT_POSTS + 50; i++) recordPost(many, `https://x/${i}`, 5);
  assert.equal(many.size, MAX_RECENT_POSTS);
  assert.ok(many.has(`https://x/${MAX_RECENT_POSTS + 49}`));
  assert.ok(!many.has('https://x/0'));
});

// video.js against a fake `chrome` (test/fake-chrome.js).
import assert from 'node:assert/strict';
import test from 'node:test';
import { installVideo } from '../video.js';
import { makeChrome, settle } from './fake-chrome.js';

const MiB = 1024 * 1024;

function install(options = {}) {
  const chrome = makeChrome(options);
  const clock = { now: 1_000_000 };
  const timers = new Map();
  let nextTimer = 1;
  const video = installVideo({
    chrome,
    setInterval: (fn) => {
      timers.set(nextTimer, fn);
      return nextTimer++;
    },
    clearInterval: (id) => timers.delete(id),
    now: () => clock.now,
    collectCookies: async (urls) => {
      chrome.state.log.push(['cookies', urls]);
      return chrome.state.cookies ?? [];
    },
    flashOk: async () => chrome.state.log.push(['ok']),
    showFailure: async (error) => chrome.state.log.push(['failure', error]),
  });
  // Run every interval once, as if POLL_MS passed.
  const tick = async (ms = 500) => {
    clock.now += ms;
    for (const fn of [...timers.values()]) fn();
    await settle();
  };
  return { chrome, video, clock, timers, tick };
}

function streamResponse(over = {}) {
  return {
    url: 'https://cdn.example.com/movie.mp4',
    type: 'media',
    statusCode: 200,
    tabId: 7,
    frameId: 0,
    responseHeaders: [
      { name: 'content-type', value: 'video/mp4' },
      { name: 'content-length', value: String(5 * MiB) },
    ],
    ...over,
  };
}

const headers = (chrome, details) => chrome.webRequest.onHeadersReceived.fire(details);

test('a caught stream is stored for its tab', async () => {
  const { chrome, video } = install();
  await settle();
  headers(chrome, streamResponse());
  const list = await video.caughtFor(7);
  assert.equal(list.length, 1);
  assert.equal(list[0].name, 'movie.mp4');
  assert.match(list[0].id, /^[0-9a-f]{8}$/);
  assert.deepEqual(await video.caughtFor(8), []);
});

test('two catches at once are both kept', async () => {
  const { chrome, video } = install();
  await settle();
  headers(chrome, streamResponse({ url: 'https://cdn.example.com/a.mp4' }));
  headers(chrome, streamResponse({ url: 'https://cdn.example.com/b.mp4' }));
  assert.deepEqual((await video.caughtFor(7)).map((e) => e.name).sort(), ['a.mp4', 'b.mp4']);
});

test('nothing is caught when the switch is off', async () => {
  const { chrome, video } = install({ local: { enabled: false } });
  await settle();
  headers(chrome, streamResponse());
  assert.deepEqual(await video.caughtFor(7), []);
});

test('turning the switch off clears every list', async () => {
  const { chrome, video } = install();
  await settle();
  headers(chrome, streamResponse());
  headers(chrome, streamResponse({ tabId: 9 }));
  await video.caughtFor(9);
  await chrome.storage.session.set({ okBadgeShownAt: 5 });
  await chrome.storage.local.set({ enabled: false });
  await settle();
  assert.deepEqual(await video.caughtFor(7), []);
  assert.deepEqual(await video.caughtFor(9), []);
  assert.deepEqual(Object.keys(chrome.storage.session.data), ['okBadgeShownAt']);
  headers(chrome, streamResponse());
  assert.deepEqual(await video.caughtFor(7), []);
});

test('a closed tab loses its list', async () => {
  const { chrome, video } = install();
  await settle();
  headers(chrome, streamResponse());
  await video.caughtFor(7);
  chrome.tabs.onRemoved.fire(7, {});
  assert.deepEqual(await video.caughtFor(7), []);
  assert.equal('caught:7' in chrome.storage.session.data, false);
});

test('moving to another page clears the list, a fragment change does not', async () => {
  const { chrome, video } = install();
  await settle();
  chrome.tabs.onUpdated.fire(7, { url: 'https://example.com/watch?v=1' }, {});
  headers(chrome, streamResponse());
  chrome.tabs.onUpdated.fire(7, { url: 'https://example.com/watch?v=1#t=30' }, {});
  assert.equal((await video.caughtFor(7)).length, 1);
  chrome.tabs.onUpdated.fire(7, { status: 'complete' }, {});
  assert.equal((await video.caughtFor(7)).length, 1);
  chrome.tabs.onUpdated.fire(7, { url: 'https://example.com/watch?v=2' }, {});
  assert.deepEqual(await video.caughtFor(7), []);
});

test('the stream listener is added once, again after the grant arrives', async () => {
  const { chrome } = install();
  chrome.permissions.onAdded.fire({ origins: ['<all_urls>'] });
  assert.equal(chrome.webRequest.onHeadersReceived.listeners.length, 1);
});

const nativeAdds = (chrome) => chrome.state.log.filter((e) => e[0] === 'native').map((e) => e[1]);
const tab = (over = {}) => ({ id: 7, url: 'https://blog.example.com/post', title: ' Angel One ', incognito: false, ...over });

test('the page menu sends the frame url with the tab title', async () => {
  const { chrome, video } = install();
  await video.menuPage({ menuItemId: 'rustdm-download-page', frameUrl: 'https://www.youtube.com/embed/x', pageUrl: 'https://blog.example.com/post' }, tab());
  const [add] = nativeAdds(chrome);
  assert.equal(add.type, 'add');
  assert.equal(add.url, 'https://www.youtube.com/embed/x');
  assert.equal(add.context.title, 'Angel One');
  assert.equal('referer' in add.context, false);
  assert.ok(chrome.state.log.some((e) => e[0] === 'ok'));
  await video.menuPage({ pageUrl: 'https://blog.example.com/post' }, tab());
  assert.equal(nativeAdds(chrome)[1].url, 'https://blog.example.com/post');
});

test('the page menu reports a refusal and skips private windows', async () => {
  const { chrome, video } = install();
  chrome.state.nativeReply = { id: 1, ok: false, error: { kind: 'rejected', message: 'no' } };
  await video.menuPage({ pageUrl: 'https://e.com/' }, tab());
  assert.deepEqual(chrome.state.log.find((e) => e[0] === 'failure')[1].kind, 'rejected');
  await video.menuPage({ pageUrl: 'https://e.com/' }, tab({ incognito: true }));
  assert.equal(nativeAdds(chrome).length, 1);
});

test('the page menu is shown only while the switch is on', async () => {
  const { chrome } = install({ local: { enabled: false } });
  await settle();
  const updates = () => chrome.state.log.filter((e) => e[0] === 'menu.update');
  assert.deepEqual(updates().at(-1), ['menu.update', 'rustdm-download-page', { visible: false }]);
  await chrome.storage.local.set({ enabled: true });
  await settle();
  assert.deepEqual(updates().at(-1), ['menu.update', 'rustdm-download-page', { visible: true }]);
});

test('the blob menu uses the caught stream', async () => {
  const { chrome, video } = install();
  await settle();
  headers(chrome, streamResponse({ url: 'https://cdn.example.com/master.m3u8', type: 'xmlhttprequest', frameId: 2, responseHeaders: [{ name: 'content-type', value: 'application/x-mpegurl' }] }));
  await video.caughtFor(7);
  await video.menuBlob({ srcUrl: 'blob:https://player.example.com/1', mediaType: 'video', frameId: 2, frameUrl: 'https://player.example.com/embed', pageUrl: 'https://blog.example.com/post' }, tab());
  const [add] = nativeAdds(chrome);
  assert.equal(add.url, 'https://cdn.example.com/master.m3u8');
  assert.equal(add.context.referer, 'https://player.example.com/embed');
  assert.equal(add.context.title, 'Angel One');
});

test('the blob menu without a catch looks up the page', async () => {
  const { chrome, video } = install();
  await video.menuBlob({ srcUrl: 'blob:https://e.com/1', mediaType: 'video', frameId: 0, pageUrl: 'https://e.com/watch' }, tab());
  const [add] = nativeAdds(chrome);
  assert.equal(add.url, 'https://e.com/watch');
  assert.equal('referer' in add.context, false);
});

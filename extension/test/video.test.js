// video.js against a fake `chrome` (test/fake-chrome.js).
import assert from 'node:assert/strict';
import test from 'node:test';
import { installVideo } from '../video.js';
import { fakePort, makeChrome, settle } from './fake-chrome.js';

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

const cardSender = (over = {}) => ({
  id: 'rustdm-extension-id',
  url: 'https://e.com/watch?v=1',
  frameId: 0,
  tab: { id: 7, url: 'https://e.com/watch?v=1', title: 'Angel One', incognito: false },
  ...over,
});

async function openCard(chrome, { sender = cardSender(), message = { type: 'open', currentSrc: '' } } = {}) {
  const card = fakePort({ name: 'rdm-card', sender });
  chrome.runtime.onConnect.fire(card);
  card.deliver(message);
  await settle();
  return card;
}

const lastNative = (chrome) => chrome.state.nativePorts.at(-1);
const answer = async (native, index, reply) => {
  native.deliver({ id: native.posted[index].id, ...reply });
  await settle();
};
const views = (card) => card.posted.filter((m) => m.type === 'view').map((m) => m.view);

test('a card runs add then status on one native port', async () => {
  const { chrome, tick } = install();
  const card = await openCard(chrome);
  const native = lastNative(chrome);
  assert.equal(native.name, 'rustdm.nmhost');
  assert.equal(native.posted[0].type, 'add');
  assert.equal(native.posted[0].url, 'https://e.com/watch?v=1');
  await answer(native, 0, { ok: true, result: { download_id: 'dl_1' } });
  assert.deepEqual(native.posted[1], { type: 'status', download_id: 'dl_1', id: 2 });
  await answer(native, 1, { ok: true, result: { status: 'active', filename: 'watch' } });
  await tick();
  assert.equal(native.posted[2].type, 'status');
  await answer(native, 2, {
    ok: true,
    result: { status: 'choosing', filename: 'w', video: { title: 'A', choices: [{ id: '720-mp4', label: '720p', container: 'MP4', size_is_estimate: false }], default: 0 } },
  });
  assert.equal(views(card).at(-1).phase, 'choosing');
  card.deliver({ type: 'choose', choiceId: '720-mp4' });
  await settle();
  assert.deepEqual(native.posted.at(-1), { type: 'choose_quality', download_id: 'dl_1', choice_id: '720-mp4', id: 4 });
  await answer(native, 3, { ok: true, result: { status: 'queued', filename: 'A.mp4', video: { title: 'A', choices: [] } } });
  assert.deepEqual(views(card).at(-1), { phase: 'started', filename: 'A.mp4', title: 'Angel One', others: [] });
  assert.equal(chrome.state.nativePorts.length, 1);
  assert.equal(native.disconnected, true, 'stopped after starting');
});

test('the add carries the title, referer and cookies', async () => {
  const { chrome } = install();
  chrome.state.cookies = [{ name: 'SID', value: 'v', domain: '.cdn.e.com', path: '/', secure: true, hostOnly: false }];
  await openCard(chrome, {
    sender: cardSender({ url: 'https://player.e.com/embed/1', frameId: 3 }),
    message: { type: 'open', currentSrc: 'https://cdn.e.com/v.mp4' },
  });
  const add = lastNative(chrome).posted[0];
  assert.equal(add.url, 'https://cdn.e.com/v.mp4');
  assert.equal(add.context.referer, 'https://player.e.com/embed/1');
  assert.equal(add.context.title, 'Angel One');
  assert.equal(add.context.cookies[0].name, 'SID');
  assert.deepEqual(chrome.state.log.find((e) => e[0] === 'cookies')[1], ['https://cdn.e.com/v.mp4', 'https://e.com/watch?v=1']);
});

test('a blob video in the card uses the stream caught in its frame', async () => {
  const { chrome, video } = install();
  await settle();
  headers(chrome, streamResponse({ url: 'https://cdn.e.com/master.m3u8', type: 'xmlhttprequest', responseHeaders: [{ name: 'content-type', value: 'application/x-mpegurl' }] }));
  await video.caughtFor(7);
  const card = await openCard(chrome, { message: { type: 'open', currentSrc: 'blob:https://e.com/1' } });
  assert.equal(lastNative(chrome).posted[0].url, 'https://cdn.e.com/master.m3u8');
  assert.deepEqual(views(card)[0].others, [{ key: 'page', label: 'This page' }]);
});

test('an incognito sender is refused', async () => {
  const { chrome } = install();
  const sender = cardSender();
  sender.tab.incognito = true;
  const card = await openCard(chrome, { sender });
  assert.equal(chrome.state.nativePorts.length, 0);
  assert.deepEqual(views(card), [{ phase: 'failed', message: 'rustdm could not use this video.', title: 'e.com', others: [] }]);
});

test('a caught id from another tab is refused', async () => {
  const { chrome } = install();
  const card = await openCard(chrome, { message: { type: 'use', source: { type: 'caught', id: 'abcd1234' } } });
  assert.equal(chrome.state.nativePorts.length, 0);
  assert.equal(views(card)[0].message, 'rustdm could not use this video.');
});

test('closing the card stops polling and the native port', async () => {
  const { chrome, timers } = install();
  const card = await openCard(chrome);
  const native = lastNative(chrome);
  await answer(native, 0, { ok: true, result: { download_id: 'dl_1' } });
  assert.equal(timers.size, 1);
  card.drop();
  assert.equal(timers.size, 0);
  assert.equal(native.disconnected, true);
});

test('a lost native port tells the card the download is still there', async () => {
  const { chrome } = install();
  const card = await openCard(chrome);
  const native = lastNative(chrome);
  await answer(native, 0, { ok: true, result: { download_id: 'dl_1' } });
  native.drop();
  await settle();
  assert.equal(views(card).at(-1).message, "Lost the connection to rustdm. The download is still in rustdm's list.");
});

test('a new source drops the old session', async () => {
  const { chrome, timers } = install();
  const card = await openCard(chrome);
  const first = lastNative(chrome);
  card.deliver({ type: 'use', source: { type: 'page' } });
  await settle();
  assert.equal(first.disconnected, true);
  assert.equal(chrome.state.nativePorts.length, 2);
  const before = card.posted.length;
  first.deliver({ id: 1, ok: true, result: { download_id: 'dl_old' } });
  await settle();
  assert.equal(card.posted.length, before, 'a late reply from the old session changes nothing');
  assert.equal(timers.size, 1);
  const second = lastNative(chrome);
  await answer(second, 0, { ok: true, result: { download_id: 'dl_new' } });
  assert.equal(second.posted[1].download_id, 'dl_new');
});

test('views carry the card title and other sources', async () => {
  const { chrome, video } = install();
  await settle();
  headers(chrome, streamResponse());
  await video.caughtFor(7);
  const card = await openCard(chrome, { message: { type: 'open', currentSrc: 'https://cdn.e.com/own.mp4' } });
  const [first] = views(card);
  assert.equal(first.phase, 'looking');
  assert.equal(first.title, 'Angel One');
  assert.deepEqual(first.others.map((o) => o.label), ['movie.mp4 · cdn.example.com', 'This page']);
});

test('a port from another extension is ignored', async () => {
  const { chrome } = install();
  const card = await openCard(chrome, { sender: cardSender({ id: 'someone-else' }) });
  assert.equal(chrome.state.nativePorts.length, 0);
  assert.deepEqual(card.posted, []);
  const other = fakePort({ name: 'other', sender: cardSender() });
  chrome.runtime.onConnect.fire(other);
  other.deliver({ type: 'open', currentSrc: '' });
  await settle();
  assert.equal(chrome.state.nativePorts.length, 0);
});

const registrations = (chrome) => chrome.state.log.filter((e) => e[0] === 'register' || e[0] === 'unregister');

test('the page script is registered with the grant and both switches', async () => {
  const { chrome } = install();
  await settle();
  await settle();
  assert.deepEqual(registrations(chrome), [['register', ['rdm-hover']]]);
  const [script] = chrome.state.registered;
  assert.deepEqual(script, {
    id: 'rdm-hover',
    js: ['content/ember.css.js', 'content/hover-logic.js', 'content/hover.js'],
    matches: ['<all_urls>'],
    allFrames: true,
    runAt: 'document_idle',
  });
});

test('it is not registered without the grant, and is once the grant arrives', async () => {
  const { chrome } = install({ granted: false });
  await settle();
  assert.deepEqual(registrations(chrome), []);
  chrome.state.granted = true;
  chrome.permissions.onAdded.fire({ origins: ['<all_urls>'] });
  await settle();
  await settle();
  assert.deepEqual(registrations(chrome), [['register', ['rdm-hover']]]);
});

test('it is removed when either switch goes off', async () => {
  for (const key of ['enabled', 'videoBar']) {
    const { chrome } = install();
    await settle();
    await settle();
    await chrome.storage.local.set({ [key]: false });
    await settle();
    await settle();
    assert.deepEqual(registrations(chrome).at(-1), ['unregister', ['rdm-hover']], key);
    assert.deepEqual(chrome.state.registered, []);
  }
});

test('it is removed when the grant is removed', async () => {
  const { chrome } = install();
  await settle();
  await settle();
  chrome.state.granted = false;
  chrome.permissions.onRemoved.fire({ origins: ['<all_urls>'] });
  await settle();
  await settle();
  assert.deepEqual(chrome.state.registered, []);
});

test('registering twice is avoided', async () => {
  const { chrome } = install();
  await settle();
  await settle();
  chrome.permissions.onAdded.fire({ origins: ['<all_urls>'] });
  await chrome.storage.local.set({ videoBar: true });
  await settle();
  await settle();
  assert.equal(registrations(chrome).length, 1);
  assert.equal(chrome.state.registered.length, 1);
});

test('the bar switch alone is off: nothing registered at start', async () => {
  const { chrome } = install({ local: { videoBar: false } });
  await settle();
  await settle();
  assert.deepEqual(registrations(chrome), []);
});

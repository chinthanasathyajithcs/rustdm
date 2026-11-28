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
  // Like the browser's timers: called as a method of another object they throw
  // "Illegal invocation" (Node's do not, which once hid a real bug).
  function browserTimer(action) {
    return function timer(...args) {
      if (this !== undefined && this !== globalThis) throw new TypeError('Illegal invocation');
      return action(...args);
    };
  }
  const video = installVideo({
    chrome,
    setInterval: browserTimer((fn) => {
      timers.set(nextTimer, fn);
      return nextTimer++;
    }),
    clearInterval: browserTimer((id) => timers.delete(id)),
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
  await video.menuBlob({ srcUrl: 'blob:https://e.com/1', mediaType: 'video', frameId: 0, pageUrl: 'https://e.com/watch' }, tab({ url: 'https://e.com/watch' }));
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

test('only media, script and other responses reach the worker', async () => {
  const { chrome } = install();
  const [[filter, extra]] = chrome.webRequest.onHeadersReceived.options;
  assert.deepEqual(filter, { urls: ['<all_urls>'], types: ['media', 'xmlhttprequest', 'other'] });
  assert.deepEqual(extra, ['responseHeaders']);
});

test('a list caught before any navigation is cleared when the tab moves on', async () => {
  const { chrome, video } = install();
  chrome.state.tabUrls = { 7: 'https://e.com/a#top', 8: 'https://e.com/b' };
  await settle();
  headers(chrome, streamResponse());
  headers(chrome, streamResponse({ tabId: 8 }));
  assert.equal((await video.caughtFor(7)).length, 1);
  chrome.tabs.onUpdated.fire(7, { url: 'https://www.youtube.com/watch?v=x' }, {});
  assert.deepEqual(await video.caughtFor(7), []);
  chrome.tabs.onUpdated.fire(8, { url: 'https://e.com/b#comments' }, {});
  assert.equal((await video.caughtFor(8)).length, 1, 'same document');
});

test('a catch for a tab the browser cannot name still counts', async () => {
  const { chrome, video } = install();
  await settle();
  headers(chrome, streamResponse({ tabId: 12 }));
  assert.equal((await video.caughtFor(12)).length, 1);
});

test('a card is refused while either switch is off', async () => {
  for (const key of ['enabled', 'videoBar']) {
    const { chrome } = install({ local: { [key]: false } });
    await settle();
    const card = await openCard(chrome);
    await settle();
    assert.equal(chrome.state.nativePorts.length, 0, key);
    assert.equal(card.disconnected, true, key);
  }
});

test('an unexpected error in the card shows a failure instead of looking forever', async () => {
  const chrome = makeChrome();
  installVideo({
    chrome,
    setInterval: () => {
      throw new Error('timer broke');
    },
    clearInterval: () => {},
    now: () => 0,
    collectCookies: async () => [],
    flashOk: async () => {},
    showFailure: async () => {},
  });
  const card = await openCard(chrome);
  await settle();
  const [view] = views(card);
  assert.equal(view?.phase, 'failed');
  assert.match(view.message, /rustdm could not use this video|went wrong/i);
  assert.equal(chrome.state.nativePorts.every((p) => p.disconnected), true, 'no native port left open');
});

test('the page menu in the main frame uses the tab address', async () => {
  const { chrome, video } = install();
  await video.menuPage({ frameId: 0, pageUrl: 'https://www.youtube.com/' }, tab({ url: 'https://www.youtube.com/shorts/abc123' }));
  assert.equal(nativeAdds(chrome)[0].url, 'https://www.youtube.com/shorts/abc123');
});

test('a card in the main frame looks up the tab address', async () => {
  const { chrome } = install();
  const moved = cardSender({ url: 'https://www.youtube.com/' });
  moved.tab.url = 'https://www.youtube.com/shorts/abc123';
  await openCard(chrome, { sender: moved });
  assert.equal(lastNative(chrome).posted[0].url, 'https://www.youtube.com/shorts/abc123');
});

const youtubeSender = (url = 'https://www.youtube.com/') => {
  const s = cardSender({ url });
  s.tab.url = url;
  s.tab.title = 'YouTube';
  return s;
};
const openOn = (extra = {}) => ({ type: 'open', currentSrc: 'blob:https://www.youtube.com/1', at: { x: 300, y: 200 }, videos: 8, ...extra });

test('on YouTube the card asks the player which video it plays', async () => {
  const { chrome } = install();
  chrome.state.playerId = 'abcdefghijk';
  await openCard(chrome, { sender: youtubeSender(), message: openOn() });
  const call = chrome.state.log.find((e) => e[0] === 'executeScript')[1];
  assert.deepEqual(call, { target: { tabId: 7, frameIds: [0] }, world: 'MAIN', args: [300, 200] });
  assert.equal(lastNative(chrome).posted[0].url, 'https://www.youtube.com/watch?v=abcdefghijk');
});

test('a YouTube home preview with no answer and no link is refused, never sent as a list', async () => {
  const { chrome } = install();
  chrome.state.playerFails = true;
  const card = await openCard(chrome, { sender: youtubeSender(), message: openOn() });
  assert.equal(chrome.state.nativePorts.length, 0);
  assert.equal(views(card)[0].phase, 'failed');
  assert.equal(views(card)[0].message, 'Open the video you want, then try again.');
});

test('a YouTube preview without a player answer uses the link of its card', async () => {
  const { chrome } = install();
  await openCard(chrome, { sender: youtubeSender(), message: openOn({ link: 'https://www.youtube.com/watch?v=zzzzzzzzzzz' }) });
  assert.equal(lastNative(chrome).posted[0].url, 'https://www.youtube.com/watch?v=zzzzzzzzzzz');
});

test('a preview inside a link on another site uses that link', async () => {
  const { chrome } = install();
  const s = cardSender({ url: 'https://news.example.com/' });
  s.tab.url = 'https://news.example.com/';
  await openCard(chrome, { sender: s, message: { type: 'open', currentSrc: '', at: { x: 1, y: 1 }, videos: 3, link: 'https://news.example.com/clips/42' } });
  assert.equal(lastNative(chrome).posted[0].url, 'https://news.example.com/clips/42');
  assert.equal(chrome.state.log.some((e) => e[0] === 'executeScript'), false, 'only YouTube players are asked');
});

test('a refused card still offers the streams caught in the tab', async () => {
  const { chrome, video } = install();
  await settle();
  headers(chrome, streamResponse({ url: 'https://cdn.example.com/other.mp4', frameId: 5 }));
  await video.caughtFor(7);
  chrome.state.playerFails = true;
  const card = await openCard(chrome, { sender: youtubeSender(), message: openOn() });
  assert.deepEqual(views(card)[0].others, [{ key: views(card)[0].others[0].key, label: 'other.mp4 · cdn.example.com' }]);
});

test('a card in a hidden tab stops asking rustdm until shown again', async () => {
  const { chrome, tick } = install();
  const card = await openCard(chrome);
  const native = lastNative(chrome);
  await answer(native, 0, { ok: true, result: { download_id: 'dl_1' } });
  await answer(native, 1, { ok: true, result: { status: 'active', filename: 'watch' } });
  card.deliver({ type: 'visible', visible: false });
  await tick();
  await tick();
  assert.equal(native.posted.length, 2, 'add and the first status only');
  card.deliver({ type: 'visible', visible: true });
  await tick();
  assert.equal(native.posted.at(-1).type, 'status');
});

test('a stream fetched again does not rewrite the tab list', async () => {
  const { chrome, video } = install();
  chrome.state.tabUrls = { 7: 'https://e.com/live' };
  await settle();
  let writes = 0;
  const realSet = chrome.storage.session.set;
  chrome.storage.session.set = async (values) => {
    writes += 1;
    return realSet(values);
  };
  const playlist = streamResponse({ url: 'https://cdn.e.com/live.m3u8', type: 'xmlhttprequest', responseHeaders: [{ name: 'content-type', value: 'application/x-mpegurl' }] });
  headers(chrome, playlist);
  await video.caughtFor(7);
  headers(chrome, playlist);
  headers(chrome, playlist);
  await video.caughtFor(7);
  assert.equal(writes, 1);
});

// The hover lookup (spec 1f): a peek before the click, then a quick or list open.
const looked = {
  title: 'Angel One',
  duration_secs: 60,
  choices: [
    { id: '1080-mp4', label: '1080p', container: 'MP4', height: 1080, size_bytes: 52 * MiB, size_is_estimate: false },
    { id: '720-mp4', label: '720p', container: 'MP4', height: 720, size_bytes: 20 * MiB, size_is_estimate: false },
  ],
  default: 0,
};

async function peek(chrome, { sender = cardSender(), message = { type: 'peek', currentSrc: '' } } = {}) {
  const replies = [];
  const kept = chrome.runtime.onMessage.fire(message, sender, (reply) => replies.push(reply));
  assert.deepEqual(kept, [true], 'the answer comes later');
  await settle();
  return replies;
}

const lookUps = (chrome) => chrome.state.nativePorts.filter((p) => p.posted[0]?.type === 'look_up');

test('a peek looks the video up once and answers the label', async () => {
  const { chrome } = install();
  const replies = await peek(chrome);
  const [native] = lookUps(chrome);
  assert.equal(native.posted[0].url, 'https://e.com/watch?v=1');
  assert.equal(native.posted[0].context.title, 'Angel One');
  await answer(native, 0, { ok: true, result: looked });
  assert.deepEqual(replies, [looked]);
  assert.equal(native.disconnected, true);
});

test('a second peek of the same video sends nothing', async () => {
  const { chrome } = install();
  await peek(chrome);
  await answer(lookUps(chrome)[0], 0, { ok: true, result: looked });
  const again = await peek(chrome);
  assert.deepEqual(again, [looked]);
  assert.equal(lookUps(chrome).length, 1);
});

test('a peek of a plain file sends nothing', async () => {
  const { chrome } = install();
  const replies = await peek(chrome, { message: { type: 'peek', currentSrc: 'https://cdn.e.com/own.mp4' } });
  assert.deepEqual(replies, [null]);
  assert.equal(chrome.state.nativePorts.length, 0);
});

test('a peek is refused while the bar is switched off', async () => {
  const { chrome } = install({ local: { videoBar: false } });
  const replies = await peek(chrome);
  assert.deepEqual(replies, [null]);
  assert.equal(chrome.state.nativePorts.length, 0);
});

test('a quick open after the answer adds with the default choice and no lookup', async () => {
  const { chrome } = install();
  await peek(chrome);
  await answer(lookUps(chrome)[0], 0, { ok: true, result: looked });
  await openCard(chrome, { message: { type: 'open', currentSrc: '', mode: 'quick' } });
  const add = lastNative(chrome).posted[0];
  assert.equal(add.type, 'add');
  assert.equal(add.choice_id, '1080-mp4');
  assert.equal(add.page, true);
  assert.equal(lookUps(chrome).length, 1);
});

test('a quick open before the answer waits for it', async () => {
  const { chrome } = install();
  await peek(chrome);
  await openCard(chrome, { message: { type: 'open', currentSrc: '', mode: 'quick' } });
  assert.equal(chrome.state.nativePorts.length, 1, 'only the lookup so far');
  await answer(lookUps(chrome)[0], 0, { ok: true, result: looked });
  const add = lastNative(chrome).posted[0];
  assert.equal(add.type, 'add');
  assert.equal(add.choice_id, '1080-mp4');
});

test('a quick open whose lookup failed starts as before', async () => {
  const { chrome } = install();
  await openCard(chrome, { message: { type: 'open', currentSrc: '', mode: 'quick' } });
  const [native] = lookUps(chrome);
  await answer(native, 0, { ok: false, error: { kind: 'lookup_failed', message: 'cancelled' } });
  const add = lastNative(chrome).posted[0];
  assert.equal(add.type, 'add');
  assert.equal('choice_id' in add, false);
});

test('a list open with an answer shows the list at once', async () => {
  const { chrome } = install();
  await peek(chrome);
  await answer(lookUps(chrome)[0], 0, { ok: true, result: looked });
  const card = await openCard(chrome, { message: { type: 'open', currentSrc: '', mode: 'list' } });
  assert.equal(views(card)[0].phase, 'choosing');
  assert.equal(views(card)[0].choices.length, 2);
  assert.deepEqual(lastNative(chrome).posted, [], 'nothing added yet');
  card.deliver({ type: 'choose', choiceId: '720-mp4' });
  await settle();
  assert.equal(lastNative(chrome).posted[0].choice_id, '720-mp4');
});

test('a new page in the tab drops its answers', async () => {
  const { chrome } = install();
  await peek(chrome);
  await answer(lookUps(chrome)[0], 0, { ok: true, result: looked });
  chrome.tabs.onUpdated.fire(7, { url: 'https://e.com/watch?v=2' }, {});
  await peek(chrome);
  assert.equal(lookUps(chrome).length, 2);
});

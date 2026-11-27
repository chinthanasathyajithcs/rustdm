// Runs background.js against a fake `chrome` to check the hand-off sequence.
import assert from 'node:assert/strict';
import test from 'node:test';

const listeners = {};
const log = [];
const store = {};
const session = {};
const state = {
  granted: true,
  reply: { id: 1, ok: true, result: { download_id: 'dl_1' } },
  cookies: [],
  cancelFails: false,
  downloadFails: false,
  badge: '',
};

const event = (name) => ({
  addListener: (fn) => {
    listeners[name] = fn;
  },
  hasListener: (fn) => listeners[name] === fn,
});

globalThis.chrome = {
  downloads: {
    onDeterminingFilename: event('determining'),
    cancel: async (id) => {
      if (state.cancelFails) throw new Error('Invalid download id');
      log.push(['cancel', id]);
    },
    erase: async (q) => log.push(['erase', q.id]),
    download: async (o) => {
      log.push(['download', o.url]);
      if (state.downloadFails) throw new Error('Invalid URL');
    },
  },
  contextMenus: { onClicked: event('menu'), create: (o) => log.push(['menu', o.id]) },
  runtime: {
    onInstalled: event('installed'),
    openOptionsPage: () => log.push(['options']),
    sendNativeMessage: async (host, message) => {
      log.push(['native', message.type, message.url, JSON.stringify(message.context)]);
      return state.reply;
    },
  },
  permissions: { contains: async () => state.granted, onAdded: event('added') },
  storage: {
    local: {
      get: async (defaults) => ({ ...(typeof defaults === 'object' ? defaults : {}), ...store }),
      set: async (o) => Object.assign(store, o),
      remove: async (k) => delete store[k],
    },
    session: {
      get: async (k) => (k in session ? { [k]: session[k] } : {}),
      set: async (o) => Object.assign(session, o),
      remove: async (k) => delete session[k],
    },
  },
  cookies: { getAll: async () => state.cookies },
  action: {
    setBadgeText: async (o) => {
      state.badge = o.text;
      log.push(['badge', o.text]);
    },
    setBadgeBackgroundColor: async () => {},
    getBadgeText: async () => state.badge,
  },
  webRequest: { onBeforeRequest: event('post'), onHeadersReceived: event('headers') },
  tabs: { onUpdated: event('tabUpdated'), onRemoved: event('tabRemoved') },
};
chrome.storage.onChanged = event('storageChanged');

await import('../background.js');

const item = {
  id: 9,
  url: 'https://example.com/get',
  finalUrl: 'https://example.com/x.zip',
  mime: 'application/zip',
  totalBytes: 5 * 1024 * 1024,
  referrer: 'https://example.com/',
  filename: '/dl/x.zip',
};

function offer(over = {}) {
  log.length = 0;
  let suggested = 0;
  const returned = listeners.determining({ ...item, ...over }, () => {
    suggested += 1;
    log.push(['suggest']);
  });
  return { returned, suggested: () => suggested, done: () => new Promise((r) => setTimeout(r, 50)) };
}

test('all the browser events are wired up', () => {
  for (const name of ['determining', 'menu', 'installed', 'added', 'post']) {
    assert.equal(typeof listeners[name], 'function', name);
  }
});

test('a taken download is sent, cancelled, erased and badged', async () => {
  state.reply = { id: 1, ok: true, result: { download_id: 'dl_1' } };
  state.cookies = [{ name: 'SID', value: 'v', domain: 'example.com', path: '/', secure: true, hostOnly: true, session: true }];
  const run = offer();
  assert.equal(run.returned, true, 'holds the download until it decides');
  await run.done();
  const steps = log.map((entry) => entry[0]);
  assert.deepEqual(steps.slice(0, 2), ['native', 'cancel']);
  assert.ok(steps.includes('erase') && steps.includes('badge'));
  assert.equal(run.suggested(), 1);
  const native = log.find((entry) => entry[0] === 'native');
  assert.equal(native[2], 'https://example.com/x.zip');
  assert.match(native[3], /"SID"/);
  assert.ok(log.some((e) => e[0] === 'badge' && e[1] === 'OK'));
});

test('a duplicate counts as handed over', async () => {
  state.reply = { id: 1, ok: false, error: { kind: 'duplicate', message: 'dup' } };
  const run = offer();
  await run.done();
  assert.ok(log.some((e) => e[0] === 'cancel'));
});

test('a failure leaves the browser download alone and shows the red badge', async () => {
  state.reply = { id: 1, ok: false, error: { kind: 'not_running', message: 'no' } };
  const run = offer();
  await run.done();
  assert.ok(!log.some((e) => e[0] === 'cancel' || e[0] === 'erase'));
  assert.ok(log.some((e) => e[0] === 'badge' && e[1] === '!'));
  assert.equal(run.suggested(), 1);
  assert.match(store.lastError.message, /rdm daemon start/);
});

test('a download that is not taken over is released at once without a native call', async () => {
  const run = offer({ incognito: true });
  await run.done();
  assert.ok(!log.some((e) => e[0] === 'native'));
  assert.equal(run.suggested(), 1);
});

test('the context menu falls back to the browser when rustdm cannot take the link', async () => {
  state.reply = { id: 1, ok: false, error: { kind: 'host_missing', message: 'x' } };
  log.length = 0;
  listeners.installed({ reason: 'update' });
  listeners.menu({ menuItemId: 'rustdm-download-link', linkUrl: 'https://example.com/a.zip', pageUrl: 'https://example.com/' });
  await new Promise((r) => setTimeout(r, 50));
  assert.ok(log.some((e) => e[0] === 'native'));
  assert.deepEqual(log.find((e) => e[0] === 'download'), ['download', 'https://example.com/a.zip']);
});

test('if the browser cannot stop its own copy, nothing is erased and no OK is shown', async () => {
  state.reply = { id: 1, ok: true, result: { download_id: 'dl_1' } };
  state.cancelFails = true;
  const run = offer();
  await run.done();
  state.cancelFails = false;
  assert.ok(!log.some((e) => e[0] === 'erase'));
  assert.ok(!log.some((e) => e[0] === 'badge' && e[1] === 'OK'));
  assert.ok(log.some((e) => e[0] === 'badge' && e[1] === '!'));
  assert.equal(run.suggested(), 1, 'the browser keeps its download');
  assert.match(store.lastError.message, /twice/);
});

test('the context menu does nothing for a private window tab', async () => {
  log.length = 0;
  listeners.menu(
    { menuItemId: 'rustdm-download-link', linkUrl: 'https://example.com/a.zip' },
    { incognito: true },
  );
  await new Promise((r) => setTimeout(r, 50));
  assert.deepEqual(log, []);
});

test('a browser fallback that fails is reported instead of lost', async () => {
  state.reply = { id: 1, ok: false, error: { kind: 'host_missing', message: 'x' } };
  state.downloadFails = true;
  log.length = 0;
  listeners.menu(
    { menuItemId: 'rustdm-download-link', linkUrl: 'https://example.com/a.zip' },
    { incognito: false },
  );
  await new Promise((r) => setTimeout(r, 50));
  state.downloadFails = false;
  assert.ok(log.some((e) => e[0] === 'download'));
  assert.match(store.lastError.message, /browser could not start/);
});

test('an OK badge older than 4 s is cleared by the next event, a fresh one is not', async () => {
  state.badge = 'OK';
  session.okBadgeShownAt = Date.now() - 1000;
  await offer({ incognito: true }).done();
  assert.equal(state.badge, 'OK', 'still fresh');

  session.okBadgeShownAt = Date.now() - 5000;
  await offer({ incognito: true }).done();
  assert.equal(state.badge, '');
  assert.ok(!('okBadgeShownAt' in session));
});

test('clearing a stale OK never wipes a red badge', async () => {
  state.badge = '!';
  session.okBadgeShownAt = Date.now() - 5000;
  await offer({ incognito: true }).done();
  assert.equal(state.badge, '!');
});

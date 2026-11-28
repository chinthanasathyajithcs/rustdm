// Browser wiring for video downloads (spec Part 1e). The decisions live in
// lib/catch.js, lib/source.js and lib/flow.js; this file only connects them to
// Chrome. `chrome` and the timers are passed in so tests can use fakes.

import { addCaught, classify, documentKey, tabRecordAfterNavigation } from './lib/catch.js';
import { buildChooseMessage, buildStatusMessage, buildVideoMessage } from './lib/contract.js';
import { POLL_MS, flowStep, startFlow } from './lib/flow.js';
import { openSession, request } from './lib/native.js';
import { frameAddress, isFeedPage, otherSources, pickSource, refinePage, resolveSource } from './lib/source.js';

const ALL_URLS = { origins: ['<all_urls>'] };
const CAUGHT_PREFIX = 'caught:';
export const PAGE_MENU_ID = 'rustdm-download-page';
export const CARD_PORT = 'rdm-card';
// Players load streams as media, by script (fetch / XHR) or as "other";
// images, fonts, styles and pages never reach the worker.
const STREAM_TYPES = ['media', 'xmlhttprequest', 'other'];
const HOVER_SCRIPT = {
  id: 'rdm-hover',
  js: ['content/ember.css.js', 'content/hover-logic.js', 'content/hover.js'],
  matches: ['<all_urls>'],
  allFrames: true,
  runAt: 'document_idle',
};
const UNUSABLE = { kind: 'unusable', message: 'rustdm could not use this video.' };
const BROKE = 'Something went wrong. Close the card and try again.';
const FEED = 'Open the video you want, then try again.';
const PLAYER_WAIT_MS = 3000;
const isYouTube = (address) => {
  try {
    return /(^|\.)(youtube\.com|youtube-nocookie\.com)$/.test(new URL(address).hostname);
  } catch {
    return false;
  }
};

// Runs in the page's own world (its player's API lives there): the id of the
// video the YouTube player under the point (x, y) plays, or "".
function playerVideoAt(x, y) {
  let found = null;
  for (const video of document.querySelectorAll('video')) {
    const box = video.getBoundingClientRect();
    const hit = box.width > 0 && box.height > 0 && x >= box.left && x <= box.right && y >= box.top && y <= box.bottom;
    if (hit && (!found || box.width * box.height < found.area)) found = { video, area: box.width * box.height };
  }
  try {
    return String(found?.video.closest('.html5-video-player')?.getVideoData?.().video_id ?? '');
  } catch {
    return '';
  }
}

const caughtKey = (tabId) => `${CAUGHT_PREFIX}${tabId}`;
const newId = () => crypto.randomUUID().replaceAll('-', '').slice(0, 8);

/**
 * @param deps `{chrome, setInterval, clearInterval, now, collectCookies, flashOk, showFailure}`
 * @returns `{caughtFor, menuPage, menuBlob}`
 */
export function installVideo(deps) {
  const { chrome, now, collectCookies, flashOk, showFailure } = deps;
  // Taken out of `deps` so they run as plain functions: a browser's timers
  // throw "Illegal invocation" when called as a method of another object.
  const startTimer = deps.setInterval;
  const stopTimer = deps.clearInterval;
  const tabWrites = new Map();
  let enabled = readEnabled(chrome);
  let menuMessageId = 1;

  // One write at a time per tab, so two catches cannot overwrite each other.
  function updateTab(tabId, change) {
    const key = caughtKey(tabId);
    const previous = tabWrites.get(tabId) ?? Promise.resolve();
    const next = previous
      .then(async () => {
        const stored = await chrome.storage.session.get(key);
        const record = await change(stored[key]);
        if (record === undefined) await chrome.storage.session.remove(key);
        else if (record !== stored[key]) await chrome.storage.session.set({ [key]: record });
      })
      .catch(() => {
        // Session storage unavailable: nothing is remembered for this tab.
      });
    tabWrites.set(tabId, next);
    void next.then(() => {
      if (tabWrites.get(tabId) === next) tabWrites.delete(tabId);
    });
    return next;
  }

  async function pageOf(tabId) {
    try {
      const tab = await chrome.tabs.get(tabId);
      return tab?.url ? documentKey(tab.url) : undefined;
    } catch {
      return undefined; // the tab is gone or not ours to see
    }
  }

  async function caughtFor(tabId) {
    await tabWrites.get(tabId);
    const stored = await chrome.storage.session.get(caughtKey(tabId)).catch(() => ({}));
    return stored[caughtKey(tabId)]?.list ?? [];
  }

  async function clearAll() {
    const everything = await chrome.storage.session.get(null).catch(() => ({}));
    const keys = Object.keys(everything).filter((k) => k.startsWith(CAUGHT_PREFIX));
    if (keys.length > 0) await chrome.storage.session.remove(keys).catch(() => {});
  }

  function onHeaders(details) {
    const caught = classify(details);
    if (!caught) return;
    void updateTab(details.tabId, async (record) => {
      if (!(await enabled)) return record;
      // A list started without a navigation (just installed, reloaded, or
      // switched back on) learns its page now, so the next page clears it.
      const doc = record?.doc ?? (await pageOf(details.tabId));
      const list = addCaught(record?.list ?? [], caught, { id: newId(), now: now() });
      if (record && list === record.list && doc === record.doc) return record; // unchanged: no write
      return { ...record, doc, list };
    });
  }

  function watchStreams() {
    try {
      if (!chrome.webRequest.onHeadersReceived.hasListener(onHeaders)) {
        chrome.webRequest.onHeadersReceived.addListener(onHeaders, { urls: ['<all_urls>'], types: STREAM_TYPES }, [
          'responseHeaders',
        ]);
      }
    } catch {
      // No host permission yet; tried again when it is granted.
    }
  }

  // Right-click: one message, today's badge feedback.
  async function sendFromMenu(resolved, tab) {
    if (!resolved.ok) {
      await showFailure(UNUSABLE);
      return;
    }
    const cookies = await collectCookies([resolved.url, tab.url]);
    const message = {
      ...buildVideoMessage({ ...resolved, cookies, userAgent: globalThis.navigator?.userAgent }),
      id: menuMessageId++,
    };
    const reply = await request(message, { send: (host, m) => chrome.runtime.sendNativeMessage(host, m) });
    if (reply.ok || reply.error?.kind === 'duplicate') await flashOk();
    else await showFailure(reply.error);
  }

  // Chrome gives `frameUrl` only for a click inside a frame; on the main page
  // the tab's address is the current one (see frameAddress in lib/source.js).
  const menuSender = (info, tab) => ({
    url: info.frameUrl || info.pageUrl,
    frameId: info.frameUrl ? info.frameId || -1 : 0,
    tab,
  });

  /** "Download video on this page with rustdm": the frame's own page. */
  async function menuPage(info, tab) {
    if (!tab || tab.incognito) return;
    await sendFromMenu(resolveSource({ type: 'page' }, menuSender(info, tab), []), tab);
  }

  /** "Download with rustdm" on a `blob:` video: the stream caught in its frame, else the page. */
  async function menuBlob(info, tab) {
    if (!tab || tab.incognito) return;
    const caught = await caughtFor(tab.id);
    const source = pickSource({ currentSrc: '', frameId: info.frameId ?? 0, caught });
    await sendFromMenu(resolveSource(source, menuSender(info, tab), caught), tab);
  }

  function showPageMenu(on) {
    Promise.resolve()
      .then(() => chrome.contextMenus.update(PAGE_MENU_ID, { visible: on }))
      .catch(() => {
        // Not created yet (first start); background creates it visible and calls this again.
      });
  }

  // The page script runs only with the grant and both switches on. One sync
  // at a time, so two quick changes cannot register it twice.
  let hoverSync = Promise.resolve();
  function syncHoverScripts() {
    hoverSync = hoverSync
      .then(async () => {
        const [granted, on, stored] = await Promise.all([
          chrome.permissions.contains(ALL_URLS).catch(() => false),
          enabled,
          chrome.storage.local.get({ videoBar: true }).catch(() => ({ videoBar: true })),
        ]);
        const wanted = granted && on && stored.videoBar !== false;
        const present = (await chrome.scripting.getRegisteredContentScripts({ ids: [HOVER_SCRIPT.id] })).length > 0;
        if (wanted && !present) await chrome.scripting.registerContentScripts([HOVER_SCRIPT]);
        if (!wanted && present) await chrome.scripting.unregisterContentScripts({ ids: [HOVER_SCRIPT.id] });
      })
      .catch(() => {
        // Tried again on the next grant or switch change.
      });
    return hoverSync;
  }

  chrome.storage.onChanged.addListener((changes, area) => {
    if (area !== 'local') return;
    if ('enabled' in changes) {
      const on = changes.enabled.newValue !== false;
      enabled = Promise.resolve(on);
      showPageMenu(on);
      if (!on) void clearAll();
    }
    if ('enabled' in changes || 'videoBar' in changes) void syncHoverScripts();
  });
  void enabled.then(showPageMenu);
  chrome.tabs.onUpdated.addListener((tabId, changeInfo) => {
    if (changeInfo.url) void updateTab(tabId, (record) => tabRecordAfterNavigation(record, changeInfo.url));
  });
  chrome.tabs.onRemoved.addListener((tabId) => void updateTab(tabId, () => undefined));
  chrome.permissions.onAdded.addListener(() => {
    watchStreams();
    void syncHoverScripts();
  });
  chrome.permissions.onRemoved.addListener(() => void syncHoverScripts());
  watchStreams();
  void syncHoverScripts();

  // The card: one port from the page, one native session per chosen source.
  function onCardConnect(port) {
    if (port.name !== CARD_PORT || port.sender?.id !== chrome.runtime.id) return;
    const sender = port.sender;
    // A page opened before a switch went off still has the bar; refuse it here.
    const allowed = Promise.all([
      enabled,
      chrome.storage.local.get({ videoBar: true }).catch(() => ({ videoBar: true })),
    ]).then(([on, stored]) => on && stored.videoBar !== false);
    let current = null; // {session, timer, state, resolved, others}
    let seen = {}; // what the page told about the video: {at, link, videos}
    let generation = 0;
    let closed = false;

    const post = (view, title, others) => {
      try {
        port.postMessage({ type: 'view', view: { ...view, title, others } });
      } catch {
        // The card is gone.
      }
    };

    const end = (run) => {
      if (!run) return;
      stopTimer(run.timer);
      run.session.close();
    };

    function dispatch(run, event) {
      if (run !== current) return; // a reply from an older source
      const { state, effects } = flowStep(run.state, event);
      run.state = state;
      for (const effect of effects) perform(run, effect);
    }

    function perform(run, effect) {
      const { resolved } = run;
      const reply = (type) => (answer) => dispatch(run, { type, reply: answer, now: now() });
      switch (effect.type) {
        case 'add':
          void collectCookies([resolved.url, sender.tab.url])
            .then((cookies) =>
              run.session.request(buildVideoMessage({ ...resolved, cookies, userAgent: globalThis.navigator?.userAgent })),
            )
            .then(reply('add_reply'));
          break;
        case 'status':
          void run.session.request(buildStatusMessage(effect.downloadId)).then(reply('status_reply'));
          break;
        case 'choose':
          void run.session.request(buildChooseMessage(effect.downloadId, effect.choiceId)).then(reply('choose_reply'));
          break;
        case 'view':
          post(effect.view, resolved.cardTitle, run.others);
          break;
        case 'stop':
          end(run);
          break;
      }
    }

    // `choose(caught)` gives the source; a newer begin or a closed card wins.
    // "This page" made precise: the video the YouTube player plays, the
    // preview's own link, or a feed (refused). A page picked by hand from
    // "Other sources" is only checked for being a feed.
    async function videoPage(page, refine) {
      const address = frameAddress(sender);
      if (!refine) return isFeedPage(address, seen.videos) ? { type: 'feed' } : page;
      const playerId = isYouTube(address) ? await askPlayer() : '';
      return refinePage({ address, playerId, link: seen.link, videos: seen.videos });
    }

    async function askPlayer() {
      const at = seen.at;
      if (!at || !Number.isFinite(at.x) || !Number.isFinite(at.y)) return '';
      const asked = chrome.scripting
        .executeScript({
          target: { tabId: sender.tab.id, frameIds: [sender.frameId ?? 0] },
          world: 'MAIN',
          func: playerVideoAt,
          args: [at.x, at.y],
        })
        .then((results) => String(results?.[0]?.result ?? ''))
        .catch(() => '');
      const late = new Promise((resolve) => setTimeout(() => resolve(''), PLAYER_WAIT_MS));
      return Promise.race([asked, late]);
    }

    async function begin(choose, refine = false) {
      const mine = ++generation;
      end(current);
      current = null;
      const [caught, ok] = await Promise.all([caughtFor(sender.tab?.id), allowed]);
      if (mine !== generation || closed) return;
      if (!ok) {
        closed = true;
        port.disconnect();
        return;
      }
      let picked = choose(caught);
      if (picked?.type === 'page') {
        picked = await videoPage(picked, refine);
        if (mine !== generation || closed) return;
      }
      const resolved = resolveSource(picked, sender, caught);
      if (!resolved.ok) {
        const message = resolved.reason === 'feed' ? FEED : UNUSABLE.message;
        post({ phase: 'failed', message }, hostOf(frameAddress(sender)), otherSources(picked ?? { type: 'page' }, caught));
        return;
      }
      const session = openSession({
        connect: (host) => chrome.runtime.connectNative(host),
        lastError: () => chrome.runtime.lastError,
      });
      const { state, effects } = startFlow(resolved.kind, now());
      const run = { session, state, resolved, others: otherSources(picked, caught), timer: null };
      current = run;
      session.onLost(() => dispatch(run, { type: 'native_lost' }));
      run.timer = startTimer(() => dispatch(run, { type: 'tick', now: now() }), POLL_MS);
      for (const effect of effects) perform(run, effect);
    }

    // Anything unexpected ends the card with a message, never a card that
    // says "Looking up" forever.
    function broke() {
      try {
        end(current);
      } catch {
        // Already ending.
      }
      current = null;
      generation++;
      post({ phase: 'failed', message: BROKE }, hostOf(sender.url), []);
    }

    port.onMessage.addListener((message) => {
      if (message?.type === 'open') {
        seen = {
          at: message.at && typeof message.at === 'object' ? { x: Number(message.at.x), y: Number(message.at.y) } : null,
          link: typeof message.link === 'string' ? message.link : null,
          videos: Number.isFinite(message.videos) ? message.videos : 1,
        };
        begin((caught) => pickSource({ currentSrc: message.currentSrc, frameId: sender.frameId, caught }), true).catch(broke);
      } else if (message?.type === 'use') {
        begin(() => message.source).catch(broke);
      } else if (message?.type === 'visible' && current) {
        dispatch(current, { type: 'hidden', hidden: message.visible === false });
      } else if (message?.type === 'choose' && current && typeof message.choiceId === 'string') {
        dispatch(current, { type: 'choose', choiceId: message.choiceId });
      }
    });
    port.onDisconnect.addListener(() => {
      closed = true;
      generation++;
      end(current);
      current = null;
    });
  }

  chrome.runtime.onConnect.addListener(onCardConnect);

  const syncPageMenu = () => enabled.then(showPageMenu);
  return { caughtFor, menuPage, menuBlob, syncPageMenu };
}

function hostOf(url) {
  try {
    return new URL(url).host;
  } catch {
    return '';
  }
}

async function readEnabled(chrome) {
  try {
    const { enabled } = await chrome.storage.local.get({ enabled: true });
    return enabled !== false;
  } catch {
    return true; // storage unavailable: keep the default (on)
  }
}


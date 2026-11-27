// Browser wiring for video downloads (spec Part 1e). The decisions live in
// lib/catch.js, lib/source.js and lib/flow.js; this file only connects them to
// Chrome. `chrome` and the timers are passed in so tests can use fakes.

import { addCaught, classify, tabRecordAfterNavigation } from './lib/catch.js';
import { buildChooseMessage, buildStatusMessage, buildVideoMessage } from './lib/contract.js';
import { POLL_MS, flowStep, startFlow } from './lib/flow.js';
import { openSession, request } from './lib/native.js';
import { otherSources, pickSource, resolveSource } from './lib/source.js';

const ALL_URLS = { origins: ['<all_urls>'] };
const CAUGHT_PREFIX = 'caught:';
export const PAGE_MENU_ID = 'rustdm-download-page';
export const CARD_PORT = 'rdm-card';
const UNUSABLE = { kind: 'unusable', message: 'rustdm could not use this video.' };

const caughtKey = (tabId) => `${CAUGHT_PREFIX}${tabId}`;
const newId = () => crypto.randomUUID().replaceAll('-', '').slice(0, 8);

/**
 * @param deps `{chrome, setInterval, clearInterval, now, collectCookies, flashOk, showFailure}`
 * @returns `{caughtFor, menuPage, menuBlob}`
 */
export function installVideo(deps) {
  const { chrome, now, collectCookies, flashOk, showFailure } = deps;
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
      return { ...record, list: addCaught(record?.list ?? [], caught, { id: newId(), now: now() }) };
    });
  }

  function watchStreams() {
    try {
      if (!chrome.webRequest.onHeadersReceived.hasListener(onHeaders)) {
        chrome.webRequest.onHeadersReceived.addListener(onHeaders, { urls: ['<all_urls>'] }, ['responseHeaders']);
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

  const menuSender = (info, tab) => ({ url: info.frameUrl || info.pageUrl, tab });

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

  chrome.storage.onChanged.addListener((changes, area) => {
    if (area !== 'local' || !('enabled' in changes)) return;
    const on = changes.enabled.newValue !== false;
    enabled = Promise.resolve(on);
    showPageMenu(on);
    if (!on) void clearAll();
  });
  void enabled.then(showPageMenu);
  chrome.tabs.onUpdated.addListener((tabId, changeInfo) => {
    if (changeInfo.url) void updateTab(tabId, (record) => tabRecordAfterNavigation(record, changeInfo.url));
  });
  chrome.tabs.onRemoved.addListener((tabId) => void updateTab(tabId, () => undefined));
  chrome.permissions.onAdded.addListener(watchStreams);
  watchStreams();

  // The card: one port from the page, one native session per chosen source.
  function onCardConnect(port) {
    if (port.name !== CARD_PORT || port.sender?.id !== chrome.runtime.id) return;
    const sender = port.sender;
    let current = null; // {session, timer, state, resolved, others}
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
      deps.clearInterval(run.timer);
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
    async function begin(choose) {
      const mine = ++generation;
      end(current);
      current = null;
      const caught = await caughtFor(sender.tab?.id);
      if (mine !== generation || closed) return;
      const picked = choose(caught);
      const resolved = resolveSource(picked, sender, caught);
      if (!resolved.ok) {
        post({ phase: 'failed', message: UNUSABLE.message }, hostOf(sender.url), []);
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
      run.timer = deps.setInterval(() => dispatch(run, { type: 'tick', now: now() }), POLL_MS);
      for (const effect of effects) perform(run, effect);
    }

    port.onMessage.addListener((message) => {
      if (message?.type === 'open') {
        void begin((caught) => pickSource({ currentSrc: message.currentSrc, frameId: sender.frameId, caught }));
      } else if (message?.type === 'use') {
        void begin(() => message.source);
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


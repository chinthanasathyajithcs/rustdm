// Browser wiring for video downloads (spec Part 1e). The decisions live in
// lib/catch.js, lib/source.js and lib/flow.js; this file only connects them to
// Chrome. `chrome` and the timers are passed in so tests can use fakes.

import { addCaught, classify, tabRecordAfterNavigation } from './lib/catch.js';
import { buildVideoMessage } from './lib/contract.js';
import { request } from './lib/native.js';
import { pickSource, resolveSource } from './lib/source.js';

const ALL_URLS = { origins: ['<all_urls>'] };
const CAUGHT_PREFIX = 'caught:';
export const PAGE_MENU_ID = 'rustdm-download-page';
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

  const syncPageMenu = () => enabled.then(showPageMenu);
  return { caughtFor, menuPage, menuBlob, syncPageMenu };
}

async function readEnabled(chrome) {
  try {
    const { enabled } = await chrome.storage.local.get({ enabled: true });
    return enabled !== false;
  } catch {
    return true; // storage unavailable: keep the default (on)
  }
}


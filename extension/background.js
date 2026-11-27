// Browser event wiring: the only Chrome-specific file. The decisions live in lib/.

import { buildDownloadMessage, buildMenuMessage, dedupeCookies, isWebUrl } from './lib/contract.js';
import { describeError, request } from './lib/native.js';
import { recordPost, shouldTakeOver } from './lib/takeover.js';
import { installVideo } from './video.js';

const ALL_URLS = { origins: ['<all_urls>'] };
const MENU_ID = 'rustdm-download-link';
const BADGE_MS = 4000;
const OK_SHOWN_AT = 'okBadgeShownAt';
const GREEN = '#1a7f37';
const RED = '#cf222e';

const recentPosts = new Map();
let nextMessageId = 1;

const send = (host, message) => chrome.runtime.sendNativeMessage(host, message);

async function loadSettings() {
  let enabled = true;
  try {
    const stored = await chrome.storage.local.get({ enabled: true });
    enabled = stored.enabled !== false;
  } catch {
    // Storage unavailable: keep the default (on).
  }
  const granted = await chrome.permissions.contains(ALL_URLS).catch(() => false);
  return { enabled, granted };
}

async function clearOkBadge() {
  if ((await chrome.action.getBadgeText({})) === 'OK') {
    await chrome.action.setBadgeText({ text: '' });
  }
  await chrome.storage.session.remove(OK_SHOWN_AT).catch(() => {});
}

// The worker can stop before the 4 s timer fires, so the time is also kept in
// session storage and checked whenever the worker starts or an event arrives.
async function clearStaleOkBadge() {
  const stored = await chrome.storage.session.get(OK_SHOWN_AT).catch(() => ({}));
  const shownAt = stored?.[OK_SHOWN_AT];
  if (typeof shownAt === 'number' && Date.now() - shownAt >= BADGE_MS) await clearOkBadge();
}

async function flashOk() {
  await chrome.storage.local.remove('lastError').catch(() => {});
  await chrome.action.setBadgeBackgroundColor({ color: GREEN });
  await chrome.action.setBadgeText({ text: 'OK' });
  await chrome.storage.session.set({ [OK_SHOWN_AT]: Date.now() }).catch(() => {});
  setTimeout(() => void clearOkBadge(), BADGE_MS);
}

// rustdm could not take it, so the browser's own download goes ahead.
async function fallbackToBrowser(url) {
  try {
    await chrome.downloads.download({ url });
  } catch (error) {
    await showFailure({ kind: 'browser_download_failed', message: String(error?.message ?? error) });
  }
}

async function showFailure(error) {
  const lastError = { kind: error.kind, message: describeError(error), at: Date.now() };
  await chrome.storage.local.set({ lastError }).catch(() => {});
  await chrome.action.setBadgeBackgroundColor({ color: RED });
  await chrome.action.setBadgeText({ text: '!' });
}

async function collectCookies(urls) {
  const unique = [...new Set(urls.filter(isWebUrl))];
  const lists = await Promise.all(
    unique.map((url) => chrome.cookies.getAll({ url }).catch(() => [])),
  );
  return dedupeCookies(lists.flat());
}

function messageId() {
  return nextMessageId++;
}

const handedOff = (reply) => reply.ok || reply.error?.kind === 'duplicate';

// Hold the download (the browser keeps writing it) until rustdm has it or fails.
async function handleDownload(item, suggest) {
  let suggested = false;
  const release = () => {
    if (suggested) return;
    suggested = true;
    suggest();
    // Read it so the browser does not log "Unchecked runtime.lastError" when
    // the download is already gone.
    void chrome.runtime.lastError;
  };
  try {
    await clearStaleOkBadge();
    const decision = shouldTakeOver(item, await loadSettings(), recentPosts, Date.now());
    if (!decision.take) return;

    const cookies = await collectCookies([item.url, item.finalUrl]);
    const message = buildDownloadMessage({
      id: messageId(),
      item,
      cookies,
      userAgent: navigator.userAgent,
    });
    const reply = await request(message, { send });
    if (!handedOff(reply)) {
      await showFailure(reply.error);
      return;
    }
    try {
      await chrome.downloads.cancel(item.id);
    } catch (error) {
      // The browser still has its copy and keeps it (released below), so say
      // that rustdm has one too instead of claiming success.
      await showFailure({ kind: 'cancel_failed', message: String(error?.message ?? error) });
      return;
    }
    release(); // after cancel, so the browser does not start writing the file
    await chrome.downloads.erase({ id: item.id }).catch(() => {});
    await flashOk();
  } catch (error) {
    await showFailure({ kind: 'internal', message: String(error?.message ?? error) }).catch(() => {});
  } finally {
    release();
  }
}

chrome.downloads.onDeterminingFilename.addListener((item, suggest) => {
  void handleDownload(item, suggest);
  return true; // suggest() is called later, once the decision is made
});

async function handleMenuClick(info) {
  await clearStaleOkBadge();
  const url = info.linkUrl || info.srcUrl;
  if (!url) return;
  if (!isWebUrl(url)) {
    await fallbackToBrowser(url); // blob:, data: and the like stay with the browser
    return;
  }
  const { granted } = await loadSettings();
  const cookies = granted ? await collectCookies([url]) : [];
  const message = buildMenuMessage({
    id: messageId(),
    url,
    pageUrl: info.pageUrl,
    cookies,
    userAgent: navigator.userAgent,
  });
  const reply = await request(message, { send });
  if (handedOff(reply)) {
    await flashOk();
    return;
  }
  await showFailure(reply.error);
  await fallbackToBrowser(url); // not caught again: it is started by an extension
}

chrome.contextMenus.onClicked.addListener((info, tab) => {
  if (info.menuItemId !== MENU_ID) return;
  // The manifest keeps the extension out of private windows ("not_allowed");
  // this is the second lock, in case that ever changes.
  if (tab?.incognito) return;
  void handleMenuClick(info);
});

// Learn which pages were POSTed, so a form download is left to the browser.
function onBeforeRequest(details) {
  if (details.method === 'POST') recordPost(recentPosts, details.url, Date.now());
}

function watchPosts() {
  try {
    if (!chrome.webRequest.onBeforeRequest.hasListener(onBeforeRequest)) {
      chrome.webRequest.onBeforeRequest.addListener(onBeforeRequest, {
        urls: ['<all_urls>'],
        types: ['main_frame', 'sub_frame'],
      });
    }
  } catch {
    // No host permission yet; tried again when it is granted.
  }
}

watchPosts();
installVideo({ chrome, setInterval, clearInterval, now: Date.now, collectCookies, flashOk, showFailure });
void clearStaleOkBadge(); // the worker may have stopped before the 4 s timer fired
chrome.permissions.onAdded.addListener(watchPosts);

chrome.runtime.onInstalled.addListener((details) => {
  chrome.contextMenus.create({
    id: MENU_ID,
    title: 'Download with rustdm',
    contexts: ['link', 'video', 'audio'],
  });
  if (details.reason === 'install') chrome.runtime.openOptionsPage();
});

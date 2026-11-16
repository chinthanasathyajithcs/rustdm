// The panel, the options page and the welcome page. Text goes in with
// textContent only; nothing from the host or the pages is parsed as HTML.

import { describeError, request } from './lib/native.js';

const ALL_URLS = { origins: ['<all_urls>'] };
const $ = (id) => document.getElementById(id);

function describeHost(reply) {
  if (!reply.ok) return describeError(reply.error);
  const { daemon, host_version: hostVersion } = reply.result;
  return daemon.running
    ? `rustdm ${daemon.version} is running (host ${hostVersion}).`
    : 'rustdm is not running; it starts when the first download arrives.';
}

async function showPermission() {
  const granted = await chrome.permissions.contains(ALL_URLS).catch(() => false);
  $('permission').hidden = granted;
}

async function showLastError() {
  const { lastError } = await chrome.storage.local.get('lastError');
  $('error-box').hidden = !lastError;
  if (lastError) $('error').textContent = lastError.message;
  // Seeing the problem is what clears the badge.
  await chrome.action.setBadgeText({ text: '' });
}

async function showHost() {
  const reply = await request({ type: 'ping', id: 1 }, { send: (host, message) => chrome.runtime.sendNativeMessage(host, message) });
  $('host').textContent = describeHost(reply);
}

async function init() {
  const { enabled } = await chrome.storage.local.get({ enabled: true });
  $('enabled').checked = enabled !== false;
  $('enabled').addEventListener('change', (event) => {
    void chrome.storage.local.set({ enabled: event.target.checked });
  });
  $('grant').addEventListener('click', async () => {
    await chrome.permissions.request(ALL_URLS); // needs this click as its user gesture
    await showPermission();
  });
  await Promise.all([showPermission(), showLastError(), showHost()]);
}

void init();

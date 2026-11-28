// Builds the messages the native host understands (spec section 3.1 and 4.3).
// Pure: browser data in, plain objects out.

const MAX_FILENAME_BYTES = 255;

function isWebUrl(text) {
  try {
    const { protocol } = new URL(text);
    return protocol === 'http:' || protocol === 'https:';
  } catch {
    return false;
  }
}

/** Keeps the first cookie for each name, domain and path. */
export function dedupeCookies(cookies) {
  const seen = new Map();
  for (const cookie of cookies) {
    const key = [cookie.name, cookie.domain, cookie.path].join('\u0000');
    if (!seen.has(key)) seen.set(key, cookie);
  }
  return [...seen.values()];
}

/** A chrome.cookies.Cookie as the `context.cookies` entry the daemon expects. */
export function toContextCookie(cookie) {
  const mapped = {
    name: cookie.name,
    value: cookie.value,
    domain: cookie.domain,
    path: cookie.path,
    secure: Boolean(cookie.secure),
    host_only: Boolean(cookie.hostOnly),
  };
  if (!cookie.session && typeof cookie.expirationDate === 'number') {
    mapped.expires = cookie.expirationDate;
  }
  return mapped;
}

/** The last part of a path, or undefined when it would not pass the host's checks. */
export function safeFilename(path) {
  if (typeof path !== 'string') return undefined;
  const name = path.split(/[\\/]/).pop();
  if (!name || name.includes('..') || /[\u0000-\u001f\u007f]/.test(name)) return undefined;
  if (new TextEncoder().encode(name).length > MAX_FILENAME_BYTES) return undefined;
  return name;
}

/** The `context` object. A title is trimmed only; the host decides the rest. */
export function buildContext({ cookies, referer, userAgent, title }) {
  const context = {};
  const mapped = dedupeCookies(cookies ?? []).map(toContextCookie);
  if (mapped.length > 0) context.cookies = mapped;
  if (referer && isWebUrl(referer)) context.referer = referer;
  if (userAgent) context.user_agent = userAgent;
  const trimmed = typeof title === 'string' ? title.trim() : '';
  if (trimmed) context.title = trimmed;
  return context;
}

/** The `add` message for a download the browser started. */
export function buildDownloadMessage({ id, item, cookies, userAgent }) {
  const message = {
    type: 'add',
    id,
    url: item.finalUrl || item.url,
    context: buildContext({ cookies, referer: item.referrer, userAgent }),
  };
  const filename = safeFilename(item.filename);
  if (filename) message.filename = filename;
  return message;
}

/** The `add` message for "Download with rustdm" on a link or media element. */
export function buildMenuMessage({ id, url, pageUrl, cookies, userAgent }) {
  return {
    type: 'add',
    id,
    url,
    context: buildContext({ cookies, referer: pageUrl, userAgent }),
  };
}

/** The `add` message for a video from the card or the page menu; the session sets `id`. */
export function buildVideoMessage({ url, referer, title, cookies, userAgent }) {
  return { type: 'add', url, context: buildContext({ cookies, referer, userAgent, title }) };
}

export function buildStatusMessage(downloadId) {
  return { type: 'status', download_id: downloadId };
}

export function buildChooseMessage(downloadId, choiceId) {
  return { type: 'choose_quality', download_id: downloadId, choice_id: choiceId };
}

export { isWebUrl };

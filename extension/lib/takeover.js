// Decides whether rustdm takes a browser download over. Pure: no browser APIs.

export const POST_WINDOW_MS = 30_000;
export const MIN_TAKEOVER_BYTES = 1024 * 1024;
export const MAX_RECENT_POSTS = 200;

const PAGE_TYPES = new Set(['text/html', 'application/xhtml+xml']);

function isWebUrl(text) {
  try {
    const { protocol } = new URL(text);
    return protocol === 'http:' || protocol === 'https:';
  } catch {
    return false;
  }
}

function baseMime(mime) {
  return String(mime ?? '').split(';')[0].trim().toLowerCase();
}

/**
 * @param item a chrome.downloads.DownloadItem
 * @param settings `{ enabled, granted }`: the on/off switch and the <all_urls> grant
 * @param recentPosts Map of URL to the time (ms) it was last POSTed
 * @param now current time in ms
 * @returns `{ take: boolean, reason: string }`
 */
export function shouldTakeOver(item, settings, recentPosts, now = Date.now()) {
  const no = (reason) => ({ take: false, reason });

  if (!settings.enabled) return no('disabled');
  if (!settings.granted) return no('no-permission');
  const finalUrl = item.finalUrl || item.url;
  if (!isWebUrl(finalUrl)) return no('not-http');
  if (item.incognito) return no('incognito');
  if (item.byExtensionId) return no('by-extension');
  for (const url of new Set([item.url, finalUrl])) {
    const postedAt = recentPosts.get(url);
    if (postedAt !== undefined && now - postedAt <= POST_WINDOW_MS) return no('recent-post');
  }
  if (PAGE_TYPES.has(baseMime(item.mime))) return no('html');
  if (item.totalBytes > 0 && item.totalBytes < MIN_TAKEOVER_BYTES) return no('small');
  return { take: true, reason: 'take' };
}

/** Remember that `url` was just POSTed; keeps the map small. Returns the map. */
export function recordPost(recentPosts, url, now = Date.now()) {
  recentPosts.delete(url);
  recentPosts.set(url, now);
  for (const [seen, at] of recentPosts) {
    if (now - at <= POST_WINDOW_MS && recentPosts.size <= MAX_RECENT_POSTS) break;
    recentPosts.delete(seen); // oldest first: Map keeps insertion order
  }
  return recentPosts;
}

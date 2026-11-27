// Which link a card or the right-click menu sends to rustdm (spec 9.2, 9.3
// step 2). Pure: the sender comes from the browser, never from the page.

import { isWebUrl } from './contract.js';

const PAGE_LABEL = 'This page';
const YOUTUBE_ID = /^[A-Za-z0-9_-]{11}$/;
// Addresses of pages with many videos and none of their own: home, feeds,
// search, trending. yt-dlp reads them as a list, never as the video.
const FEED_PATH = /^\/(?:$|home\/?$|feed|explore|search|discover|trending|popular|latest|timeline|for-?you|following)/i;
const YOUTUBE_VIDEO_PATH = /^\/(?:watch|shorts\/|embed\/|live\/|v\/)/;

function parse(url) {
  try {
    return new URL(url);
  } catch {
    return null;
  }
}

const isYouTubeHost = (host) => /(^|\.)(youtube\.com|youtube-nocookie\.com)$/.test(host);

/** True for an address that is a list of videos, not one video's page. */
export function isFeedPage(address, videos) {
  const url = parse(address);
  if (!url) return false;
  if (isYouTubeHost(url.hostname)) return !YOUTUBE_VIDEO_PATH.test(url.pathname);
  return videos > 1 && FEED_PATH.test(url.pathname);
}

/**
 * What a "This page" source becomes when the page says more: the video the
 * YouTube player plays, the video's own link, or a feed (refused).
 */
export function refinePage({ address, playerId, link, videos }) {
  if (YOUTUBE_ID.test(playerId ?? '')) return { type: 'link', url: `https://www.youtube.com/watch?v=${playerId}` };
  if (link && isWebUrl(link)) return { type: 'link', url: link };
  return isFeedPage(address, videos) ? { type: 'feed' } : { type: 'page' };
}

/** The source for a video: its own http(s) link, else a stream caught in its frame, else the page. */
export function pickSource({ currentSrc, frameId, caught }) {
  if (currentSrc && isWebUrl(currentSrc)) return { type: 'element', url: currentSrc };
  const inFrame = caught.filter((e) => e.frameId === frameId);
  const best = inFrame.find((e) => e.kind === 'hls') ?? inFrame.find((e) => e.kind === 'file');
  return best ? { type: 'caught', id: best.id } : { type: 'page' };
}

/** What the card's "Other sources" lists, besides `chosen`. */
export function otherSources(chosen, caught) {
  const others = caught
    .filter((e) => !(chosen.type === 'caught' && chosen.id === e.id))
    .map((e) => ({ key: e.id, label: `${e.name} · ${e.host}` }));
  // The page is offered only when it may be the video's: not when the video's
  // own page was found, and not on a feed.
  if (chosen.type !== 'page' && chosen.type !== 'link' && chosen.type !== 'feed') {
    others.push({ key: 'page', label: PAGE_LABEL });
  }
  return others;
}

function isPlaylistLink(url) {
  try {
    return new URL(url).pathname.toLowerCase().endsWith('.m3u8');
  } catch {
    return false;
  }
}

function hostOf(url) {
  try {
    return new URL(url).host;
  } catch {
    return '';
  }
}

/**
 * The address of the frame the video is in. For the main page it is the tab's:
 * sites like YouTube move to another video without loading a new page, and
 * the sender keeps the first address while the tab's follows. A frame keeps
 * its own (embedded players do not move like that).
 */
export function frameAddress(sender) {
  const tabUrl = sender?.tab?.url;
  if (sender?.frameId === 0 && tabUrl && isWebUrl(tabUrl)) return tabUrl;
  return sender?.url;
}

/**
 * Checks a source against the sender and this tab's caught list.
 * @returns `{ok: true, url, referer?, kind: 'file'|'stream'|'page', title?, cardTitle}` or `{ok: false}`
 */
export function resolveSource(source, sender, caught) {
  const refused = { ok: false };
  const tab = sender?.tab;
  if (!tab || tab.incognito) return refused;

  const address = frameAddress(sender);
  let picked;
  if (source?.type === 'element' && isWebUrl(source.url ?? '')) {
    picked = { url: source.url, referer: address, kind: isPlaylistLink(source.url) ? 'stream' : 'file' };
  } else if (source?.type === 'caught') {
    const entry = caught.find((e) => e.id === source.id);
    if (!entry) return refused;
    picked = { url: entry.url, referer: address, kind: entry.kind === 'hls' ? 'stream' : 'file' };
  } else if (source?.type === 'page' && isWebUrl(address ?? '')) {
    picked = { url: address, kind: 'page' };
  } else if (source?.type === 'link' && isWebUrl(source.url ?? '')) {
    picked = { url: source.url, kind: 'page' };
  } else if (source?.type === 'feed') {
    return { ok: false, reason: 'feed' };
  } else {
    return refused;
  }

  if (!picked.referer || !isWebUrl(picked.referer)) delete picked.referer;
  const title = typeof tab.title === 'string' ? tab.title.trim() : '';
  return {
    ok: true,
    ...picked,
    ...(title ? { title } : {}),
    cardTitle: title || hostOf(address),
  };
}

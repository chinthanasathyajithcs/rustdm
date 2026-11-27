// Which responses are video streams, and the per-tab list of them (spec 7).
// Pure: webRequest details in, plain objects out.

import { MIN_TAKEOVER_BYTES } from './takeover.js';

export const MAX_CAUGHT = 20;

const HLS_TYPES = new Set([
  'application/vnd.apple.mpegurl',
  'application/x-mpegurl',
  'audio/mpegurl',
  'audio/x-mpegurl',
]);
const PIECE_TYPES = new Set(['video/mp2t', 'video/iso.segment', 'application/dash+xml']);
const PIECE_ENDINGS = ['.ts', '.m4s', '.m4f', '.aac', '.mpd'];
const FILE_ENDINGS = ['.mp4', '.webm', '.mkv', '.mov', '.m4a', '.mp3', '.ogg', '.opus', '.flac', '.wav'];

function header(details, name) {
  const found = (details.responseHeaders ?? []).find((h) => h.name?.toLowerCase() === name);
  return found?.value;
}

const baseType = (value) => String(value ?? '').split(';')[0].trim().toLowerCase();

function parseUrl(text) {
  try {
    return new URL(text);
  } catch {
    return null;
  }
}

function totalBytes(details) {
  const range = /\/(\d+)\s*$/.exec(header(details, 'content-range') ?? '');
  if (range) return Number(range[1]);
  if (details.statusCode !== 200) return undefined;
  const length = header(details, 'content-length');
  return length !== undefined && /^\d+$/.test(length.trim()) ? Number(length) : undefined;
}

/**
 * @param details a webRequest.onHeadersReceived details object
 * @returns `{url, kind: 'hls'|'file', bytes?, frameId}` or null
 */
export function classify(details) {
  if (!(details.tabId >= 0)) return null;
  const url = parseUrl(details.url);
  if (!url || (url.protocol !== 'http:' && url.protocol !== 'https:')) return null;
  if (details.statusCode !== 200 && details.statusCode !== 206) return null;

  const type = baseType(header(details, 'content-type'));
  const path = url.pathname.toLowerCase();
  if (PIECE_TYPES.has(type) || PIECE_ENDINGS.some((e) => path.endsWith(e))) return null;

  const base = { url: details.url, frameId: details.frameId };
  if (HLS_TYPES.has(type) || (path.endsWith('.m3u8') && type !== 'text/html')) {
    return { ...base, kind: 'hls' };
  }

  if (details.type !== 'media') return null;
  const media = type.startsWith('video/') || type.startsWith('audio/');
  const octet = type === 'application/octet-stream' && FILE_ENDINGS.some((e) => path.endsWith(e));
  if (!media && !octet) return null;
  const bytes = totalBytes(details);
  if (bytes !== undefined && bytes < MIN_TAKEOVER_BYTES) return null;
  return bytes === undefined ? { ...base, kind: 'file' } : { ...base, kind: 'file', bytes };
}

/** The URL without its fragment. */
export function documentKey(url) {
  const hash = url.indexOf('#');
  return hash === -1 ? url : url.slice(0, hash);
}

function nameOf(url) {
  const last = url.pathname.split('/').pop();
  if (!last) return url.host;
  try {
    return decodeURIComponent(last);
  } catch {
    return last;
  }
}

const folderOf = (pathname) => pathname.slice(0, pathname.lastIndexOf('/') + 1);

// A later playlist from the same host and frame, in the kept playlist's folder
// or below it: one of its qualities. Another video on a feed sits elsewhere.
function isVariantOf(url, frameId, entry) {
  if (entry.kind !== 'hls' || entry.frameId !== frameId) return false;
  const kept = new URL(entry.url);
  return kept.host === url.host && url.pathname.startsWith(folderOf(kept.pathname));
}

/**
 * A new list with `caught` added (spec 7.3). `id` names a new entry; a repeat
 * keeps its old id.
 */
export function addCaught(list, caught, { id, now }) {
  const key = documentKey(caught.url);
  const url = new URL(key);
  const host = url.host;
  const old = list.find((e) => documentKey(e.url) === key);
  const rest = list.filter((e) => e !== old);
  if (!old && caught.kind === 'hls' && rest.some((e) => isVariantOf(url, caught.frameId, e))) {
    return list; // a single-quality playlist of one we already have
  }
  const sizes = [old?.bytes, caught.bytes].filter((b) => typeof b === 'number');
  const entry = {
    id: old?.id ?? id,
    url: key,
    kind: caught.kind,
    host,
    name: nameOf(url),
    frameId: caught.frameId,
    seenAt: now,
    ...(sizes.length > 0 ? { bytes: Math.max(...sizes) } : {}),
  };
  return [entry, ...rest].slice(0, MAX_CAUGHT);
}

/** The tab's `{doc, list}` after it moved to `newUrl`: a new document starts empty. */
export function tabRecordAfterNavigation(record, newUrl) {
  const doc = documentKey(newUrl);
  const list = record?.list ?? [];
  const same = record?.doc === undefined || record.doc === doc;
  return { doc, list: same ? list : [] };
}

import assert from 'node:assert/strict';
import test from 'node:test';
import {
  MAX_CAUGHT,
  addCaught,
  classify,
  documentKey,
  tabRecordAfterNavigation,
} from '../lib/catch.js';

const MiB = 1024 * 1024;

function details(over = {}) {
  const { headers = {}, ...rest } = over;
  return {
    url: 'https://cdn.example.com/v/movie.mp4',
    type: 'media',
    statusCode: 200,
    tabId: 7,
    frameId: 0,
    responseHeaders: Object.entries({ 'Content-Type': 'video/mp4', 'Content-Length': String(5 * MiB), ...headers })
      .filter(([, value]) => value !== undefined)
      .map(([name, value]) => ({ name, value })),
    ...rest,
  };
}

const hls = (url, frameId = 0) =>
  classify(details({ url, type: 'xmlhttprequest', headers: { 'Content-Type': 'application/vnd.apple.mpegurl', 'Content-Length': '900' }, frameId }));

let counter = 0;
const add = (list, caught, now = 1000) => addCaught(list, caught, { id: `id${++counter}`, now });

test('a media video/mp4 response is a file', () => {
  assert.deepEqual(classify(details()), {
    url: 'https://cdn.example.com/v/movie.mp4',
    kind: 'file',
    bytes: 5 * MiB,
    frameId: 0,
  });
});

test('a script-fetched video is not caught', () => {
  assert.equal(classify(details({ type: 'xmlhttprequest' })), null);
  assert.equal(classify(details({ type: 'other' })), null);
});

test('each HLS type is caught from any request type', () => {
  for (const type of ['application/vnd.apple.mpegurl', 'application/x-mpegURL', 'audio/mpegurl', 'audio/x-mpegurl; charset=utf-8']) {
    const caught = classify(details({ url: 'https://e.com/live/index', type: 'xmlhttprequest', headers: { 'Content-Type': type } }));
    assert.equal(caught?.kind, 'hls', type);
  }
});

test('an .m3u8 path is HLS unless the reply is HTML', () => {
  const by = (contentType) =>
    classify(details({ url: 'https://e.com/a/Master.M3U8?token=1', type: 'xmlhttprequest', headers: { 'Content-Type': contentType } }));
  assert.equal(by('text/plain').kind, 'hls');
  assert.equal(by(undefined).kind, 'hls');
  assert.equal(by('text/html; charset=utf-8'), null);
});

test('an octet-stream needs a media file ending', () => {
  const by = (url) => classify(details({ url, headers: { 'Content-Type': 'application/octet-stream' } }));
  assert.equal(by('https://e.com/clip.mkv').kind, 'file');
  assert.equal(by('https://e.com/song.FLAC').kind, 'file');
  assert.equal(by('https://e.com/blob.bin'), null);
});

test('the size comes from content-range, then content-length on a 200', () => {
  const ranged = classify(details({ statusCode: 206, headers: { 'Content-Range': 'bytes 0-1/5242880', 'Content-Length': '2' } }));
  assert.equal(ranged.bytes, 5242880);
  const partial = classify(details({ statusCode: 206, headers: { 'Content-Range': undefined, 'Content-Length': String(2 * MiB) } }));
  assert.equal(partial.bytes, undefined);
});

test('below one MiB is skipped only when the size is known', () => {
  assert.equal(classify(details({ headers: { 'Content-Length': String(MiB - 1) } })), null);
  assert.equal(classify(details({ headers: { 'Content-Length': String(MiB) } })).kind, 'file');
  const unknown = classify(details({ headers: { 'Content-Length': undefined } }));
  assert.equal(unknown.kind, 'file');
  assert.equal('bytes' in unknown, false);
});

test('pieces and DASH are never caught', () => {
  for (const contentType of ['video/mp2t', 'video/iso.segment', 'application/dash+xml']) {
    assert.equal(classify(details({ headers: { 'Content-Type': contentType } })), null, contentType);
  }
  for (const ending of ['ts', 'm4s', 'm4f', 'aac', 'mpd']) {
    assert.equal(classify(details({ url: `https://e.com/seg/part1.${ending}` })), null, ending);
  }
  assert.equal(classify(details({ url: 'https://e.com/manifest.mpd', type: 'xmlhttprequest' })), null);
});

test('only http(s) with status 200 or 206, from a tab', () => {
  assert.equal(classify(details({ url: 'blob:https://e.com/1234' })), null);
  assert.equal(classify(details({ statusCode: 304 })), null);
  assert.equal(classify(details({ statusCode: 404 })), null);
  assert.equal(classify(details({ tabId: -1 })), null);
  assert.equal(classify(details({ url: 'http://e.com/a.webm', headers: { 'Content-Type': 'video/webm' } })).kind, 'file');
});

test('the first playlist per host and frame is kept', () => {
  let list = add([], hls('https://cdn.e.com/master.m3u8'));
  list = add(list, hls('https://cdn.e.com/720p/index.m3u8'), 2000);
  assert.equal(list.length, 1);
  assert.equal(list[0].url, 'https://cdn.e.com/master.m3u8');
  list = add(list, hls('https://cdn.e.com/player2/master.m3u8', 3), 3000);
  assert.equal(list.length, 2, 'another frame is its own entry');
});

test('an ad host playlist is its own entry', () => {
  let list = add([], hls('https://cdn.e.com/master.m3u8'));
  list = add(list, hls('https://ads.example.net/spot.m3u8'), 2000);
  assert.deepEqual(list.map((e) => e.host), ['ads.example.net', 'cdn.e.com']);
});

test('a repeat moves to the front and keeps the larger size and its id', () => {
  let list = add([], classify(details({ url: 'https://e.com/a.mp4' })));
  const firstId = list[0].id;
  list = add(list, classify(details({ url: 'https://e.com/b.mp4' })), 2000);
  list = add(list, classify(details({ url: 'https://e.com/a.mp4', statusCode: 206, headers: { 'Content-Length': undefined } })), 3000);
  assert.deepEqual(list.map((e) => e.name), ['a.mp4', 'b.mp4']);
  assert.equal(list[0].bytes, 5 * MiB);
  assert.equal(list[0].id, firstId);
  assert.equal(list[0].seenAt, 3000);
});

test('at most twenty entries are kept, newest first', () => {
  let list = [];
  for (let i = 0; i < MAX_CAUGHT + 5; i++) {
    list = add(list, classify(details({ url: `https://e.com/${i}.mp4` })), i);
  }
  assert.equal(list.length, 20);
  assert.equal(list[0].name, '24.mp4');
  assert.equal(list.at(-1).name, '5.mp4');
});

test('keys ignore the fragment', () => {
  assert.equal(documentKey('https://e.com/a.mp4#t=10'), 'https://e.com/a.mp4');
  let list = add([], classify(details({ url: 'https://e.com/a.mp4#t=1' })));
  list = add(list, classify(details({ url: 'https://e.com/a.mp4#t=2' })), 2000);
  assert.equal(list.length, 1);
});

test('an entry has a decoded name, or the host when the path is empty', () => {
  const [entry] = add([], classify(details({ url: 'https://e.com/My%20Clip.mp4?x=1' })));
  assert.equal(entry.name, 'My Clip.mp4');
  assert.equal(entry.host, 'e.com');
  assert.equal(entry.kind, 'file');
  const [bare] = add([], hls('https://stream.e.com/'));
  assert.equal(bare.name, 'stream.e.com');
  const [broken] = add([], classify(details({ url: 'https://e.com/%E0%A4%A.mp4' })));
  assert.equal(broken.name, '%E0%A4%A.mp4');
});

test('navigation clears on a new document but not on a fragment change', () => {
  const list = [{ id: 'x' }];
  assert.deepEqual(tabRecordAfterNavigation({ doc: 'https://e.com/a', list }, 'https://e.com/a#comments'), {
    doc: 'https://e.com/a',
    list,
  });
  assert.deepEqual(tabRecordAfterNavigation({ doc: 'https://e.com/a', list }, 'https://e.com/b'), {
    doc: 'https://e.com/b',
    list: [],
  });
  assert.deepEqual(tabRecordAfterNavigation({ list }, 'https://e.com/a'), { doc: 'https://e.com/a', list });
  assert.deepEqual(tabRecordAfterNavigation(undefined, 'https://e.com/a'), { doc: 'https://e.com/a', list: [] });
});

test('addCaught does not change its input', () => {
  const list = Object.freeze([Object.freeze({ id: 'a', url: 'https://e.com/a.mp4', kind: 'file', host: 'e.com', name: 'a.mp4', frameId: 0, seenAt: 1, bytes: 1 })]);
  const next = add(list, classify(details({ url: 'https://e.com/a.mp4' })), 5);
  assert.equal(list[0].seenAt, 1);
  assert.equal(next[0].seenAt, 5);
});

test('a second video on a feed keeps its own playlist', () => {
  let list = add([], hls('https://video.twimg.com/ext/111/pl/a.m3u8'));
  list = add(list, hls('https://video.twimg.com/ext/111/pl/avc1/720x1280/b.m3u8'), 2000);
  assert.equal(list.length, 1, 'a variant under the master folder is dropped');
  list = add(list, hls('https://video.twimg.com/ext/222/pl/c.m3u8'), 3000);
  assert.deepEqual(list.map((e) => e.name), ['c.m3u8', 'a.m3u8']);
  list = add(list, hls('https://video.twimg.com/ext/222/pl/avc1/480x852/d.m3u8'), 4000);
  assert.equal(list.length, 2);
});

test('the newest entry seen again with no bigger size changes nothing', () => {
  const first = add([], classify(details({ url: 'https://e.com/live.m3u8', type: 'xmlhttprequest', headers: { 'Content-Type': 'application/x-mpegurl' } })));
  const again = add(first, classify(details({ url: 'https://e.com/live.m3u8', type: 'xmlhttprequest', headers: { 'Content-Type': 'application/x-mpegurl' } })), 9000);
  assert.equal(again, first, 'the same list, so nothing is written');
  const bigger = add(add([], classify(details({ url: 'https://e.com/a.mp4' }))), classify(details({ url: 'https://e.com/a.mp4', statusCode: 206, headers: { 'Content-Range': 'bytes 0-1/99999999' } })), 9000);
  assert.equal(bigger[0].bytes, 99999999);
});

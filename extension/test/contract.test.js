import assert from 'node:assert/strict';
import test from 'node:test';
import {
  buildChooseMessage,
  buildContext,
  buildDownloadMessage,
  buildLookUpMessage,
  buildMenuMessage,
  buildStatusMessage,
  buildVideoMessage,
  dedupeCookies,
  safeFilename,
  toContextCookie,
} from '../lib/contract.js';

const cookie = (over = {}) => ({
  name: 'SID',
  value: 'abc',
  domain: '.example.com',
  path: '/',
  secure: true,
  hostOnly: false,
  httpOnly: true,
  session: false,
  expirationDate: 1767225600.5,
  ...over,
});

test('a persistent cookie maps with its fractional expiry', () => {
  assert.deepEqual(toContextCookie(cookie()), {
    name: 'SID',
    value: 'abc',
    domain: '.example.com',
    path: '/',
    secure: true,
    host_only: false,
    expires: 1767225600.5,
  });
});

test('a session cookie has no expiry, and host-only is carried', () => {
  const mapped = toContextCookie(cookie({ session: true, expirationDate: undefined, hostOnly: true }));
  assert.equal('expires' in mapped, false);
  assert.equal(mapped.host_only, true);
});

test('cookies from both URLs are de-duplicated by name, domain and path', () => {
  const list = [cookie(), cookie({ value: 'second' }), cookie({ path: '/a' }), cookie({ domain: 'x.com' })];
  const unique = dedupeCookies(list);
  assert.equal(unique.length, 3);
  assert.equal(unique[0].value, 'abc');
});

test('the download message carries the final url, referrer, user agent and cookies', () => {
  const item = {
    url: 'https://example.com/get',
    finalUrl: 'https://cdn.example.com/x.zip',
    referrer: 'https://example.com/files',
    filename: 'C:\\Users\\me\\Downloads\\x.zip',
  };
  assert.deepEqual(
    buildDownloadMessage({ id: 7, item, cookies: [cookie()], userAgent: 'UA/1' }),
    {
      type: 'add',
      id: 7,
      url: 'https://cdn.example.com/x.zip',
      filename: 'x.zip',
      context: {
        cookies: [toContextCookie(cookie())],
        referer: 'https://example.com/files',
        user_agent: 'UA/1',
      },
    },
  );
});

test('missing pieces are left out rather than sent empty', () => {
  const message = buildDownloadMessage({
    id: 1,
    item: { url: 'https://example.com/x', referrer: '', filename: '' },
    cookies: [],
    userAgent: 'UA/1',
  });
  assert.deepEqual(message, {
    type: 'add',
    id: 1,
    url: 'https://example.com/x',
    context: { user_agent: 'UA/1' },
  });
});

test('a non-web referrer is not sent', () => {
  const message = buildDownloadMessage({
    id: 1,
    item: { url: 'https://example.com/x', referrer: 'chrome://newtab/' },
    cookies: [],
    userAgent: 'UA',
  });
  assert.equal('referer' in message.context, false);
});

test('file names the host would refuse are dropped', () => {
  assert.equal(safeFilename('/home/me/Downloads/a b.zip'), 'a b.zip');
  assert.equal(safeFilename('a..b.zip'), undefined);
  assert.equal(safeFilename('dir/'), undefined);
  assert.equal(safeFilename('x\u0007y'), undefined);
  assert.equal(safeFilename('n'.repeat(256)), undefined);
  assert.equal(safeFilename('n'.repeat(255)), 'n'.repeat(255));
  assert.equal(safeFilename(undefined), undefined);
});

test('the menu message uses the page as the referer', () => {
  assert.deepEqual(
    buildMenuMessage({
      id: 3,
      url: 'https://example.com/v.mp4',
      pageUrl: 'https://example.com/watch',
      cookies: [],
      userAgent: 'UA',
    }),
    {
      type: 'add',
      id: 3,
      url: 'https://example.com/v.mp4',
      context: { referer: 'https://example.com/watch', user_agent: 'UA' },
    },
  );
});

test('a title is trimmed into the context', () => {
  assert.equal(buildContext({ title: '  Angel One \n' }).title, 'Angel One');
});

test('an empty title is left out', () => {
  assert.equal('title' in buildContext({ title: '   ' }), false);
  assert.equal('title' in buildContext({}), false);
});

test('a long title is sent whole', () => {
  assert.equal(buildContext({ title: 'a'.repeat(5000) }).title.length, 5000);
});

test('the video message has no id and no referer when none is given', () => {
  const message = buildVideoMessage({ url: 'https://e.com/v', title: 'T', userAgent: 'UA' });
  assert.deepEqual(message, {
    type: 'add',
    url: 'https://e.com/v',
    context: { user_agent: 'UA', title: 'T' },
  });
});

test('the video message carries the referer and cookies', () => {
  const message = buildVideoMessage({
    url: 'https://e.com/v.m3u8',
    referer: 'https://e.com/watch',
    cookies: [cookie()],
  });
  assert.equal(message.context.referer, 'https://e.com/watch');
  assert.equal(message.context.cookies.length, 1);
});

test('status and choose messages have the host field names', () => {
  assert.deepEqual(buildStatusMessage('dl_1'), { type: 'status', download_id: 'dl_1' });
  assert.deepEqual(buildChooseMessage('dl_1', '720-mp4'), {
    type: 'choose_quality',
    download_id: 'dl_1',
    choice_id: '720-mp4',
  });
});

test('look_up message carries the context', () => {
  const message = buildLookUpMessage({ url: 'https://e.com/v', referer: 'https://e.com/', title: 'T', userAgent: 'UA' });
  assert.deepEqual(message, {
    type: 'look_up',
    url: 'https://e.com/v',
    context: { referer: 'https://e.com/', user_agent: 'UA', title: 'T' },
  });
});

test('video message carries choice_id and page', () => {
  const message = buildVideoMessage({ url: 'https://e.com/v.html', choiceId: '1080-mp4', page: true });
  assert.equal(message.choice_id, '1080-mp4');
  assert.equal(message.page, true);
  const plain = buildVideoMessage({ url: 'https://e.com/v' });
  assert.equal('choice_id' in plain, false);
  assert.equal('page' in plain, false);
});

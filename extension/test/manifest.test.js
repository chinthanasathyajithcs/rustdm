import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('../', import.meta.url));
const read = (path) => readFileSync(new URL(path, import.meta.url), 'utf8');
const manifest = JSON.parse(read('../manifest.json'));

// Chrome derives an extension's ID from its public key: the first 16 bytes of
// the SHA-256 of the DER key, each hex digit written as a letter a to p.
function extensionIdFor(base64Key) {
  const digest = createHash('sha256').update(Buffer.from(base64Key, 'base64')).digest('hex');
  return [...digest.slice(0, 32)].map((c) => String.fromCharCode(97 + parseInt(c, 16))).join('');
}

test('the manifest key produces an ID that the host allows', () => {
  const source = read('../../src/nmhost/origin.rs');
  const list = source.match(/ALLOWED_EXTENSION_IDS: &\[&str\] = &\[([^\]]*)\]/);
  assert.ok(list, 'ALLOWED_EXTENSION_IDS found in origin.rs');
  const allowed = [...list[1].matchAll(/"([a-p]{32})"/g)].map((m) => m[1]);
  assert.ok(allowed.includes(extensionIdFor(manifest.key)), `allowed: ${allowed}`);
});

test('permissions are exactly the documented set', () => {
  assert.deepEqual(
    [...manifest.permissions].sort(),
    ['contextMenus', 'cookies', 'downloads', 'nativeMessaging', 'storage', 'webRequest'],
  );
  assert.deepEqual(manifest.optional_host_permissions, ['<all_urls>']);
  assert.equal(manifest.host_permissions, undefined);
});

test('nothing lets a web page reach the extension', () => {
  for (const field of [
    'externally_connectable',
    'content_scripts',
    'web_accessible_resources',
    'content_security_policy',
  ]) {
    assert.equal(manifest[field], undefined, field);
  }
});

test('the worker is a module and the panel serves as popup and options page', () => {
  assert.equal(manifest.manifest_version, 3);
  assert.deepEqual(manifest.background, { service_worker: 'background.js', type: 'module' });
  assert.equal(manifest.action.default_popup, 'panel.html');
  assert.equal(manifest.options_ui.page, 'panel.html');
});

test('every file the manifest names exists', () => {
  const paths = [
    manifest.background.service_worker,
    manifest.action.default_popup,
    manifest.options_ui.page,
    ...Object.values(manifest.icons),
    ...Object.values(manifest.action.default_icon),
  ];
  for (const path of paths) assert.doesNotThrow(() => readFileSync(root + path), path);
});

test('the panel puts text in with textContent only', () => {
  const panel = read('../panel.js');
  assert.ok(!/innerHTML|outerHTML|insertAdjacentHTML|document\.write|eval\(/.test(panel));
  assert.match(panel, /textContent/);
  assert.ok(!/<script(?![^>]*\bsrc=)[^>]*>/.test(read('../panel.html')), 'no inline script');
});

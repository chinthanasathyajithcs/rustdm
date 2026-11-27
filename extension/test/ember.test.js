// The browser parts use the dashboard's Ember colours (src/tui/theme.rs), so
// these tests read that file and keep the two from drifting apart.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';

const read = (path) => readFileSync(new URL(path, import.meta.url), 'utf8');
const context = {};
vm.runInNewContext(read('../content/ember.css.js'), context);
const ember = context.rdmEmber;

const theme = read('../../src/tui/theme.rs');
const themeColours = Object.fromEntries(
  [...theme.matchAll(/pub const (\w+): Color = Color::Rgb\(0x([0-9a-f]{2}), 0x([0-9a-f]{2}), 0x([0-9a-f]{2})\)/gi)].map(
    ([, name, r, g, b]) => [name, `#${r}${g}${b}`.toLowerCase()],
  ),
);
const USED = ['QUIT_BG', 'SELECTED_BG', 'BUTTON_BG', 'FRAME', 'RULE', 'BRIGHT', 'MUTED', 'FAINT', 'AMBER', 'ON_AMBER', 'SAGE', 'RED'];
const hexes = (text) => [...text.matchAll(/#[0-9a-f]{3,8}\b/gi)].map(([hex]) => hex.toLowerCase());

test('the palette equals theme.rs', () => {
  assert.deepEqual(Object.keys(ember.palette).sort(), [...USED].sort());
  for (const name of USED) {
    assert.ok(themeColours[name], `${name} found in theme.rs`);
    assert.equal(ember.palette[name], themeColours[name], name);
  }
});

test('the css uses no other colours', () => {
  const allowed = new Set(Object.values(ember.palette));
  for (const hex of hexes(ember.css)) assert.ok(allowed.has(hex), `${hex} is not an Ember colour`);
  assert.ok(ember.css.includes('var(--rdm-amber)'));
});

test('the css fetches nothing', () => {
  assert.doesNotMatch(ember.css, /url\(|@import|@font-face/);
});

test('the crab is the small icon', () => {
  assert.equal(ember.crab, read('../../assets/icon/rustdm-small.svg'));
});

test('the panel uses no other colours', () => {
  const allowed = new Set(Object.values(ember.palette));
  for (const hex of hexes(read('../panel.html'))) assert.ok(allowed.has(hex), `${hex} in panel.html`);
});

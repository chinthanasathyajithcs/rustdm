// rustdm's Ember look for the bar, the card and the panel. The colours are
// copied from the `palette` constants in src/tui/theme.rs (the dashboard);
// test/ember.test.js keeps them equal. Classic script: sets globalThis.rdmEmber.
'use strict';

(() => {
  const palette = Object.freeze({
    QUIT_BG: '#201c18',
    SELECTED_BG: '#2a2219',
    BUTTON_BG: '#2c2721',
    FRAME: '#4a433a',
    RULE: '#39342d',
    BRIGHT: '#f5efe4',
    MUTED: '#8e8678',
    FAINT: '#776f63',
    AMBER: '#e5a04f',
    ON_AMBER: '#1a140c',
    SAGE: '#9cbf78',
    RED: '#e05a4f',
  });

  const css = `
:host {
  all: initial;
  --rdm-quit-bg: #201c18;
  --rdm-selected-bg: #2a2219;
  --rdm-button-bg: #2c2721;
  --rdm-frame: #4a433a;
  --rdm-rule: #39342d;
  --rdm-bright: #f5efe4;
  --rdm-muted: #8e8678;
  --rdm-faint: #776f63;
  --rdm-amber: #e5a04f;
  --rdm-on-amber: #1a140c;
  --rdm-sage: #9cbf78;
  --rdm-red: #e05a4f;
  --rdm-ui: system-ui, "Segoe UI", sans-serif;
  --rdm-mono: ui-monospace, "Cascadia Mono", Consolas, monospace;
}
* { box-sizing: border-box; margin: 0; font: inherit; color: inherit; }
.layer { position: fixed; inset: 0 auto auto 0; font: 13px/1.35 var(--rdm-ui); color: var(--rdm-bright); }
button { all: unset; box-sizing: border-box; cursor: pointer; font: inherit; color: inherit; }
button:focus-visible { outline: 2px solid var(--rdm-amber); outline-offset: 1px; }
.bar {
  position: fixed; display: flex; align-items: center; height: 26px; padding: 0;
  background: var(--rdm-quit-bg); border: 1px solid var(--rdm-frame); border-radius: 6px;
  white-space: nowrap; user-select: none; opacity: 0.78;
}
.bar:hover, .bar:focus-within, .bar.open { border-color: var(--rdm-amber); opacity: 1; }
.bar .go { display: flex; align-items: center; gap: 6px; height: 24px; padding: 0 9px; border-radius: 5px; color: var(--rdm-bright); }
.bar .go svg { width: 16px; height: 16px; flex: none; }
.bar .x { visibility: hidden; margin: 0 3px 0 -4px; }
.bar:hover .x, .bar:focus-within .x { visibility: visible; }
.x { display: grid; place-items: center; width: 20px; height: 20px; border-radius: 4px; color: var(--rdm-muted); font-size: 12px; }
.x:hover { color: var(--rdm-bright); background: var(--rdm-button-bg); }
.head .crab { width: 16px; height: 16px; flex: none; }
.card {
  position: fixed; width: 300px; max-height: min(420px, calc(100vh - 16px)); display: flex; flex-direction: column;
  background: var(--rdm-quit-bg); border: 1px solid var(--rdm-frame); border-radius: 6px;
  box-shadow: 0 8px 28px rgb(0 0 0 / 50%); overflow: hidden;
}
.head { display: flex; align-items: center; gap: 8px; padding: 8px 6px 8px 10px; border-bottom: 1px solid var(--rdm-rule); }
.badge { flex: none; padding: 1px 6px; border-radius: 3px; background: var(--rdm-amber); color: var(--rdm-on-amber); font-weight: 700; }
.title { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.body { padding: 8px 10px; overflow-y: auto; }
.message { padding: 4px 0; }
.looking { color: var(--rdm-muted); }
.started { color: var(--rdm-sage); }
.failed { color: var(--rdm-red); }
.note { color: var(--rdm-bright); }
.mono { font-family: var(--rdm-mono); }
.file { display: block; margin-top: 2px; color: var(--rdm-bright); word-break: break-all; }
.duration { color: var(--rdm-faint); padding: 0 0 6px; }
.rows { display: flex; flex-direction: column; gap: 1px; }
.row { display: grid; grid-template-columns: 12px 1fr auto auto; gap: 8px; align-items: center; padding: 5px 6px; border-radius: 4px; }
.row:hover, .row:focus-visible { background: var(--rdm-selected-bg); }
.row.default { background: var(--rdm-selected-bg); }
.row .marker { color: var(--rdm-amber); }
.row .container { color: var(--rdm-muted); font-family: var(--rdm-mono); }
.row .size { color: var(--rdm-muted); font-family: var(--rdm-mono); text-align: right; min-width: 72px; }
.row[aria-disabled="true"] { cursor: default; opacity: 0.6; }
.foot { border-top: 1px solid var(--rdm-rule); padding: 6px 10px 8px; }
.toggle { color: var(--rdm-muted); }
.toggle:hover { color: var(--rdm-bright); }
.others { display: flex; flex-direction: column; gap: 1px; padding-top: 4px; }
.other { padding: 4px 6px; border-radius: 4px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-family: var(--rdm-mono); color: var(--rdm-muted); }
.other:hover, .other:focus-visible { background: var(--rdm-selected-bg); color: var(--rdm-bright); }
[hidden] { display: none !important; }
`;

  // assets/icon/rustdm-small.svg, inlined so no extension file is ever loaded
  // by a page (test/ember.test.js checks it is the same file).
  const crab = "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 256 256\" width=\"256\" height=\"256\">\n  <!-- rustdm icon, small (16-24 px): fewer, bolder shapes; same colours and light. -->\n  <defs>\n    <radialGradient id=\"shell\" cx=\"0.36\" cy=\"0.26\" r=\"0.85\">\n      <stop offset=\"0\" stop-color=\"#ffc48c\"/><stop offset=\"0.35\" stop-color=\"#ff8a3d\"/><stop offset=\"0.8\" stop-color=\"#d4520f\"/><stop offset=\"1\" stop-color=\"#a3380b\"/>\n    </radialGradient>\n    <linearGradient id=\"gold\" x1=\"0.2\" y1=\"0\" x2=\"0.8\" y2=\"1\">\n      <stop offset=\"0\" stop-color=\"#fff4d2\"/><stop offset=\"0.5\" stop-color=\"#ffd27a\"/><stop offset=\"1\" stop-color=\"#e09a32\"/>\n    </linearGradient>\n  </defs>\n  <g fill=\"none\" stroke=\"#d65a17\" stroke-linecap=\"round\" stroke-width=\"20\">\n    <path d=\"M58 176 L22 204\"/><path d=\"M198 176 L234 204\"/>\n    <path d=\"M84 196 L66 232\"/><path d=\"M172 196 L190 232\"/>\n    <path d=\"M56 140 L30 104\"/><path d=\"M200 140 L226 104\"/>\n  </g>\n  <g fill=\"#f2742c\">\n    <path d=\"M10 92 C6 60 18 34 36 22 C40 40 40 56 36 66 C46 58 54 46 58 32 C70 50 66 82 48 98 C34 108 14 106 10 92 Z\"/>\n    <path d=\"M246 92 C250 60 238 34 220 22 C216 40 216 56 220 66 C210 58 202 46 198 32 C186 50 190 82 208 98 C222 108 242 106 246 92 Z\"/>\n  </g>\n  <g stroke=\"#d65a17\" stroke-width=\"14\" stroke-linecap=\"round\"><path d=\"M104 90 L96 52\"/><path d=\"M152 90 L160 52\"/></g>\n  <path d=\"M24 152 C24 102 72 80 128 80 C184 80 232 102 232 152 C232 196 186 218 128 218 C70 218 24 196 24 152 Z\" fill=\"url(#shell)\"/>\n  <path d=\"M109 104 H147 V146 H174 L128 200 L82 146 H109 Z\" fill=\"#7a2604\" opacity=\".55\" transform=\"translate(-3 -3)\" stroke=\"#7a2604\" stroke-width=\"6\" stroke-linejoin=\"round\"/>\n  <path d=\"M109 104 H147 V146 H174 L128 200 L82 146 H109 Z\" fill=\"url(#gold)\" stroke=\"url(#gold)\" stroke-width=\"6\" stroke-linejoin=\"round\"/>\n  <circle cx=\"94\" cy=\"40\" r=\"25\" fill=\"#f08a48\"/><circle cx=\"162\" cy=\"40\" r=\"25\" fill=\"#f08a48\"/><circle cx=\"94\" cy=\"40\" r=\"20\" fill=\"#140c09\"/><circle cx=\"162\" cy=\"40\" r=\"20\" fill=\"#140c09\"/>\n  <circle cx=\"87\" cy=\"32\" r=\"7\" fill=\"#fff\"/><circle cx=\"155\" cy=\"32\" r=\"7\" fill=\"#fff\"/>\n</svg>\n";

  globalThis.rdmEmber = Object.freeze({ palette, css, crab });
})();

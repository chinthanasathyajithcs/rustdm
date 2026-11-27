// The "Download this video" bar and its card, drawn on the page inside one
// closed shadow root (spec 8 and 9). The rules live in hover-logic.js; this
// file only reads the DOM and draws. Text goes in with textContent only, clicks
// count only when trusted, and nothing is posted to the page: the card talks
// to the extension background over one runtime port.
'use strict';

(() => {
  const H = globalThis.rdmHover;
  const E = globalThis.rdmEmber;
  if (!H || !E || globalThis.rdmHoverStarted || !globalThis.chrome?.runtime?.connect) return;
  globalThis.rdmHoverStarted = true;

  const CARD_PORT = 'rdm-card';
  const TICK_MS = 250;
  const BAR_TEXT = 'Download this video';
  const BAR_HINT = 'Download this video with rustdm';
  const LOOKING = 'Looking up the video…';
  const PROTECTED = "This video is protected and can't be saved.";

  const memory = { dismissed: new WeakSet(), played: new WeakSet() };
  const tracked = new Set();
  let bar = H.createBar();
  let ui = null; // {host, layer, barEl, cardEl}
  let card = null; // {video, port, view, othersOpen}
  let ticker = null;
  let pointer = null;
  let frameQueued = false;
  let crabTemplate = null;
  let followFrame = 0;
  let recheck = 0;
  // The page script is registered only while both switches are on; a switch
  // turned off later stops it in pages that are already open.
  const switches = { enabled: true, videoBar: true };
  let off = false;

  const now = () => Date.now();
  const viewport = () => ({ width: document.documentElement.clientWidth || innerWidth, height: document.documentElement.clientHeight || innerHeight });
  const isVisible = (el) =>
    typeof el.checkVisibility === 'function' ? el.checkVisibility({ opacityProperty: true, visibilityProperty: true }) : true;

  function el(tag, className, text) {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined) node.textContent = text;
    return node;
  }

  function onTrusted(node, type, handler) {
    node.addEventListener(type, (event) => {
      if (!event.isTrusted) return;
      event.preventDefault();
      event.stopPropagation();
      handler(event);
    });
  }

  function icon(text, kept) {
    const template = kept ?? new DOMParser().parseFromString(text, 'image/svg+xml').documentElement;
    const svg = document.importNode(template, true);
    svg.setAttribute('aria-hidden', 'true');
    return { svg, template };
  }

  function crab() {
    const { svg, template } = icon(E.crab, crabTemplate);
    crabTemplate = template;
    svg.setAttribute('class', 'crab');
    return svg;
  }


  // --- Videos on the page ---------------------------------------------------

  function track(video) {
    if (tracked.has(video)) return;
    tracked.add(video);
    video.addEventListener('playing', onPlaying);
  }

  function trackWithin(node) {
    if (node.nodeType !== Node.ELEMENT_NODE) return;
    if (node.tagName === 'VIDEO') track(node);
    else node.querySelectorAll?.('video').forEach(track);
  }

  function onPlaying(event) {
    if (off) return;
    const video = event.currentTarget;
    const seen = { muted: video.muted, volume: video.volume, rect: video.getBoundingClientRect(), visible: isVisible(video) };
    if (H.shouldShowOnPlay(seen)) dispatch({ type: 'played', video, now: now() });
  }

  // Topmost first: the videos the browser finds under the pointer, then any
  // other tracked video there (a player layer with pointer-events: none).
  function candidatesAt(x, y) {
    for (const video of tracked) if (!video.isConnected) tracked.delete(video);
    if (tracked.size === 0) return [];
    const seen = new Set();
    const ordered = [];
    for (const node of document.elementsFromPoint(x, y)) {
      if (node.tagName === 'VIDEO' && tracked.has(node)) {
        seen.add(node);
        ordered.push(node);
      }
    }
    for (const video of [...tracked].reverse()) if (!seen.has(video)) ordered.push(video);
    return ordered.map((video) => ({ video, rect: video.getBoundingClientRect(), visible: () => isVisible(video) }));
  }

  // The bar counts a few pixels around it, so the gap to the video is part of it.
  function insideOwnUi(x, y) {
    const bar = ui?.barEl?.isConnected && H.inflate(ui.barEl.getBoundingClientRect(), H.GAP_PX + 2);
    const cardBox = ui?.cardEl?.isConnected && ui.cardEl.getBoundingClientRect();
    return [bar, cardBox].some((box) => box && H.containsPoint(box, x, y));
  }

  function onPointer() {
    frameQueued = false;
    if (!pointer || off || (tracked.size === 0 && !bar.video)) return;
    if (!chrome.runtime?.id) {
      stopForGood();
      return;
    }
    const { x, y } = pointer;
    if (bar.video && insideOwnUi(x, y)) {
      dispatch({ type: 'hover', video: bar.video, now: now() });
      return;
    }
    const found = H.pickHovered(candidatesAt(x, y), x, y);
    if (found) dispatch({ type: 'hover', video: found.video, now: now() });
    else if (bar.video || bar.pending) dispatch({ type: 'away', now: now() });
    // A pointer resting on a video sends no more moves: look again once the wait is over.
    clearTimeout(recheck);
    if (bar.pending) recheck = setTimeout(onPointer, H.HOVER_DELAY_MS + 10);
  }

  // --- The bar ------------------------------------------------------------

  function dispatch(event) {
    bar = H.barStep(bar, event, memory);
    draw();
  }

  function ensureUi() {
    if (ui) return ui;
    const host = document.createElement('div');
    for (const [name, value] of [['all', 'initial'], ['position', 'fixed'], ['inset', '0 auto auto 0'], ['width', '0'], ['height', '0'], ['z-index', '2147483647']]) {
      host.style.setProperty(name, value, 'important');
    }
    const shadow = host.attachShadow({ mode: 'closed' });
    if ('adoptedStyleSheets' in shadow && typeof CSSStyleSheet === 'function') {
      const sheet = new CSSStyleSheet();
      sheet.replaceSync(E.css);
      shadow.adoptedStyleSheets = [sheet];
    } else {
      shadow.append(el('style', '', E.css));
    }
    const layer = el('div', 'layer');
    // Keys pressed in the card stay in it (a video page's shortcuts would
    // seek or change the volume); Escape still reaches the page.
    for (const type of ['keydown', 'keyup', 'keypress']) {
      layer.addEventListener(type, (event) => {
        if (event.key !== 'Escape') event.stopPropagation();
      });
    }
    shadow.append(layer);
    ui = { host, layer, barEl: null, cardEl: null };
    placeHost();
    return ui;
  }

  function placeHost() {
    if (!ui) return;
    const parent = H.fullscreenParent(document.fullscreenElement, document.documentElement);
    if (ui.host.parentNode !== parent) parent.append(ui.host);
  }

  function buildBar() {
    const root = el('div', 'bar');
    const go = el('button', 'go');
    go.type = 'button';
    go.title = BAR_HINT;
    go.setAttribute('aria-label', BAR_HINT);
    go.append(crab(), el('span', '', BAR_TEXT));
    onTrusted(go, 'click', () => toggleCard(bar.video));
    const close = el('button', 'x', '✕');
    close.type = 'button';
    close.setAttribute('aria-label', 'Hide for this video');
    onTrusted(close, 'click', () => {
      const video = bar.video;
      if (card?.video === video) closeCard();
      dispatch({ type: 'dismiss', video });
    });
    root.append(go, close);
    return root;
  }

  function draw() {
    if (!bar.video && !card) {
      removeUi();
      return;
    }
    const { layer } = ensureUi();
    if (bar.video) {
      ui.barEl ??= layer.appendChild(buildBar());
      ui.barEl.classList.toggle('open', Boolean(card));
      placeBar();
    } else if (ui.barEl) {
      ui.barEl.remove();
      ui.barEl = null;
    }
    if (card) placeCard();
    ticker ??= setInterval(onTick, TICK_MS);
    followFrame ||= requestAnimationFrame(follow);
  }

  // While shown, the bar and card move with the video in the same frame the
  // page scrolls or the player resizes, instead of catching up later.
  function follow() {
    followFrame = 0;
    if (!ui) return;
    placeBar();
    if (card) placeCard();
    followFrame = requestAnimationFrame(follow);
  }

  function moveTo(node, left, top) {
    const x = `${left}px`;
    const y = `${top}px`;
    if (node.style.left !== x) node.style.left = x;
    if (node.style.top !== y) node.style.top = y;
  }

  function placeBar() {
    if (!ui?.barEl || !bar.video) return;
    const box = ui.barEl.getBoundingClientRect();
    const size = { width: box.width, height: box.height };
    const place = H.barPlacement(bar.video.getBoundingClientRect(), viewport(), size);
    moveTo(ui.barEl, place.left, place.top);
  }

  function onTick() {
    const video = bar.video;
    if (video) {
      const rect = video.getBoundingClientRect();
      const { width, height } = viewport();
      const away = !video.isConnected || rect.bottom <= 0 || rect.top >= height || rect.right <= 0 || rect.left >= width;
      dispatch(away ? { type: 'gone', video } : { type: 'tick', now: now() });
    } else {
      draw();
    }
  }

  function removeUi() {
    clearInterval(ticker);
    ticker = null;
    cancelAnimationFrame(followFrame);
    followFrame = 0;
    ui?.host.remove();
    ui = null;
  }

  // --- The card -----------------------------------------------------------

  function toggleCard(video) {
    if (!video) return;
    if (card?.video === video) {
      closeCard();
      return;
    }
    closeCard();
    const title = document.title || location.host;
    // A protected (DRM) video can't be saved by anyone: say so without asking rustdm.
    if (video.mediaKeys) {
      card = { video, port: null, view: { phase: 'failed', title, message: PROTECTED, others: [] }, othersOpen: false };
      dispatch({ type: 'card', open: true, now: now() });
      renderCard();
      return;
    }
    let port;
    try {
      port = chrome.runtime.connect({ name: CARD_PORT });
    } catch {
      stopForGood(); // the extension was updated or removed: this copy is left over
      return;
    }
    card = { video, port, view: { phase: 'looking', title, others: [] }, othersOpen: false };
    port.onMessage.addListener((message) => {
      if (card?.port !== port || message?.type !== 'view' || typeof message.view !== 'object') return;
      card.view = message.view;
      card.othersOpen = false;
      renderCard();
    });
    port.onDisconnect.addListener(() => {
      if (card?.port === port) closeCard();
    });
    const box = video.getBoundingClientRect();
    port.postMessage({
      type: 'open',
      currentSrc: String(video.currentSrc || ''),
      at: { x: Math.round(box.left + box.width / 2), y: Math.round(box.top + box.height / 2) },
      link: H.videoLink(video, location.href),
      videos: document.querySelectorAll('video').length,
    });
    dispatch({ type: 'card', open: true, now: now() });
    renderCard();
  }

  function closeCard() {
    if (!card) return;
    const { port } = card;
    card = null;
    try {
      port?.disconnect();
    } catch {
      // Already gone.
    }
    ui?.cardEl?.remove();
    if (ui) ui.cardEl = null;
    dispatch({ type: 'card', open: false, now: now() });
  }

  function send(message) {
    try {
      card?.port?.postMessage(message);
    } catch {
      closeCard();
    }
  }

  function messageLine(className, text) {
    return el('div', `message ${className}`, text);
  }

  function rowsFor(view) {
    const rows = el('div', 'rows');
    rows.setAttribute('role', 'list');
    view.choices.forEach((choice, index) => {
      const cells = H.choiceRow(choice);
      const row = el('button', index === view.defaultIndex ? 'row default' : 'row');
      row.type = 'button';
      row.append(
        el('span', 'marker', index === view.defaultIndex ? '▸' : ''),
        el('span', 'label', cells.label),
        el('span', 'container', cells.container),
        el('span', 'size', cells.size),
      );
      onTrusted(row, 'click', () => {
        if (rows.dataset.sent) return;
        rows.dataset.sent = '1';
        rows.querySelectorAll('.row').forEach((r) => r.setAttribute('aria-disabled', 'true'));
        send({ type: 'choose', choiceId: String(choice.id) });
      });
      rows.append(row);
    });
    rows.addEventListener('keydown', (event) => {
      if (event.key !== 'ArrowDown' && event.key !== 'ArrowUp') return;
      const all = [...rows.querySelectorAll('.row')];
      const at = all.indexOf(event.target);
      if (at === -1) return;
      event.preventDefault();
      all[(at + (event.key === 'ArrowDown' ? 1 : all.length - 1)) % all.length].focus();
    });
    return rows;
  }

  function bodyFor(view) {
    const body = el('div', 'body');
    switch (view.phase) {
      case 'choosing':
        if (typeof view.durationSecs === 'number') body.append(el('div', 'duration mono', H.formatDuration(view.durationSecs)));
        body.append(rowsFor(view));
        break;
      case 'started': {
        const line = messageLine('started', 'Downloading in rustdm:');
        line.append(el('span', 'file mono', String(view.filename ?? '')));
        body.append(line);
        break;
      }
      case 'failed':
        body.append(messageLine('failed', String(view.message ?? '')));
        break;
      case 'note':
        body.append(messageLine('note', String(view.message ?? '')));
        break;
      default:
        body.append(messageLine('looking', LOOKING));
    }
    return body;
  }

  function footFor(view) {
    const others = Array.isArray(view.others) ? view.others : [];
    if (others.length === 0) return null;
    const foot = el('div', 'foot');
    const toggle = el('button', 'toggle', `Other sources (${others.length})`);
    toggle.type = 'button';
    toggle.setAttribute('aria-expanded', String(card.othersOpen));
    onTrusted(toggle, 'click', () => {
      card.othersOpen = !card.othersOpen;
      renderCard();
    });
    foot.append(toggle);
    if (card.othersOpen) {
      const list = el('div', 'others');
      for (const other of others) {
        const button = el('button', 'other', String(other.label ?? ''));
        button.type = 'button';
        onTrusted(button, 'click', () => {
          const source = other.key === 'page' ? { type: 'page' } : { type: 'caught', id: String(other.key) };
          card.view = { ...card.view, phase: 'looking' };
          card.othersOpen = false;
          renderCard();
          send({ type: 'use', source });
        });
        list.append(button);
      }
      foot.append(list);
    }
    return foot;
  }

  function renderCard() {
    if (!card) return;
    const { layer } = ensureUi();
    const view = card.view;
    const root = el('div', 'card');
    root.setAttribute('role', 'dialog');
    root.setAttribute('aria-label', 'rustdm');
    const head = el('div', 'head');
    const close = el('button', 'x', '✕');
    close.type = 'button';
    close.setAttribute('aria-label', 'Close');
    onTrusted(close, 'click', closeCard);
    head.append(crab(), el('span', 'badge', 'rustdm'), el('span', 'title', String(view.title ?? '')), close);
    root.append(head, bodyFor(view));
    const foot = footFor(view);
    if (foot) root.append(foot);
    const hadFocus = ui.cardEl?.contains(ui.layer.getRootNode().activeElement);
    if (ui.cardEl) ui.cardEl.replaceWith(root);
    else layer.append(root);
    ui.cardEl = root;
    draw();
    const focusTarget = root.querySelector('.row.default') ?? root.querySelector('.row') ?? (hadFocus ? close : null);
    focusTarget?.focus({ preventScroll: true });
  }

  function placeCard() {
    if (!ui?.cardEl) return;
    const size = ui.cardEl.getBoundingClientRect();
    const anchor = ui.barEl?.getBoundingClientRect() ?? ui.cardEl.getBoundingClientRect();
    const place = H.cardPlacement(anchor, viewport(), { width: size.width, height: size.height });
    moveTo(ui.cardEl, place.left, place.top);
  }

  function setSwitches(changes) {
    for (const key of Object.keys(switches)) {
      if (key in changes) switches[key] = changes[key].newValue !== false;
    }
    off = !(switches.enabled && switches.videoBar);
    if (off) hideAll();
  }

  function hideAll() {
    closeCard();
    bar = H.createBar();
    removeUi();
  }

  // After an extension update this copy can no longer reach the extension:
  // it takes its bar away and stays quiet until the page is reloaded.
  function stopForGood() {
    off = true;
    switches.enabled = false;
    hideAll();
  }

  // --- Page events --------------------------------------------------------

  chrome.storage?.onChanged?.addListener((changes, area) => {
    if (area === 'local') setSwitches(changes);
  });

  addEventListener(
    'pointermove',
    (event) => {
      pointer = { x: event.clientX, y: event.clientY };
      if (!frameQueued) {
        frameQueued = true;
        requestAnimationFrame(onPointer);
      }
    },
    { capture: true, passive: true },
  );
  document.documentElement.addEventListener('pointerleave', () => {
    pointer = null;
    if (bar.video || bar.pending) dispatch({ type: 'away', now: now() });
  });
  addEventListener(
    'pointerdown',
    (event) => {
      if (event.isTrusted && card && ui && !event.composedPath().includes(ui.host)) closeCard();
    },
    true,
  );
  addEventListener(
    'keydown',
    (event) => {
      if (event.isTrusted && card && event.key === 'Escape') closeCard();
    },
    true,
  );
  document.addEventListener('fullscreenchange', () => {
    placeHost();
    if (ui) draw();
  });

  document.querySelectorAll('video').forEach(track);
  new MutationObserver((records) => {
    for (const record of records) record.addedNodes.forEach(trackWithin);
    if (bar.video && !bar.video.isConnected) dispatch({ type: 'gone', video: bar.video });
  }).observe(document.documentElement, { childList: true, subtree: true });
})();

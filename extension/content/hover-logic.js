// Where and when the "Download this video" bar and its card show (spec 8.2,
// 9.1). Pure functions on globalThis.rdmHover: content scripts are classic
// scripts, and the tests run this file in a fresh `vm` context.
'use strict';

(() => {
  const MIN_HOVER = Object.freeze({ width: 200, height: 112 });
  const MIN_PLAY = Object.freeze({ width: 320, height: 180 });
  const HIDE_AFTER_MS = 1500;
  const PLAY_SHOW_MS = 4000;
  const INSET_PX = 8;
  const GAP_PX = 4;
  const FOUR_K_HEIGHT = 2160;
  const UNITS = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];

  const atLeast = (rect, min) => rect.width >= min.width && rect.height >= min.height;
  const clamp = (value, low, high) => Math.max(low, Math.min(value, Math.max(low, high)));

  function containsPoint(rect, x, y) {
    return x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom;
  }

  /** `candidates` come topmost first: `[{video, rect, visible}]`. */
  function pickHovered(candidates, x, y) {
    return candidates.find((c) => c.visible && atLeast(c.rect, MIN_HOVER) && containsPoint(c.rect, x, y)) ?? null;
  }

  /** Above the video's top-right corner, or inside it when there is no room above. */
  function barPlacement(videoRect, viewport, barSize) {
    const above = videoRect.top - barSize.height - GAP_PX;
    const inside = above < 0;
    const top = inside ? videoRect.top + INSET_PX : above;
    const left = videoRect.right - barSize.width - (inside ? INSET_PX : 0);
    return {
      left: clamp(left, 0, viewport.width - barSize.width),
      top: clamp(top, 0, viewport.height - barSize.height),
    };
  }

  /** Below the bar, or above it when it does not fit; right edges lined up. */
  function cardPlacement(barRect, viewport, cardSize) {
    const below = barRect.bottom + GAP_PX;
    const top = below + cardSize.height <= viewport.height ? below : barRect.top - cardSize.height - GAP_PX;
    return {
      left: clamp(barRect.right - cardSize.width, 0, viewport.width - cardSize.width),
      top: clamp(top, 0, viewport.height - cardSize.height),
    };
  }

  function shouldShowOnPlay({ muted, volume, rect, visible }) {
    return !muted && volume > 0 && Boolean(visible) && atLeast(rect, MIN_PLAY);
  }

  function createBar() {
    return { video: null, mode: null, hideAt: null, cardOpen: false };
  }

  const hidden = (bar) => ({ ...bar, video: null, mode: null, hideAt: null });

  /**
   * One event in, the next bar out. `memory` holds two WeakSets kept for the
   * page's life: `dismissed` (✕ pressed) and `played` (shown on first play).
   */
  function barStep(bar, event, memory) {
    switch (event.type) {
      case 'hover':
        if (memory.dismissed.has(event.video)) return bar.video === event.video ? hidden(bar) : bar;
        return { ...bar, video: event.video, mode: 'hover', hideAt: null };
      case 'away':
        if (!bar.video || bar.mode !== 'hover' || bar.hideAt !== null) return bar;
        return { ...bar, hideAt: event.now + HIDE_AFTER_MS };
      case 'played':
        if (memory.dismissed.has(event.video) || memory.played.has(event.video)) return bar;
        memory.played.add(event.video);
        return { ...bar, video: event.video, mode: 'play', hideAt: event.now + PLAY_SHOW_MS };
      case 'tick':
        if (!bar.video || bar.cardOpen || bar.hideAt === null || event.now < bar.hideAt) return bar;
        return hidden(bar);
      case 'dismiss':
        memory.dismissed.add(event.video);
        return bar.video === event.video ? hidden(bar) : bar;
      case 'gone':
        return bar.video === event.video ? hidden(bar) : bar;
      case 'card':
        if (event.open) return { ...bar, cardOpen: true, hideAt: null };
        return { ...bar, cardOpen: false, hideAt: bar.video ? event.now + HIDE_AFTER_MS : null };
      default:
        return bar;
    }
  }

  /** Where the shadow host lives in fullscreen: not inside a bare <video>. */
  function fullscreenParent(fullscreenElement, root) {
    return fullscreenElement && fullscreenElement.tagName !== 'VIDEO' ? fullscreenElement : root;
  }

  /** The dashboard's format_bytes: binary units, one decimal. */
  function formatBytes(bytes) {
    let value = bytes;
    let unit = 0;
    while (value >= 1024 && unit < UNITS.length - 1) {
      value /= 1024;
      unit += 1;
    }
    return unit === 0 ? `${bytes} B` : `${value.toFixed(1)} ${UNITS[unit]}`;
  }

  /** The dashboard's format_duration: "m:ss", or "h:mm:ss" from an hour. */
  function formatDuration(secs) {
    const two = (n) => String(n).padStart(2, '0');
    const hours = Math.floor(secs / 3600);
    const minutes = Math.floor((secs % 3600) / 60);
    const seconds = Math.floor(secs % 60);
    return hours > 0 ? `${hours}:${two(minutes)}:${two(seconds)}` : `${minutes}:${two(seconds)}`;
  }

  /** A quality row's three columns, as the dashboard's quality dialog shows them. */
  function choiceRow(choice) {
    const bytes = typeof choice.size_bytes === 'number' ? choice.size_bytes : null;
    const size = bytes === null ? '' : `${choice.size_is_estimate ? '~' : ''}${formatBytes(bytes)}`;
    const fourK = typeof choice.height === 'number' && choice.height >= FOUR_K_HEIGHT ? ' (4K)' : '';
    return { label: String(choice.label ?? ''), container: String(choice.container ?? ''), size: `${size}${fourK}`.trim() };
  }

  globalThis.rdmHover = Object.freeze({
    MIN_HOVER,
    MIN_PLAY,
    HIDE_AFTER_MS,
    PLAY_SHOW_MS,
    INSET_PX,
    GAP_PX,
    containsPoint,
    pickHovered,
    barPlacement,
    cardPlacement,
    shouldShowOnPlay,
    createBar,
    barStep,
    fullscreenParent,
    formatBytes,
    formatDuration,
    choiceRow,
  });
})();

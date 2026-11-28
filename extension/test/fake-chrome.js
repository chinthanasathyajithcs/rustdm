// A small in-memory `chrome` for testing video.js. Each call makes a fresh one.

export function event() {
  const listeners = [];
  const options = [];
  return {
    listeners,
    options,
    addListener: (fn, ...rest) => {
      listeners.push(fn);
      options.push(rest);
    },
    removeListener: (fn) => listeners.splice(listeners.indexOf(fn), 1),
    hasListener: (fn) => listeners.includes(fn),
    fire: (...args) => listeners.map((fn) => fn(...args)),
  };
}

function storageArea(name, data, onChanged) {
  const pick = (keys) => {
    if (keys === null || keys === undefined) return { ...data };
    if (typeof keys === 'string') return keys in data ? { [keys]: data[keys] } : {};
    if (Array.isArray(keys)) return Object.fromEntries(keys.filter((k) => k in data).map((k) => [k, data[k]]));
    return Object.fromEntries(Object.entries(keys).map(([k, d]) => [k, k in data ? data[k] : d]));
  };
  return {
    data,
    get: async (keys) => structuredClone(pick(keys)),
    set: async (values) => {
      const changes = {};
      for (const [k, v] of Object.entries(values)) {
        changes[k] = { oldValue: data[k], newValue: v };
        data[k] = structuredClone(v);
      }
      onChanged.fire(changes, name);
    },
    remove: async (keys) => {
      const changes = {};
      for (const k of [keys].flat()) {
        if (!(k in data)) continue;
        changes[k] = { oldValue: data[k] };
        delete data[k];
      }
      onChanged.fire(changes, name);
    },
  };
}

// A native or card port. `peer` gets what this side posts.
export function fakePort({ name, sender } = {}) {
  const port = {
    name,
    sender,
    posted: [],
    disconnected: false,
    onMessage: event(),
    onDisconnect: event(),
    postMessage: (message) => {
      if (port.disconnected) throw new Error('Attempting to use a disconnected port object');
      port.posted.push(structuredClone(message));
    },
    disconnect: () => {
      port.disconnected = true;
    },
    // The other side sends a message / goes away.
    deliver: (message) => port.onMessage.fire(message, port),
    drop: () => {
      port.disconnected = true;
      port.onDisconnect.fire(port);
    },
  };
  return port;
}

export function makeChrome({ granted = true, local = {}, session = {} } = {}) {
  const onChanged = event();
  const state = { granted, badge: '', registered: [], nativePorts: [], log: [] };
  const chrome = {
    state,
    runtime: {
      id: 'rustdm-extension-id',
      lastError: undefined,
      onConnect: event(),
      onMessage: event(),
      connectNative: (host) => {
        const port = fakePort({ name: host });
        state.nativePorts.push(port);
        return port;
      },
      sendNativeMessage: async (host, message) => {
        state.log.push(['native', structuredClone(message)]);
        return state.nativeReply ?? { id: message.id, ok: true, result: { download_id: 'dl_1' } };
      },
    },
    storage: {
      onChanged,
      local: storageArea('local', local, onChanged),
      session: storageArea('session', session, onChanged),
    },
    permissions: {
      contains: async () => state.granted,
      onAdded: event(),
      onRemoved: event(),
    },
    webRequest: { onHeadersReceived: event(), onBeforeRequest: event() },
    tabs: {
      onUpdated: event(),
      onRemoved: event(),
      get: async (tabId) => {
        if (!state.tabUrls?.[tabId]) throw new Error(`No tab with id: ${tabId}.`);
        return { id: tabId, url: state.tabUrls[tabId] };
      },
    },
    contextMenus: {
      onClicked: event(),
      create: (options) => state.log.push(['menu.create', options]),
      update: (id, options) => state.log.push(['menu.update', id, options]),
    },
    cookies: { getAll: async () => state.cookies ?? [] },
    scripting: {
      getRegisteredContentScripts: async (filter) =>
        state.registered.filter((s) => !filter?.ids || filter.ids.includes(s.id)),
      registerContentScripts: async (scripts) => {
        state.log.push(['register', scripts.map((s) => s.id)]);
        state.registered.push(...structuredClone(scripts));
      },
      executeScript: async (injection) => {
        state.log.push(['executeScript', { target: injection.target, world: injection.world, args: injection.args }]);
        if (state.playerFails) throw new Error('Cannot access contents of the page.');
        return [{ frameId: injection.target.frameIds?.[0] ?? 0, result: state.playerId ?? '' }];
      },
      unregisterContentScripts: async (filter) => {
        state.log.push(['unregister', filter?.ids]);
        state.registered = state.registered.filter((s) => filter?.ids && !filter.ids.includes(s.id));
      },
    },
  };
  return chrome;
}

// Wait until every pending promise callback has run.
export const settle = () => new Promise((resolve) => setTimeout(resolve, 0));

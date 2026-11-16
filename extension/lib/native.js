// Talks to the native host. `send` is injected so this runs outside a browser.

export const HOST_NAME = 'rustdm.nmhost';
export const DEFAULT_TIMEOUT_MS = 15_000;

const failure = (kind, message) => ({ ok: false, error: { kind, message } });

function classify(error) {
  const text = String(error?.message ?? error ?? '');
  if (/not found/i.test(text)) return 'host_missing';
  if (/forbidden/i.test(text)) return 'host_forbidden';
  return 'host_error';
}

function normalise(reply) {
  if (reply && typeof reply === 'object' && typeof reply.ok === 'boolean') return reply;
  return failure('no_reply', 'rustdm did not send a reply');
}

/**
 * Send one message and wait for the host's reply.
 * Never throws: every problem comes back as `{ ok: false, error: { kind, message } }`.
 * Host replies pass through unchanged (kinds: not_running, spawn_failed,
 * rejected, duplicate, invalid, too_large). This adds host_missing,
 * host_forbidden, host_error, no_reply and timeout.
 */
export async function request(message, { send, timeoutMs = DEFAULT_TIMEOUT_MS }) {
  let timer;
  const timedOut = new Promise((resolve) => {
    timer = setTimeout(() => resolve(failure('timeout', 'rustdm did not answer in time')), timeoutMs);
  });
  const answered = (async () => {
    try {
      return normalise(await send(HOST_NAME, message));
    } catch (error) {
      return failure(classify(error), String(error?.message ?? error));
    }
  })();
  try {
    return await Promise.race([answered, timedOut]);
  } finally {
    clearTimeout(timer);
  }
}

/** What to tell the user, including how to fix it. */
export function describeError(error) {
  const detail = error?.message ? ` (${error.message})` : '';
  switch (error?.kind) {
    case 'host_missing':
      return 'The rustdm browser host is not registered. Run `rdm browser install`, then try again.';
    case 'host_forbidden':
      return 'The browser is not allowed to use the rustdm host. Run `rdm browser install` again.';
    case 'not_running':
    case 'spawn_failed':
      return `rustdm is not running and could not be started. Run \`rdm daemon start\` to see why${detail}.`;
    case 'timeout':
      return 'rustdm did not answer in time.';
    case 'no_reply':
    case 'host_error':
      return `The rustdm browser host stopped unexpectedly${detail}. Run \`rdm browser install\` and try again.`;
    case 'too_large':
    case 'invalid':
      return `rustdm could not use what the browser sent${detail}.`;
    case 'rejected':
      return `rustdm refused the download${detail}.`;
    default:
      return error?.message || 'Something went wrong.';
  }
}

/**
 * Reject as soon as `signal` aborts, without disturbing `p` itself. Used to
 * bound one caller's wait on shared work (a dial, bootstrap): the work keeps
 * running, so a result that lands late still serves the other waiters.
 */
export function abortRace<T>(
  p: Promise<T>,
  signal: AbortSignal | undefined,
  makeError: () => Error,
): Promise<T> {
  if (!signal) return p;
  return new Promise<T>((resolve, reject) => {
    const onAbort = () => reject(makeError());
    if (signal.aborted) return onAbort();
    signal.addEventListener('abort', onAbort, { once: true });
    p.then(resolve, reject).finally(() => signal.removeEventListener('abort', onAbort));
  });
}

/**
 * The error a fetch rejects with when its signal aborts, shaped like the one
 * the WASM side raises (`JsTorError::aborted`).
 */
export function abortError(): Error {
  return Object.assign(new Error('The operation was aborted'), {
    code: 'ABORT',
    kind: 'abort',
    retryable: false,
  });
}

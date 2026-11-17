// Small timing helpers shared by the API clients.

export interface BackoffOptions {
  attempts: number;
  initialDelayMs: number;
  maxDelayMs: number;
}

const DEFAULT_BACKOFF: BackoffOptions = { attempts: 5, initialDelayMs: 200, maxDelayMs: 10_000 };

/** Resolves after `ms` milliseconds. */
export function sleep(ms: number): Promise<void> {
  const { promise, resolve } = Promise.withResolvers<void>();
  setTimeout(resolve, ms);
  return promise;
}

/**
 * Calls `operation` until it succeeds. After each failure it waits twice as long as before,
 * plus random jitter, up to `maxDelayMs`. The last error is rethrown.
 */
export async function withExponentialBackoff<T>(
  operation: () => Promise<T>,
  options: BackoffOptions = DEFAULT_BACKOFF,
): Promise<T> {
  let delay = options.initialDelayMs;
  let lastError: unknown;
  for (let attempt = 1; attempt <= options.attempts; attempt++) {
    try {
      return await operation();
    } catch (err) {
      lastError = err;
      if (attempt === options.attempts) {
        break;
      }
      const jitter = Math.random() * delay * 0.2;
      await sleep(Math.min(delay + jitter, options.maxDelayMs));
      delay *= 2;
    }
  }
  throw lastError;
}

/** Delays calls to `fn` until `waitMs` passed without another call; used by search inputs. */
export function debounce<A extends unknown[]>(fn: (...args: A) => void, waitMs: number) {
  let timer: number | undefined;
  return (...args: A) => {
    clearTimeout(timer);
    timer = setTimeout(() => fn(...args), waitMs);
  };
}

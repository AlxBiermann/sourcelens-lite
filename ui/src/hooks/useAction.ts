import { useCallback, useState } from 'react';
import { errorMessage } from '../api';
import { useToast } from '../components/Toast';

type Key = string | number;

type RunOptions<T> = {
  /** Identifies the running action, e.g. a row id; `busy` holds it until the action settles. */
  key?: Key;
  /** Toast shown after `fn` resolved. */
  success?: string | ((result: T) => string);
  /** Receives the error message instead of an error toast, e.g. to show it inside a form. */
  onError?: (message: string) => void;
};

/** Runs one async action at a time: tracks busy state, reports success as a toast and failure as an error toast. */
export function useAction() {
  const notify = useToast();
  const [busy, setBusy] = useState<Key | null>(null);

  const run = useCallback(
    async <T>(fn: () => Promise<T>, options: RunOptions<T> = {}): Promise<boolean> => {
      setBusy(options.key ?? 'action');
      try {
        const result = await fn();
        const message = typeof options.success === 'function' ? options.success(result) : options.success;
        if (message) notify(message);
        return true;
      } catch (e) {
        if (options.onError) options.onError(errorMessage(e));
        else notify(errorMessage(e), 'error');
        return false;
      } finally {
        setBusy(null);
      }
    },
    [notify],
  );

  return { run, busy };
}

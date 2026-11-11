import { useCallback, useEffect, useRef, useState } from 'react';
import { api } from '../api';

export type ApiState<T> = {
  data: T | null;
  error: Error | null;
  loading: boolean;
  /** Unix seconds of the last successful load. */
  updatedAt: number | null;
  reload: () => Promise<void>;
};

/**
 * Loads `path` (skipped when null). With `pollMs`, the next poll is scheduled only after the previous
 * request settled, polling pauses while the tab is hidden, and the data reloads once the tab is visible again.
 */
export function useApi<T>(path: string | null, options: { pollMs?: number } = {}): ApiState<T> {
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<Error | null>(null);
  const [loading, setLoading] = useState(path !== null);
  const [updatedAt, setUpdatedAt] = useState<number | null>(null);
  const sequence = useRef(0);
  const inFlight = useRef(0);

  const reload = useCallback(async () => {
    if (path === null) return;
    const current = ++sequence.current;
    inFlight.current++;
    try {
      const result = await api.get<T>(path);
      if (current !== sequence.current) return;
      setData(result);
      setError(null);
      setUpdatedAt(Date.now() / 1000);
    } catch (e) {
      if (current !== sequence.current) return;
      setError(e instanceof Error ? e : new Error(String(e)));
    } finally {
      inFlight.current--;
      if (current === sequence.current) setLoading(false);
    }
  }, [path]);

  useEffect(() => {
    setData(null);
    setError(null);
    setUpdatedAt(null);
    setLoading(path !== null);
    void reload();
    return () => {
      sequence.current++;
    };
  }, [path, reload]);

  const pollMs = options.pollMs;
  useEffect(() => {
    if (path === null || !pollMs) return;
    let timer: number | undefined;
    let ticking = false;
    let stopped = false;

    const tick = async () => {
      timer = undefined;
      // Hidden: stop here; the visibilitychange listener restarts the loop.
      if (document.hidden) return;
      ticking = true;
      // A load that is still running (first load, explicit reload) counts as this tick.
      if (inFlight.current === 0) await reload();
      ticking = false;
      if (!stopped) timer = window.setTimeout(() => void tick(), pollMs);
    };

    const onVisibility = () => {
      if (document.hidden || ticking) return;
      window.clearTimeout(timer);
      void tick();
    };

    timer = window.setTimeout(() => void tick(), pollMs);
    document.addEventListener('visibilitychange', onVisibility);
    return () => {
      stopped = true;
      window.clearTimeout(timer);
      document.removeEventListener('visibilitychange', onVisibility);
    };
  }, [path, pollMs, reload]);

  return { data, error, loading, updatedAt, reload };
}

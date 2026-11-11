import { useCallback, useEffect, useRef, useState } from 'react';
import { api } from '../api';

export type ApiState<T> = {
  data: T | null;
  error: Error | null;
  loading: boolean;
  reload: () => Promise<void>;
};

/** Loads `path` (skipped when null) and re-fetches every `pollMs` while mounted. */
export function useApi<T>(path: string | null, options: { pollMs?: number } = {}): ApiState<T> {
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<Error | null>(null);
  const [loading, setLoading] = useState(path !== null);
  const sequence = useRef(0);

  const reload = useCallback(async () => {
    if (path === null) return;
    const current = ++sequence.current;
    try {
      const result = await api.get<T>(path);
      if (current !== sequence.current) return;
      setData(result);
      setError(null);
    } catch (e) {
      if (current !== sequence.current) return;
      setError(e instanceof Error ? e : new Error(String(e)));
    } finally {
      if (current === sequence.current) setLoading(false);
    }
  }, [path]);

  useEffect(() => {
    setData(null);
    setError(null);
    setLoading(path !== null);
    void reload();
    return () => {
      sequence.current++;
    };
  }, [path, reload]);

  const pollMs = options.pollMs;
  useEffect(() => {
    if (path === null || !pollMs) return;
    const timer = window.setInterval(() => void reload(), pollMs);
    return () => window.clearInterval(timer);
  }, [path, pollMs, reload]);

  return { data, error, loading, reload };
}

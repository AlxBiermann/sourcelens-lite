import { createContext, createElement, useCallback, useContext, useEffect, useState, type ReactNode } from 'react';
import { api, type AppStateDto } from '../api';

type Session = { state: AppStateDto | null; error: Error | null; reload: () => Promise<void> };

const SessionContext = createContext<Session | null>(null);

export function SessionProvider({ children }: { children: ReactNode }) {
  const [state, setState] = useState<AppStateDto | null>(null);
  const [error, setError] = useState<Error | null>(null);

  const reload = useCallback(async () => {
    try {
      setState(await api.get<AppStateDto>('/api/state'));
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e : new Error(String(e)));
    }
  }, []);

  useEffect(() => {
    void reload();
  }, [reload]);

  return createElement(SessionContext.Provider, { value: { state, error, reload } }, children);
}

export function useSession(): Session {
  const session = useContext(SessionContext);
  if (!session) throw new Error('useSession must be used inside SessionProvider');
  return session;
}

/** The signed-in user; only call below the route guard that requires one. */
export function useUser() {
  const { state } = useSession();
  const user = state?.user;
  if (!user) throw new Error('useUser called without a signed-in user');
  return user;
}

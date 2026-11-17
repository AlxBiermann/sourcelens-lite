import { useEffect, useState } from 'react';
import { Navigate, Route, Routes, useLocation, useSearchParams } from 'react-router';
import { RETURN_TO_KEY } from './api';
import { EntryPage } from './components/EntryPage';
import { Loading, Notice, Shell } from './components/Shell';
import { useSession } from './hooks/useSession';
import { ApiKeys } from './pages/ApiKeys';
import { Login } from './pages/Login';
import { OAuthAuthorize } from './pages/OAuthAuthorize';
import { Playground } from './pages/Playground';
import { Repositories } from './pages/Repositories';
import { RepositoryDetail } from './pages/RepositoryDetail';
import { SettingsEmbeddings } from './pages/SettingsEmbeddings';
import { SettingsGithub } from './pages/SettingsGithub';
import { SettingsMcpClients } from './pages/SettingsMcpClients';
import { SettingsMembers } from './pages/SettingsMembers';
import { SettingsSso } from './pages/SettingsSso';
import { SettingsSync } from './pages/SettingsSync';
import { Setup } from './pages/Setup';

const RETURN_TO_MAX_AGE_MS = 10 * 60 * 1000;

/** The page an expired session interrupted (stored by `api.request`), when it is recent and a page of this app. */
function storedReturnPath(): string | null {
  try {
    const stored = JSON.parse(sessionStorage.getItem(RETURN_TO_KEY) ?? 'null') as { path?: unknown; at?: unknown } | null;
    const path = stored?.path;
    const at = stored?.at;
    if (typeof path !== 'string' || typeof at !== 'number' || Date.now() - at >= RETURN_TO_MAX_AGE_MS) return null;
    if (!path.startsWith('/') || path.startsWith('//') || path.startsWith('/login') || path.startsWith('/setup') || path === '/repositories') return null;
    return path;
  } catch {
    return null;
  }
}

function RequireUser({ setupDone, signedIn }: { setupDone: boolean; signedIn: boolean }) {
  const location = useLocation();
  // Every sign-in lands on /repositories. Storage is only read here (twice under StrictMode, same result)
  // and cleared in the effect; the location key makes the redirect happen once, on the landing entry.
  const landing = setupDone && signedIn && location.pathname === '/repositories';
  const [returnTo] = useState(() => {
    const path = landing ? storedReturnPath() : null;
    return path === null ? null : { path, key: location.key };
  });
  useEffect(() => {
    if (!landing) return;
    try {
      sessionStorage.removeItem(RETURN_TO_KEY);
    } catch {
      // Unavailable storage holds no return path either.
    }
  }, [landing]);

  if (!setupDone) {
    // GitHub sends the installer back to /repositories?installed=1 while setup still lacks an owner.
    const installed = new URLSearchParams(location.search).get('installed') === '1';
    return <Navigate to={installed ? '/setup?step=signin' : '/setup'} replace />;
  }
  if (!signedIn) return <Navigate to="/login" replace />;
  if (returnTo !== null && location.key === returnTo.key) return <Navigate to={returnTo.path} replace />;
  return <Shell />;
}

function LoginRoute({ setupDone, signedIn }: { setupDone: boolean; signedIn: boolean }) {
  const [params] = useSearchParams();
  if (signedIn) {
    const next = params.get('next');
    return <Navigate to={next?.startsWith('/oauth/authorize?') ? next : '/repositories'} replace />;
  }
  if (!setupDone && !params.get('error')) return <Navigate to="/setup" replace />;
  return <Login />;
}

export function App() {
  const { state, error, reload } = useSession();

  if (!state) {
    if (!error) {
      return (
        <EntryPage>
          <Loading />
        </EntryPage>
      );
    }
    return (
      <EntryPage>
        <Notice tone="error">{error.message}</Notice>
        <div className="sl-actions">
          <button type="button" className="sl-button sl-button-primary" onClick={() => void reload()}>
            Try again
          </button>
        </div>
      </EntryPage>
    );
  }

  // Dev mode has neither a GitHub App nor a setup: its dev accounts sign in without one.
  const setupDone = state.devMode || (state.setup.appConfigured && state.setup.ownerExists);
  const signedIn = state.user !== null;

  return (
    <Routes>
      <Route path="/setup" element={setupDone ? <Navigate to={signedIn ? '/repositories' : '/login'} replace /> : <Setup />} />
      <Route path="/login" element={<LoginRoute setupDone={setupDone} signedIn={signedIn} />} />
      <Route path="/oauth/authorize" element={<OAuthAuthorize />} />
      <Route element={<RequireUser setupDone={setupDone} signedIn={signedIn} />}>
        <Route path="/repositories" element={<Repositories />} />
        <Route path="/repositories/:id" element={<RepositoryDetail />} />
        <Route path="/playground" element={<Playground />} />
        <Route path="/keys" element={<ApiKeys />} />
        <Route path="/settings" element={<Navigate to="/settings/embeddings" replace />} />
        <Route path="/settings/embeddings" element={<SettingsEmbeddings />} />
        <Route path="/settings/github" element={<SettingsGithub />} />
        <Route path="/settings/sync" element={<SettingsSync />} />
        <Route path="/settings/mcp-clients" element={<SettingsMcpClients />} />
        <Route path="/settings/members" element={<SettingsMembers />} />
        <Route path="/settings/sso" element={<SettingsSso />} />
        <Route path="*" element={<Navigate to="/repositories" replace />} />
      </Route>
    </Routes>
  );
}

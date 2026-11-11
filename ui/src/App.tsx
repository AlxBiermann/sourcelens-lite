import { Navigate, Route, Routes, useLocation, useSearchParams } from 'react-router';
import { Shell } from './components/Shell';
import { useSession } from './hooks/useSession';
import { ApiKeys } from './pages/ApiKeys';
import { Login } from './pages/Login';
import { Playground } from './pages/Playground';
import { Repositories } from './pages/Repositories';
import { RepositoryDetail } from './pages/RepositoryDetail';
import { SettingsEmbeddings } from './pages/SettingsEmbeddings';
import { SettingsGithub } from './pages/SettingsGithub';
import { SettingsMembers } from './pages/SettingsMembers';
import { Setup } from './pages/Setup';

function RequireUser({ setupDone, signedIn }: { setupDone: boolean; signedIn: boolean }) {
  const location = useLocation();
  if (!setupDone) {
    // GitHub sends the installer back to /repositories?installed=1 while setup still lacks an owner.
    const installed = new URLSearchParams(location.search).get('installed') === '1';
    return <Navigate to={installed ? '/setup?step=signin' : '/setup'} replace />;
  }
  if (!signedIn) return <Navigate to="/login" replace />;
  return <Shell />;
}

function LoginRoute({ setupDone, signedIn }: { setupDone: boolean; signedIn: boolean }) {
  const [params] = useSearchParams();
  if (signedIn) return <Navigate to="/repositories" replace />;
  if (!setupDone && !params.get('error')) return <Navigate to="/setup" replace />;
  return <Login />;
}

export function App() {
  const { state, error, reload } = useSession();

  if (!state) {
    if (!error) return null;
    return (
      <div className="sl-entry-page">
        <div className="sl-entry-content">
          <h1 className="sl-entry-wordmark">SourceLens</h1>
          <p className="sl-notice sl-notice-error" role="alert">
            {error.message}
          </p>
          <div className="sl-actions">
            <button type="button" className="sl-button sl-button-primary" onClick={() => void reload()}>
              Try again
            </button>
          </div>
        </div>
      </div>
    );
  }

  const setupDone = state.setup.appConfigured && state.setup.ownerExists;
  const signedIn = state.user !== null;

  return (
    <Routes>
      <Route path="/setup" element={setupDone ? <Navigate to={signedIn ? '/repositories' : '/login'} replace /> : <Setup />} />
      <Route path="/login" element={<LoginRoute setupDone={setupDone} signedIn={signedIn} />} />
      <Route element={<RequireUser setupDone={setupDone} signedIn={signedIn} />}>
        <Route path="/repositories" element={<Repositories />} />
        <Route path="/repositories/:id" element={<RepositoryDetail />} />
        <Route path="/playground" element={<Playground />} />
        <Route path="/keys" element={<ApiKeys />} />
        <Route path="/settings" element={<Navigate to="/settings/embeddings" replace />} />
        <Route path="/settings/embeddings" element={<SettingsEmbeddings />} />
        <Route path="/settings/github" element={<SettingsGithub />} />
        <Route path="/settings/members" element={<SettingsMembers />} />
        <Route path="*" element={<Navigate to="/repositories" replace />} />
      </Route>
    </Routes>
  );
}

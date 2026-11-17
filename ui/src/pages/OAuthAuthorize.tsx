import { useState } from 'react';
import { Link, Navigate, useLocation } from 'react-router';
import { api, handle, type OAuthAuthorizeView } from '../api';
import { EntryPage } from '../components/EntryPage';
import { LoadState } from '../components/LoadState';
import { Notice } from '../components/Shell';
import { useAction } from '../hooks/useAction';
import { useApi } from '../hooks/useApi';
import { useSession } from '../hooks/useSession';

/**
 * Consent page of the OAuth authorization flow for MCP clients; the client opens it in the browser.
 * The page only sends the browser back to the client when the user presses a button.
 */
export function OAuthAuthorize() {
  const { pathname, search } = useLocation();
  const { state } = useSession();
  const user = state?.user ?? null;
  const view = useApi<OAuthAuthorizeView>(user ? `/api/oauth/authorize${search}` : null);
  const action = useAction();
  const [answerError, setAnswerError] = useState<string | null>(null);
  const [leaving, setLeaving] = useState(false);

  if (!user) return <Navigate to={`/login?next=${encodeURIComponent(pathname + search)}`} replace />;

  function leave(redirectTo: string) {
    setLeaving(true);
    window.location.assign(redirectTo);
  }

  function answer(approve: boolean) {
    setAnswerError(null);
    void action.run(
      async () => {
        const result = await api.post<{ redirectTo: string }>('/api/oauth/authorize', { ...Object.fromEntries(new URLSearchParams(search)), approve });
        leave(result.redirectTo);
      },
      { onError: setAnswerError },
    );
  }

  const data = view.data;
  const loadFailed = view.error !== null && data === null;
  const busy = action.busy !== null || leaving;
  const target = data ? (data.redirectLocal ? 'the app' : data.redirectTarget) : '';
  const returning = <p className="sl-muted sl-break">Returning to {target}…</p>;

  if (data?.error) {
    const redirectTo = data.redirectTo;
    return (
      <EntryPage>
        <Notice tone="error">{data.error}</Notice>
        {leaving ? (
          returning
        ) : redirectTo ? (
          <div className="sl-actions">
            <button type="button" className="sl-button sl-button-secondary" onClick={() => leave(redirectTo)}>
              Return to {target}
            </button>
          </div>
        ) : null}
      </EntryPage>
    );
  }

  return (
    <EntryPage lead={data ? `Allow “${data.clientName}” to search your code?` : undefined}>
      <LoadState state={view} />
      {loadFailed ? (
        <p>
          <Link className="sl-link" to="/repositories">
            Go to repositories
          </Link>
        </p>
      ) : null}
      {data ? (
        <>
          <p className="sl-prose">
            This client registered itself with that name. If you allow it, it can read and search every repository connected to SourceLens, as {handle(user)}. You can
            revoke access at any time in Settings → MCP clients.
          </p>
          <dl className="sl-settings-list">
            <div className="sl-settings-item">
              <dt className="sl-settings-label">Sends you back to</dt>
              <dd className="sl-settings-value sl-break">{data.redirectLocal ? `an app on this computer (${data.redirectTarget})` : data.redirectTarget}</dd>
            </div>
          </dl>
          {answerError ? <Notice tone="error">{answerError}</Notice> : null}
          {leaving ? (
            returning
          ) : (
            <div className="sl-actions">
              <button type="button" className="sl-button sl-button-primary" disabled={busy} onClick={() => answer(true)}>
                Allow
              </button>
              <button type="button" className="sl-button sl-button-secondary" disabled={busy} onClick={() => answer(false)}>
                Deny
              </button>
            </div>
          )}
        </>
      ) : null}
    </EntryPage>
  );
}

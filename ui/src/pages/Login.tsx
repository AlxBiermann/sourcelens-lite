import { Link, useSearchParams } from 'react-router';
import { EntryPage } from '../components/EntryPage';
import { Notice } from '../components/Shell';
import { useSession } from '../hooks/useSession';

type LoginMessage = { text: string; tone?: 'error' };

const MESSAGES: Record<string, LoginMessage> = {
  not_invited: { text: "Your GitHub account is not on this instance's member list. Ask an admin to add you.", tone: 'error' },
  github: { text: 'GitHub sign-in did not complete. Try again.', tone: 'error' },
  state: { text: 'The sign-in response did not match this browser session. Sign in again.', tone: 'error' },
  setup_required: { text: 'This instance has no admin yet. Finish the setup first.', tone: 'error' },
  pending_approval: { text: 'Your access request was sent. An admin needs to approve it; sign in again after they have.' },
  sso: { text: 'Single sign-on did not complete. Try again.', tone: 'error' },
};

export function Login() {
  const { state } = useSession();
  const [params] = useSearchParams();
  const error = params.get('error');
  const message = error ? (MESSAGES[error] ?? null) : null;
  const setupRequired = error === 'setup_required';
  const sso = state?.sso ?? null;
  const devMode = state?.devMode ?? false;
  // In dev mode without a GitHub App, GitHub sign-in cannot work.
  const github = !devMode || (state?.setup.appConfigured ?? false);
  const next = params.get('next');
  const nextQuery = next ? `?next=${encodeURIComponent(next)}` : '';
  const devQuery = (account: 'admin' | 'member') => `?as=${account}${next ? `&next=${encodeURIComponent(next)}` : ''}`;

  return (
    <EntryPage lead="SourceLens indexes your GitHub repositories so coding agents can search them over MCP.">
      {message ? <Notice tone={message.tone}>{message.text}</Notice> : null}
      {devMode && !setupRequired ? (
        <>
          <Notice>Dev mode: sign in without GitHub. This instance shows demo data.</Notice>
          <div className="sl-actions">
            <a className="sl-button sl-button-primary" href={`/auth/dev/login${devQuery('admin')}`}>
              Sign in as admin
            </a>
            <a className="sl-button sl-button-secondary" href={`/auth/dev/login${devQuery('member')}`}>
              Sign in as SSO member
            </a>
          </div>
        </>
      ) : null}
      {setupRequired ? (
        <div className="sl-actions">
          <Link className="sl-button sl-button-primary" to="/setup">
            Finish the setup
          </Link>
        </div>
      ) : github || sso ? (
        <div className="sl-actions">
          {github ? (
            <a className="sl-button sl-button-primary" href={`/auth/github/login${nextQuery}`}>
              Sign in with GitHub
            </a>
          ) : null}
          {sso ? (
            <a className="sl-button sl-button-secondary" href={`/auth/sso/login${nextQuery}`}>
              Sign in with {sso.name}
            </a>
          ) : null}
        </div>
      ) : null}
    </EntryPage>
  );
}

import { Link, useSearchParams } from 'react-router';
import { ThemeToggle } from '../components/ThemeToggle';

export function Login() {
  const [params] = useSearchParams();
  const error = params.get('error');

  return (
    <div className="sl-entry-page">
      <div className="sl-entry-top">
        <ThemeToggle />
      </div>
      <div className="sl-entry-content">
        <h1 className="sl-entry-wordmark">SourceLens</h1>
        <p className="sl-entry-lead">SourceLens indexes your GitHub repositories so coding agents can search them over MCP.</p>
        {error === 'not_invited' ? (
          <p className="sl-notice sl-notice-error" role="alert">
            Your GitHub account is not on this instance's member list. Ask the owner to add you.
          </p>
        ) : null}
        {error === 'github' ? (
          <p className="sl-notice sl-notice-error" role="alert">
            GitHub sign-in did not complete. Try again.
          </p>
        ) : null}
        {error === 'state' ? (
          <p className="sl-notice sl-notice-error" role="alert">
            The GitHub response did not match this browser session. Sign in again.
          </p>
        ) : null}
        {error === 'setup_required' ? (
          <p className="sl-notice sl-notice-error" role="alert">
            This instance has no owner yet.{' '}
            <Link className="sl-link" to="/setup">
              Finish the setup
            </Link>
          </p>
        ) : null}
        <div className="sl-actions">
          <a className="sl-button sl-button-primary" href="/auth/github/login">
            Sign in with GitHub
          </a>
        </div>
      </div>
    </div>
  );
}

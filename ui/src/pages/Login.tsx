import { Link, useSearchParams } from 'react-router';
import { ThemeToggle } from '../components/ThemeToggle';

export function Login() {
  const [params] = useSearchParams();
  const error = params.get('error');

  return (
    <div className="sl-marker-page">
      <div className="sl-marker-top">
        <ThemeToggle />
      </div>
      <div className="sl-marker-content">
        <h1 className="sl-marker-wordmark">SourceLens</h1>
        <p className="sl-marker-lead">SourceLens indexes your GitHub repositories so coding agents can search them over MCP.</p>
        {error === 'not_invited' ? (
          <p className="sl-marker-error" role="alert">
            Your GitHub account is not on this instance's member list. Ask the owner to add you.
          </p>
        ) : null}
        {error === 'github' ? (
          <p className="sl-marker-error" role="alert">
            GitHub sign-in did not complete. Try again.
          </p>
        ) : null}
        {error === 'setup_required' ? (
          <p className="sl-marker-error" role="alert">
            This instance has no owner yet.{' '}
            <Link className="sl-marker-error-link" to="/setup">
              Finish the setup
            </Link>
          </p>
        ) : null}
        <div className="sl-actions">
          <a className="sl-button sl-button-ink" href="/auth/github/login">
            Sign in with GitHub
          </a>
        </div>
      </div>
    </div>
  );
}

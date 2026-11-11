import { Link, useSearchParams } from 'react-router';
import { EntryPage } from '../components/EntryPage';
import { Notice } from '../components/Shell';

const ERRORS: Record<string, string> = {
  not_invited: "Your GitHub account is not on this instance's member list. Ask the owner to add you.",
  github: 'GitHub sign-in did not complete. Try again.',
  state: 'The GitHub response did not match this browser session. Sign in again.',
  setup_required: 'This instance has no owner yet. Finish the setup first.',
};

export function Login() {
  const [params] = useSearchParams();
  const error = params.get('error');
  const setupRequired = error === 'setup_required';

  return (
    <EntryPage lead="SourceLens indexes your GitHub repositories so coding agents can search them over MCP.">
      {error && ERRORS[error] ? <Notice tone="error">{ERRORS[error]}</Notice> : null}
      <div className="sl-actions">
        {setupRequired ? (
          <Link className="sl-button sl-button-primary" to="/setup">
            Finish the setup
          </Link>
        ) : (
          <a className="sl-button sl-button-primary" href="/auth/github/login">
            Sign in with GitHub
          </a>
        )}
      </div>
    </EntryPage>
  );
}

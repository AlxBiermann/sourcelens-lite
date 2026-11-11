import { useState, type FormEvent, type ReactNode } from 'react';
import { useSearchParams } from 'react-router';
import { api, errorMessage } from '../api';
import { Field } from '../components/Field';
import { ThemeToggle } from '../components/ThemeToggle';
import { useSession } from '../hooks/useSession';

const TOKEN_KEY = 'sourcelens.setupToken';

const ERRORS: Record<string, string> = {
  state: 'The GitHub response did not match this browser session. Start again.',
  conversion: 'GitHub did not return the app credentials. Start again.',
};

function storedToken(): string {
  try {
    return window.sessionStorage.getItem(TOKEN_KEY) ?? '';
  } catch {
    return '';
  }
}

function rememberToken(token: string) {
  try {
    window.sessionStorage.setItem(TOKEN_KEY, token);
  } catch {
    // Not remembered; the field keeps the value for this page view.
  }
}

function postManifest(postUrl: string, manifest: unknown) {
  const form = document.createElement('form');
  form.method = 'post';
  form.action = postUrl;
  form.hidden = true;
  const field = document.createElement('input');
  field.type = 'hidden';
  field.name = 'manifest';
  field.value = JSON.stringify(manifest);
  form.append(field);
  document.body.append(form);
  form.submit();
}

function Step({ number, title, state, children }: { number: number; title: string; state: 'done' | 'current' | 'later'; children: ReactNode }) {
  return (
    <li className={state === 'current' ? 'sl-step sl-step-current' : 'sl-step'}>
      <span className={state === 'current' ? 'sl-step-number sl-step-number-current' : 'sl-step-number'} aria-hidden="true">
        {number}
      </span>
      <div className="sl-step-body">
        <h2 className="sl-step-title">
          {title}
          {state === 'done' ? <span className="sl-step-done">Done</span> : null}
        </h2>
        {children}
      </div>
    </li>
  );
}

export function Setup() {
  const { state } = useSession();
  const [params] = useSearchParams();
  const [token, setToken] = useState(storedToken);
  const [organization, setOrganization] = useState('');
  const [busy, setBusy] = useState(false);
  const [formError, setFormError] = useState<string | null>(null);

  if (!state) return null;
  const { appConfigured, ownerExists, installUrl, appSlug } = state.setup;
  const error = params.get('error');
  const current = !appConfigured ? 1 : params.get('step') === 'signin' ? 3 : 2;
  const stepState = (n: number) => (n === 1 && appConfigured) || (n === 3 && ownerExists) ? 'done' : n === current ? 'current' : 'later';

  async function createApp(event: FormEvent) {
    event.preventDefault();
    setBusy(true);
    setFormError(null);
    rememberToken(token.trim());
    try {
      const { postUrl, manifest } = await api.post<{ postUrl: string; manifest: unknown }>('/api/setup/manifest', {
        setupToken: token.trim(),
        organization: organization.trim() || undefined,
      });
      postManifest(postUrl, manifest);
    } catch (e) {
      setFormError(errorMessage(e));
      setBusy(false);
    }
  }

  const tokenField = (
    <Field label="Setup token" hint="Printed in the server log when SourceLens starts.">
      {(props) => <input {...props} className="sl-input sl-input-mono" value={token} onChange={(e) => setToken(e.target.value)} autoComplete="off" spellCheck={false} required />}
    </Field>
  );

  const signInHref = `/auth/github/login?setup_token=${encodeURIComponent(token.trim())}`;

  return (
    <div className="sl-entry-page">
      <div className="sl-entry-top">
        <ThemeToggle />
      </div>
      <div className="sl-entry-content">
        <h1 className="sl-entry-wordmark">SourceLens</h1>
        <p className="sl-entry-lead">Three steps connect this instance to GitHub and make you its owner.</p>
        {error && ERRORS[error] ? (
          <p className="sl-notice sl-notice-error" role="alert">
            {ERRORS[error]}
          </p>
        ) : null}
        {formError ? (
          <p className="sl-notice sl-notice-error" role="alert">
            {formError}
          </p>
        ) : null}
        <ol className="sl-steps">
          <Step number={1} title="Create the GitHub App" state={stepState(1)}>
            {appConfigured ? (
              <p className="sl-prose">
                The GitHub App {appSlug ? <strong>{appSlug}</strong> : null} is configured.
              </p>
            ) : (
              <form className="sl-form" onSubmit={(e) => void createApp(e)}>
                {tokenField}
                <Field label="Organization" hint="Leave empty to create the app on your personal account.">
                  {(props) => <input {...props} className="sl-input" value={organization} onChange={(e) => setOrganization(e.target.value)} autoComplete="off" spellCheck={false} />}
                </Field>
                <div className="sl-actions">
                  <button type="submit" className="sl-button sl-button-primary" disabled={busy || !token.trim()}>
                    Create GitHub App
                  </button>
                </div>
              </form>
            )}
          </Step>
          <Step number={2} title="Install it" state={stepState(2)}>
            <p className="sl-prose">Choose the repositories SourceLens may read. You can change this later on GitHub.</p>
            {appConfigured && installUrl ? (
              <div className="sl-actions">
                <a className={current === 2 ? 'sl-button sl-button-primary' : 'sl-button sl-button-secondary'} href={installUrl} target="_blank" rel="noreferrer">
                  Install on GitHub
                </a>
              </div>
            ) : null}
          </Step>
          <Step number={3} title="Sign in" state={stepState(3)}>
            <p className="sl-prose">The first person to sign in with the setup token becomes the owner of this instance.</p>
            {appConfigured ? (
              <div className="sl-form">
                {tokenField}
                <div className="sl-actions">
                  {token.trim() ? (
                    <a className={current === 3 ? 'sl-button sl-button-primary' : 'sl-button sl-button-secondary'} href={signInHref} onClick={() => rememberToken(token.trim())}>
                      Sign in with GitHub
                    </a>
                  ) : (
                    <button type="button" className="sl-button sl-button-secondary" disabled>
                      Sign in with GitHub
                    </button>
                  )}
                </div>
              </div>
            ) : null}
          </Step>
        </ol>
      </div>
    </div>
  );
}

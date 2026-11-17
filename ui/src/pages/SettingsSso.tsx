import { useEffect, useId, useRef, useState, type FormEvent } from 'react';
import { Navigate } from 'react-router';
import { api, ApiError, type SsoSettings, type SsoSettingsInput } from '../api';
import { CopyButton } from '../components/CopyButton';
import { Field } from '../components/Field';
import { LoadState } from '../components/LoadState';
import { PageHeader, SettingsTabs } from '../components/Shell';
import { useAction } from '../hooks/useAction';
import { useApi } from '../hooks/useApi';
import { useUser } from '../hooks/useSession';

type Form = { enabled: boolean; name: string; issuer: string; clientId: string; clientSecret: string };

type FieldErrors = { issuer?: string; clientId?: string; clientSecret?: string };

const SECRET_AGAIN = 'Enter the client secret again when the issuer or client ID changes.';

function toForm(s: SsoSettings): Form {
  return { enabled: s.enabled, name: s.name, issuer: s.issuer, clientId: s.clientId, clientSecret: '' };
}

function toInput(form: Form): SsoSettingsInput {
  return {
    enabled: form.enabled,
    name: form.name.trim(),
    issuer: form.issuer.trim(),
    clientId: form.clientId.trim(),
    clientSecret: form.clientSecret ? form.clientSecret : undefined,
  };
}

export function SettingsSso() {
  const user = useUser();
  const owner = user.role === 'owner';
  const settings = useApi<SsoSettings>(owner ? '/api/settings/sso' : null);
  const action = useAction();
  const [form, setForm] = useState<Form | null>(null);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [errors, setErrors] = useState<FieldErrors>({});
  const redirectRef = useRef<HTMLElement>(null);
  const switchHintId = useId();

  useEffect(() => {
    if (settings.data && form === null) setForm(toForm(settings.data));
  }, [settings.data, form]);

  if (!owner) return <Navigate to="/settings/embeddings" replace />;

  function update<K extends keyof Form>(key: K, value: Form[K]) {
    setForm((current) => (current ? { ...current, [key]: value } : current));
  }

  const data = settings.data;
  const busy = action.busy !== null;
  // The server reuses a saved secret only for the issuer and client ID it was saved with.
  const identityChanged = form !== null && data !== null && (form.issuer.trim().replace(/\/+$/, '') !== data.issuer || form.clientId.trim() !== data.clientId);
  const secretKept = data !== null && data.clientSecretSet && !identityChanged;

  function save(event: FormEvent) {
    event.preventDefault();
    if (!form || !data) return;
    const input = toInput(form);
    const required: FieldErrors = {};
    if (input.enabled) {
      if (!input.issuer) required.issuer = 'Enter the issuer URL.';
      if (!input.clientId) required.clientId = 'Enter the client ID.';
      if (!input.clientSecret && !secretKept) required.clientSecret = 'Enter the client secret.';
    }
    setSaveError(null);
    setErrors(required);
    if (Object.keys(required).length > 0) return;
    let discoveryFailed = false;
    void action.run(
      async () => {
        let saved: SsoSettings;
        try {
          saved = await api.put<SsoSettings>('/api/settings/sso', input);
        } catch (e) {
          discoveryFailed = e instanceof ApiError && e.code === 'sso_discovery_failed';
          throw e;
        }
        setForm(toForm(saved));
        await settings.reload();
      },
      { key: 'save', success: 'Saved.', onError: (message) => (discoveryFailed ? setErrors({ issuer: message }) : setSaveError(message)) },
    );
  }

  return (
    <div className="sl-page sl-page-settings">
      <PageHeader title="Settings" />
      <SettingsTabs owner />
      <LoadState state={settings} />

      {form && data ? (
        <>
          <dl className="sl-settings-list">
            <div className="sl-settings-item">
              <dt className="sl-settings-label">Redirect URL</dt>
              <dd className="sl-settings-value sl-settings-copy">
                <code className="sl-mono sl-break" ref={redirectRef}>
                  {data.redirectUrl}
                </code>
                <CopyButton text={data.redirectUrl} target={redirectRef} what="URL" />
              </dd>
            </div>
          </dl>
          <p className="sl-prose sl-muted">Register this redirect URL with your provider. People who sign in with SSO appear under Members and wait for approval.</p>

          <form className="sl-form" onSubmit={save}>
            <fieldset className="sl-fieldset">
              <legend className="sl-visually-hidden">Sign-in settings</legend>
              <div className="sl-field">
                <label className="sl-switch">
                  <input type="checkbox" role="switch" className="sl-switch-input" checked={form.enabled} onChange={(e) => update('enabled', e.target.checked)} aria-describedby={switchHintId} />
                  <span>Allow sign-in with SSO</span>
                </label>
                <p className="sl-hint" id={switchHintId}>
                  Turning SSO off, or changing the issuer, signs out everyone who signed in with SSO, disconnects their MCP clients and deletes the API keys they created.
                </p>
              </div>
              <Field label="Button label" hint="Shown on the sign-in page as “Sign in with …”.">
                {(props) => <input {...props} className="sl-input" value={form.name} onChange={(e) => update('name', e.target.value)} autoComplete="off" maxLength={24} />}
              </Field>
              <Field label="Issuer URL" hint="The provider's OpenID Connect issuer, for example https://login.example.com/realms/main." error={errors.issuer}>
                {(props) => <input {...props} className="sl-input sl-input-mono" type="url" value={form.issuer} onChange={(e) => update('issuer', e.target.value)} autoComplete="off" spellCheck={false} />}
              </Field>
              <Field label="Client ID" error={errors.clientId}>
                {(props) => <input {...props} className="sl-input sl-input-mono" value={form.clientId} onChange={(e) => update('clientId', e.target.value)} autoComplete="off" spellCheck={false} />}
              </Field>
              <Field label="Client secret" hint={secretKept ? 'Leave empty to keep the saved secret.' : data.clientSecretSet ? SECRET_AGAIN : undefined} error={errors.clientSecret}>
                {(props) => (
                  <input {...props} className="sl-input sl-input-mono" type="password" value={form.clientSecret} placeholder={secretKept ? 'Saved' : undefined} onChange={(e) => update('clientSecret', e.target.value)} autoComplete="new-password" />
                )}
              </Field>
            </fieldset>

            {saveError ? (
              <p className="sl-field-error" role="alert">
                {saveError}
              </p>
            ) : null}

            <div className="sl-actions">
              <button type="submit" className="sl-button sl-button-primary" disabled={busy}>
                Save sign-in settings
              </button>
            </div>
          </form>
        </>
      ) : null}
    </div>
  );
}

import { useEffect, useState, type FormEvent } from 'react';
import { api, ApiError, type SyncSettings } from '../api';
import { Field } from '../components/Field';
import { LoadState } from '../components/LoadState';
import { Notice, PageHeader, SettingsTabs } from '../components/Shell';
import { useAction } from '../hooks/useAction';
import { useApi } from '../hooks/useApi';
import { useUser } from '../hooks/useSession';
import { date } from '../format';

type Form = { enabled: boolean; cron: string };

const PRESETS = [
  { label: 'Every 15 minutes', cron: '*/15 * * * *' },
  { label: 'Every hour', cron: '0 * * * *' },
  { label: 'Every 6 hours', cron: '0 */6 * * *' },
  { label: 'Every day at 02:00 UTC', cron: '0 2 * * *' },
];

const timeFormat = new Intl.DateTimeFormat('en-GB', { hour: '2-digit', minute: '2-digit', timeZoneName: 'short' });

function dateTime(ts: number): string {
  return `${date(ts)}, ${timeFormat.format(new Date(ts * 1000))}`;
}

/** The expression as the server stores it: fields separated by one space. */
function normalize(cron: string): string {
  return cron.trim().split(/\s+/).join(' ');
}

/** "On, at minute 0 past every hour (UTC)." for the saved schedule; the next run below is in local time. */
function status(settings: SyncSettings): string {
  if (!settings.enabled) return 'Off';
  const text = (settings.description ?? settings.cron).replace(/\.$/, '');
  return `On, ${text.charAt(0).toLowerCase()}${text.slice(1)} (UTC).`;
}

export function SettingsSync() {
  const user = useUser();
  const owner = user.role === 'owner';
  const settings = useApi<SyncSettings>('/api/settings/sync');
  const action = useAction();
  const [form, setForm] = useState<Form | null>(null);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [cronError, setCronError] = useState<string | null>(null);

  useEffect(() => {
    if (settings.data && form === null) setForm({ enabled: settings.data.enabled, cron: settings.data.cron });
  }, [settings.data, form]);

  function update<K extends keyof Form>(key: K, value: Form[K]) {
    setForm((current) => (current ? { ...current, [key]: value } : current));
  }

  function save(event: FormEvent) {
    event.preventDefault();
    if (!form) return;
    setSaveError(null);
    setCronError(null);
    let invalidCron = false;
    void action.run(
      async () => {
        let saved: SyncSettings;
        try {
          saved = await api.put<SyncSettings>('/api/settings/sync', form);
        } catch (e) {
          invalidCron = e instanceof ApiError && e.code === 'invalid_cron';
          throw e;
        }
        setForm({ enabled: saved.enabled, cron: saved.cron });
        await settings.reload();
      },
      { key: 'save', success: 'Saved.', onError: (message) => (invalidCron ? setCronError(message) : setSaveError(message)) },
    );
  }

  const data = settings.data;
  const busy = action.busy !== null;
  const preset = form ? (PRESETS.find((p) => p.cron === normalize(form.cron))?.cron ?? '') : '';
  const changed = form !== null && data !== null && (form.enabled !== data.enabled || normalize(form.cron) !== data.cron);

  return (
    <div className="sl-page sl-page-settings">
      <PageHeader title="Settings" />
      <SettingsTabs owner={owner} />
      <p className="sl-prose sl-muted">Queue a sync of every connected repository on a schedule. Repositories without new commits are left as they are.</p>
      {!owner ? <Notice>Only admins can change the sync schedule.</Notice> : null}
      <LoadState state={settings} />

      {form && data ? (
        <>
          <dl className="sl-settings-list" aria-label="Saved sync schedule">
            <div className="sl-settings-item">
              <dt className="sl-settings-label">Saved schedule</dt>
              <dd className="sl-settings-value">{status(data)}</dd>
            </div>
            {changed ? null : (
              <div className="sl-settings-item">
                <dt className="sl-settings-label">Next run</dt>
                <dd className="sl-settings-value">{data.nextRunAt !== null ? dateTime(data.nextRunAt) : '—'}</dd>
              </div>
            )}
          </dl>

          <form className="sl-form" onSubmit={save}>
            <fieldset className="sl-fieldset" disabled={!owner}>
              <legend className="sl-visually-hidden">Sync schedule</legend>
              <label className="sl-switch">
                <input type="checkbox" role="switch" className="sl-switch-input" checked={form.enabled} onChange={(e) => update('enabled', e.target.checked)} />
                <span>Sync all repositories automatically</span>
              </label>
              {owner ? (
                <div className="sl-toolbar">
                  <Field label="Preset">
                    {(props) => (
                      <select
                        {...props}
                        className="sl-select"
                        value={preset}
                        onChange={(e) => {
                          if (e.target.value) update('cron', e.target.value);
                        }}
                      >
                        <option value="">Custom</option>
                        {PRESETS.map((p) => (
                          <option key={p.cron} value={p.cron}>
                            {p.label}
                          </option>
                        ))}
                      </select>
                    )}
                  </Field>
                </div>
              ) : null}
              <Field label="Schedule (cron, UTC)" hint="minute hour day-of-month month day-of-week" error={cronError}>
                {(props) => <input {...props} className="sl-input sl-input-mono" value={form.cron} onChange={(e) => update('cron', e.target.value)} autoComplete="off" spellCheck={false} />}
              </Field>
            </fieldset>

            {saveError ? (
              <p className="sl-field-error" role="alert">
                {saveError}
              </p>
            ) : null}

            {owner ? (
              <div className="sl-actions">
                <button type="submit" className="sl-button sl-button-primary" disabled={busy}>
                  Save sync schedule
                </button>
              </div>
            ) : null}
          </form>
        </>
      ) : null}
    </div>
  );
}

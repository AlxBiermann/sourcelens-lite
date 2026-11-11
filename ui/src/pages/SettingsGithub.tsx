import { ApiError, type GithubSettings } from '../api';
import { CopyButton } from '../components/CopyButton';
import { Loading, Notice, PageHeader, SettingsTabs } from '../components/Shell';
import { useApi } from '../hooks/useApi';
import { useUser } from '../hooks/useSession';
import { ago } from '../format';

export function SettingsGithub() {
  const user = useUser();
  const github = useApi<GithubSettings>('/api/settings/github');
  const data = github.data;
  const notConfigured = github.error instanceof ApiError && github.error.code === 'app_not_configured';

  return (
    <div className="sl-page">
      <PageHeader title="Settings" />
      <SettingsTabs owner={user.role === 'owner'} />
      {github.loading ? <Loading /> : null}
      {notConfigured ? <Notice tone="error">No GitHub App is configured. Restart SourceLens and open the setup page.</Notice> : null}
      {github.error && !notConfigured ? <Notice tone="error">{github.error.message}</Notice> : null}
      {data ? (
        <>
          <dl className="sl-settings-list">
            <div className="sl-settings-item">
              <dt className="sl-settings-label">GitHub App</dt>
              <dd className="sl-settings-value">
                <a className="sl-link" href={data.appUrl} target="_blank" rel="noreferrer">
                  {data.slug}
                </a>
              </dd>
            </div>
            <div className="sl-settings-item">
              <dt className="sl-settings-label">Source</dt>
              <dd className="sl-settings-value">{data.source === 'manifest' ? 'Created here' : 'From environment'}</dd>
            </div>
            <div className="sl-settings-item">
              <dt className="sl-settings-label">Webhook URL</dt>
              <dd className="sl-settings-value sl-settings-copy">
                <code className="sl-mono sl-break">{data.webhookUrl}</code>
                <CopyButton text={data.webhookUrl} />
              </dd>
            </div>
            <div className="sl-settings-item">
              <dt className="sl-settings-label">Webhook</dt>
              <dd className="sl-settings-value">
                {data.webhookActive ? (
                  'Active. Pushes start an index run within seconds.'
                ) : (
                  <span className="sl-text-red">Inactive. GitHub cannot reach this address, so pushes are not picked up automatically. Use Sync now.</span>
                )}
              </dd>
            </div>
            <div className="sl-settings-item">
              <dt className="sl-settings-label">Last delivery</dt>
              <dd className="sl-settings-value">{data.lastDeliveryAt ? `${data.lastDeliveryEvent ?? 'Event'} ${ago(data.lastDeliveryAt)}` : 'No deliveries yet'}</dd>
            </div>
          </dl>
          <div className="sl-actions">
            <a className="sl-button sl-button-primary" href={data.installUrl} target="_blank" rel="noreferrer">
              Install on GitHub
            </a>
          </div>
        </>
      ) : null}
    </div>
  );
}

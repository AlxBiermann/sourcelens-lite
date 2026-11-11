import { useMemo, useState } from 'react';
import { api, errorMessage, type Installation, type Repository } from '../api';
import { Dialog } from '../components/Dialog';
import { Field } from '../components/Field';
import { Loading, Notice } from '../components/Shell';
import { useToast } from '../components/Toast';
import { useApi } from '../hooks/useApi';
import { useSession } from '../hooks/useSession';

export function AddRepositories({ open, onClose, onConnected }: { open: boolean; onClose: () => void; onConnected: (repo: Repository) => void }) {
  const { state } = useSession();
  const installations = useApi<Installation[]>(open ? '/api/github/repositories' : null);
  const [filter, setFilter] = useState('');
  const [connected, setConnected] = useState<Record<string, true>>({});
  const [pending, setPending] = useState<string | null>(null);
  const notify = useToast();

  const groups = useMemo(() => {
    const needle = filter.trim().toLowerCase();
    return (installations.data ?? []).map((installation) => ({
      installation,
      repositories: needle ? installation.repositories.filter((repo) => repo.fullName.toLowerCase().includes(needle)) : installation.repositories,
    }));
  }, [installations.data, filter]);

  async function connect(installationId: number, fullName: string) {
    setPending(fullName);
    try {
      const repo = await api.post<Repository>('/api/repositories', { fullName, installationId });
      setConnected((current) => ({ ...current, [fullName]: true }));
      notify(`Connected ${fullName}`);
      onConnected(repo);
    } catch (e) {
      notify(errorMessage(e), 'error');
    } finally {
      setPending(null);
    }
  }

  const installUrl = state?.setup.installUrl;
  const empty = installations.data !== null && installations.data.length === 0;
  const visible = groups.reduce((sum, group) => sum + group.repositories.length, 0);

  return (
    <Dialog
      open={open}
      title="Add repositories"
      onClose={onClose}
      wide
      actions={
        <button type="button" className="sl-button sl-button-secondary" onClick={onClose}>
          Close
        </button>
      }
    >
      {installations.loading ? <Loading /> : null}
      {installations.error ? <Notice tone="error">{installations.error.message}</Notice> : null}
      {empty ? (
        <div className="sl-empty">
          <p className="sl-empty-text">The GitHub App is not installed anywhere you can access.</p>
          {installUrl ? (
            <a className="sl-button sl-button-primary" href={installUrl} target="_blank" rel="noreferrer">
              Install on GitHub
            </a>
          ) : null}
        </div>
      ) : null}
      {installations.data && installations.data.length > 0 ? (
        <div className="sl-stack">
          <Field label="Filter repositories">
            {(props) => <input {...props} type="search" className="sl-input" value={filter} onChange={(e) => setFilter(e.target.value)} autoComplete="off" spellCheck={false} />}
          </Field>
          {filter && visible === 0 ? <p className="sl-muted">No repository matches "{filter.trim()}".</p> : null}
          {groups.map(({ installation, repositories }) => (
            <section key={installation.installationId} className="sl-account" aria-label={installation.account.login}>
              <div className="sl-account-head">
                {installation.account.avatarUrl ? <img className="sl-avatar" src={installation.account.avatarUrl} alt="" width={32} height={32} /> : null}
                <h3 className="sl-account-name">{installation.account.login}</h3>
                <a className="sl-link sl-account-manage" href={installation.manageUrl} target="_blank" rel="noreferrer">
                  Change which repositories GitHub shares
                </a>
              </div>
              {installation.truncated ? <p className="sl-hint">Showing the first 1,000 repositories. Filter on GitHub to narrow access.</p> : null}
              <ul className="sl-picklist">
                {repositories.map((repo) => {
                  const isConnected = repo.connected || connected[repo.fullName] === true;
                  return (
                    <li key={repo.githubId} className="sl-pick">
                      <span className="sl-pick-name">
                        {repo.fullName}
                        {repo.private ? <span className="sl-private">Private</span> : null}
                      </span>
                      <span className="sl-pick-branch">{repo.defaultBranch}</span>
                      <button
                        type="button"
                        className={isConnected ? 'sl-button sl-button-quiet sl-button-small' : 'sl-button sl-button-secondary sl-button-small'}
                        disabled={isConnected || pending !== null}
                        onClick={() => void connect(installation.installationId, repo.fullName)}
                      >
                        {isConnected ? 'Connected' : 'Connect'}
                      </button>
                    </li>
                  );
                })}
              </ul>
            </section>
          ))}
        </div>
      ) : null}
    </Dialog>
  );
}

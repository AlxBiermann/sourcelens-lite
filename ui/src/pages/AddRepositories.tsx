import { useEffect, useMemo, useRef, useState } from 'react';
import { api, type Installation, type Repository } from '../api';
import { Dialog } from '../components/Dialog';
import { Field } from '../components/Field';
import { LoadState } from '../components/LoadState';
import { useAction } from '../hooks/useAction';
import { useApi } from '../hooks/useApi';
import { useSession } from '../hooks/useSession';

export function AddRepositories({ open, onClose, onConnected }: { open: boolean; onClose: () => void; onConnected: (repo: Repository) => void }) {
  const { state } = useSession();
  const installations = useApi<Installation[]>(open ? '/api/github/repositories' : null);
  const [filter, setFilter] = useState('');
  const [connected, setConnected] = useState<Record<string, true>>({});
  const action = useAction();
  const filterInput = useRef<HTMLInputElement>(null);

  const groups = useMemo(() => {
    const needle = filter.trim().toLowerCase();
    return (installations.data ?? []).map((installation) => ({
      installation,
      repositories: needle ? installation.repositories.filter((repo) => repo.fullName.toLowerCase().includes(needle)) : installation.repositories,
    }));
  }, [installations.data, filter]);

  // The list arrives after the dialog opened, when "Close" already has focus; move it to the filter.
  const hasList = (installations.data?.length ?? 0) > 0;
  useEffect(() => {
    if (hasList) filterInput.current?.focus();
  }, [hasList]);

  function connect(installationId: number, fullName: string) {
    void action.run(
      async () => {
        const repo = await api.post<Repository>('/api/repositories', { fullName, installationId });
        setConnected((current) => ({ ...current, [fullName]: true }));
        onConnected(repo);
      },
      { key: fullName, success: `Connected ${fullName}` },
    );
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
      <LoadState state={installations} />
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
      {hasList ? (
        <div className="sl-stack">
          <Field label="Filter repositories">
            {(props) => <input {...props} ref={filterInput} type="search" className="sl-input" value={filter} onChange={(e) => setFilter(e.target.value)} autoComplete="off" spellCheck={false} />}
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
                        className={isConnected ? 'sl-button sl-button-quiet sl-button-small sl-pick-button' : 'sl-button sl-button-secondary sl-button-small sl-pick-button'}
                        disabled={isConnected || action.busy !== null}
                        onClick={() => connect(installation.installationId, repo.fullName)}
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

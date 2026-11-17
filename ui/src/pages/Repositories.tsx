import { useState } from 'react';
import { Link, useSearchParams } from 'react-router';
import { api, type GithubSettings, type Repository } from '../api';
import { LoadState } from '../components/LoadState';
import { RepoRow } from '../components/RepoRow';
import { Notice, PageHeader } from '../components/Shell';
import { useEmbeddingsOn } from '../components/StatusBlock';
import { plural } from '../format';
import { useAction } from '../hooks/useAction';
import { useApi } from '../hooks/useApi';
import { useUser } from '../hooks/useSession';
import { AddRepositories } from './AddRepositories';

export function Repositories() {
  const [params, setParams] = useSearchParams();
  const [adding, setAdding] = useState(params.get('installed') === '1');
  const [active, setActive] = useState(false);
  const repos = useApi<Repository[]>('/api/repositories', { pollMs: active ? 2000 : 15000 });
  const github = useApi<GithubSettings>('/api/settings/github');
  const embeddingsOn = useEmbeddingsOn();
  const action = useAction();
  const user = useUser();
  const awaiting = user.role === 'owner' ? user.pendingApprovals : 0;

  const anyActive = repos.data?.some((repo) => repo.activeJob !== null) ?? false;
  if (anyActive !== active) setActive(anyActive);

  function sync(repo: Repository) {
    void action.run(
      async () => {
        await api.post(`/api/repositories/${repo.id}/sync`, {});
        await repos.reload();
      },
      { key: repo.id, success: 'Sync started' },
    );
  }

  function syncAll() {
    void action.run(
      async () => {
        const { queued } = await api.post<{ queued: number }>('/api/repositories/sync');
        await repos.reload();
        return queued;
      },
      { key: 'all', success: (n) => `Sync started for ${plural(n, 'repository', 'repositories')}` },
    );
  }

  function closeDialog() {
    setAdding(false);
    if (params.has('installed')) setParams({}, { replace: true });
  }

  const addButton = (
    <button type="button" className="sl-button sl-button-primary" onClick={() => setAdding(true)}>
      Add repositories
    </button>
  );

  const anyConnected = repos.data?.some((repo) => repo.state === 'connected') ?? false;
  const headerActions = (
    <>
      {anyConnected ? (
        <button type="button" className="sl-button sl-button-secondary" disabled={action.busy !== null} onClick={syncAll}>
          Sync all
        </button>
      ) : null}
      {addButton}
    </>
  );

  return (
    <div className="sl-page">
      <PageHeader title="Repositories" action={repos.data?.length === 0 ? undefined : headerActions} />
      {github.data && !github.data.webhookActive ? (
        <Notice>GitHub cannot reach this address, so pushes are not picked up automatically. Use Sync all or turn on automatic sync in Settings → Sync.</Notice>
      ) : null}
      {awaiting > 0 ? (
        <Notice>
          {plural(awaiting, 'person is', 'people are')} waiting for approval.{' '}
          <Link className="sl-link" to="/settings/members">
            Review them
          </Link>
          .
        </Notice>
      ) : null}
      <LoadState state={repos} />
      {repos.data && repos.data.length === 0 ? (
        <div className="sl-empty">
          <p className="sl-empty-text">No repositories yet. Add one and SourceLens indexes its default branch.</p>
          {addButton}
        </div>
      ) : null}
      {repos.data && repos.data.length > 0 ? (
        <ul className="sl-ledger" aria-label="Connected repositories">
          {repos.data.map((repo) => (
            <RepoRow key={repo.id} repo={repo} embeddingsOn={embeddingsOn} onSync={sync} syncing={action.busy === repo.id} />
          ))}
        </ul>
      ) : null}
      <AddRepositories open={adding} onClose={closeDialog} onConnected={() => void repos.reload()} />
    </div>
  );
}

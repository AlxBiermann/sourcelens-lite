import { useState } from 'react';
import { useSearchParams } from 'react-router';
import { api, type GithubSettings, type Repository } from '../api';
import { LoadState } from '../components/LoadState';
import { RepoRow } from '../components/RepoRow';
import { Notice, PageHeader } from '../components/Shell';
import { useEmbeddingsOn } from '../components/StatusBlock';
import { useAction } from '../hooks/useAction';
import { useApi } from '../hooks/useApi';
import { AddRepositories } from './AddRepositories';

export function Repositories() {
  const [params, setParams] = useSearchParams();
  const [adding, setAdding] = useState(params.get('installed') === '1');
  const [active, setActive] = useState(false);
  const repos = useApi<Repository[]>('/api/repositories', { pollMs: active ? 2000 : 15000 });
  const github = useApi<GithubSettings>('/api/settings/github');
  const embeddingsOn = useEmbeddingsOn();
  const action = useAction();

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

  function closeDialog() {
    setAdding(false);
    if (params.has('installed')) setParams({}, { replace: true });
  }

  const addButton = (
    <button type="button" className="sl-button sl-button-primary" onClick={() => setAdding(true)}>
      Add repositories
    </button>
  );

  return (
    <div className="sl-page">
      <PageHeader title="Repositories" action={repos.data?.length === 0 ? undefined : addButton} />
      {github.data && !github.data.webhookActive ? <Notice>GitHub cannot reach this address, so pushes are not picked up automatically. Use Sync now.</Notice> : null}
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

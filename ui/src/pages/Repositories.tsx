import { useState } from 'react';
import { useSearchParams } from 'react-router';
import { api, errorMessage, type EmbeddingSettings, type GithubSettings, type Repository } from '../api';
import { RepoRow } from '../components/RepoRow';
import { Loading, Notice, PageHeader } from '../components/Shell';
import { useToast } from '../components/Toast';
import { useApi } from '../hooks/useApi';
import { AddRepositories } from './AddRepositories';

export function Repositories() {
  const [params, setParams] = useSearchParams();
  const [adding, setAdding] = useState(params.get('installed') === '1');
  const [syncing, setSyncing] = useState<number | null>(null);
  const [active, setActive] = useState(false);
  const notify = useToast();
  const repos = useApi<Repository[]>('/api/repositories', { pollMs: active ? 2000 : 15000 });
  const github = useApi<GithubSettings>('/api/settings/github');
  const embeddings = useApi<EmbeddingSettings>('/api/settings/embeddings');

  const anyActive = repos.data?.some((repo) => repo.activeJob !== null) ?? false;
  if (anyActive !== active) setActive(anyActive);

  async function sync(repo: Repository) {
    setSyncing(repo.id);
    try {
      await api.post(`/api/repositories/${repo.id}/sync`, {});
      notify('Sync started');
      await repos.reload();
    } catch (e) {
      notify(errorMessage(e), 'error');
    } finally {
      setSyncing(null);
    }
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
      <PageHeader title="Repositories" action={addButton} />
      {github.data && !github.data.webhookActive ? <Notice>GitHub cannot reach this address, so pushes are not picked up automatically. Use Sync now.</Notice> : null}
      {repos.error && !repos.data ? <Notice tone="error">{repos.error.message}</Notice> : null}
      {repos.loading && !repos.data ? <Loading /> : null}
      {repos.data && repos.data.length === 0 ? (
        <div className="sl-empty">
          <p className="sl-empty-text">No repositories yet. Add one and SourceLens indexes its default branch.</p>
          {addButton}
        </div>
      ) : null}
      {repos.data && repos.data.length > 0 ? (
        <ul className="sl-ledger" aria-label="Connected repositories">
          {repos.data.map((repo) => (
            <RepoRow key={repo.id} repo={repo} embeddingsOn={embeddings.data?.enabled ?? false} onSync={(r) => void sync(r)} syncing={syncing === repo.id} />
          ))}
        </ul>
      ) : null}
      <AddRepositories open={adding} onClose={closeDialog} onConnected={() => void repos.reload()} />
    </div>
  );
}

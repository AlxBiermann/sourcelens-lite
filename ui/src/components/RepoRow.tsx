import type { CSSProperties, ReactNode } from 'react';
import { Link } from 'react-router';
import type { Repository } from '../api';
import { ago, plural, shortSha } from '../format';
import { StatusBlock, jobProgress, jobProgressText, repoStatus, type RepoStatus } from './StatusBlock';

function detail(repo: Repository, status: RepoStatus): ReactNode {
  switch (status) {
    case 'indexing':
      return repo.activeJob ? jobProgressText(repo.activeJob) : null;
    case 'failed':
      return <span className="sl-row-error">{repo.lastJob?.error ?? 'Indexing failed.'}</span>;
    case 'disconnected':
      return repo.stateReason ?? 'No access on GitHub';
    case 'queued':
      return repo.indexedCommit ? plural(repo.fileCount, 'file') : 'Waiting to index';
    case 'ready':
      return (
        <>
          {plural(repo.fileCount, 'file')}
          {repo.indexedAt ? <span className="sl-row-time">{ago(repo.indexedAt)}</span> : null}
        </>
      );
  }
}

function Cells({ repo, status }: { repo: Repository; status: RepoStatus }) {
  return (
    <>
      <span className="sl-row-status">
        <StatusBlock status={status} />
      </span>
      <span className="sl-row-name">
        <Link className="sl-row-link" to={`/repositories/${repo.id}`}>
          {repo.fullName}
        </Link>
        {repo.private ? <span className="sl-private">Private</span> : null}
      </span>
      <span className="sl-row-meta">
        <span className="sl-row-branch">{repo.effectiveBranch}</span>
        <span className="sl-row-commit">{shortSha(repo.indexedCommit)}</span>
        <span className="sl-row-detail">{detail(repo, status)}</span>
      </span>
    </>
  );
}

export function RepoRow({ repo, embeddingsOn, onSync, syncing }: { repo: Repository; embeddingsOn: boolean; onSync: (repo: Repository) => void; syncing: boolean }) {
  const status = repoStatus(repo);
  const progress = status === 'indexing' && repo.activeJob ? Math.min(100, Math.max(0, jobProgress(repo.activeJob, embeddingsOn))) : null;
  const idle = !repo.activeJob && repo.state === 'connected';

  return (
    <li className="sl-row" style={progress === null ? undefined : ({ '--p': `${progress}%` } as CSSProperties)}>
      <div className="sl-row-grid">
        <Cells repo={repo} status={status} />
        <span className="sl-row-action">
          {idle ? (
            <button type="button" className="sl-button sl-button-secondary sl-button-small sl-row-button" disabled={syncing} onClick={() => onSync(repo)}>
              Sync now
            </button>
          ) : null}
        </span>
      </div>
      {progress === null ? null : (
        <span className="sl-row-bar" role="progressbar" aria-label={`Indexing ${repo.fullName}`} aria-valuemin={0} aria-valuemax={100} aria-valuenow={Math.round(progress)}>
          <span className="sl-progress-fill" />
        </span>
      )}
    </li>
  );
}

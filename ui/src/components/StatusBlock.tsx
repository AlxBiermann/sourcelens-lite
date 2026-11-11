import type { Job, Repository } from '../api';
import { count } from '../format';

export type RepoStatus = 'ready' | 'indexing' | 'queued' | 'failed' | 'disconnected';

const LABEL: Record<RepoStatus, string> = {
  ready: 'Ready',
  indexing: 'Indexing',
  queued: 'Queued',
  failed: 'Failed',
  disconnected: 'Disconnected',
};

export function repoStatus(repo: Repository): RepoStatus {
  if (repo.activeJob?.status === 'running') return 'indexing';
  if (repo.activeJob?.status === 'queued') return 'queued';
  if (repo.state === 'disconnected') return 'disconnected';
  if (repo.lastJob?.status === 'failed') return 'failed';
  if (repo.indexedCommit) return 'ready';
  return 'queued';
}

/** Share of a running job that is done, 0–100. Indexing fills 0–60 when embeddings follow, else 0–100. */
export function jobProgress(job: Job, embeddingsOn: boolean): number {
  if (job.phase === 'embed') return 60 + (40 * job.embedDone) / Math.max(job.embedTotal, 1);
  if (job.phase === 'index') return ((embeddingsOn ? 60 : 100) * job.filesDone) / Math.max(job.filesTotal, 1);
  return 0;
}

export function jobProgressText(job: Job): string {
  if (job.phase === 'embed') return `Embedding ${count(job.embedDone)} of ${count(job.embedTotal)} chunks`;
  if (job.phase === 'index') return `${count(job.filesDone)} of ${count(job.filesTotal)} files`;
  return 'Reading the branch';
}

export function StatusBlock({ status }: { status: RepoStatus }) {
  return <span className={`sl-status sl-status-${status}`}>{LABEL[status]}</span>;
}

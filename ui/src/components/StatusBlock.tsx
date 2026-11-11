import type { EmbeddingSettings, Job, Repository } from '../api';
import { count } from '../format';
import { useApi } from '../hooks/useApi';

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

/** Share of a job that is done, clamped to 0–100. Indexing fills 0–60 when embeddings follow, else 0–100. */
export function clampedProgress(job: Job, embeddingsOn: boolean): number {
  let share = 0;
  if (job.phase === 'embed') share = 60 + (40 * job.embedDone) / Math.max(job.embedTotal, 1);
  else if (job.phase === 'index') share = ((embeddingsOn ? 60 : 100) * job.filesDone) / Math.max(job.filesTotal, 1);
  return Math.min(100, Math.max(0, share));
}

export function jobProgressText(job: Job): string {
  if (job.status === 'queued') return 'Waiting to index';
  if (job.phase === 'embed') return `Embedding ${count(job.embedDone)} of ${count(job.embedTotal)} chunks`;
  if (job.phase === 'index') return `${count(job.filesDone)} of ${count(job.filesTotal)} files`;
  return 'Reading the branch';
}

/** Whether embeddings are on; decides how indexing progress is scaled. */
export function useEmbeddingsOn(): boolean {
  return useApi<EmbeddingSettings>('/api/settings/embeddings').data?.enabled ?? false;
}

export function StatusBlock({ status }: { status: RepoStatus }) {
  return <span className={`sl-status sl-status-${status}`}>{LABEL[status]}</span>;
}

import { useState, type CSSProperties } from 'react';
import { Link, useNavigate, useParams } from 'react-router';
import { api, errorMessage, type EmbeddingSettings, type Job, type JobTrigger, type RepositoryDetail as Detail } from '../api';
import { ConfirmDialog } from '../components/Dialog';
import { Field } from '../components/Field';
import { Loading, Notice, PageHeader } from '../components/Shell';
import { StatusBlock, jobProgress, jobProgressText, repoStatus } from '../components/StatusBlock';
import { useToast } from '../components/Toast';
import { useApi } from '../hooks/useApi';
import { ago, count, duration, shortSha } from '../format';

const TRIGGER: Record<JobTrigger, string> = {
  connect: 'Connected',
  webhook: 'Push',
  manual: 'Manual',
  startup: 'Restart',
  settings: 'Embedding settings',
};

const JOB_STATUS: Record<Job['status'], string> = {
  queued: 'Queued',
  running: 'Running',
  succeeded: 'Succeeded',
  failed: 'Failed',
};

const SKIP_REASON: Record<string, string> = {
  binary: 'Binary files',
  minified: 'Minified files',
  generated: 'Generated files',
  not_in_archive: 'Missing from the GitHub archive',
};

type Branches = { defaultBranch: string; branches: string[] };

export function RepositoryDetail() {
  const { id } = useParams();
  const navigate = useNavigate();
  const notify = useToast();
  const [active, setActive] = useState(false);
  const detail = useApi<Detail>(`/api/repositories/${id}`, { pollMs: active ? 2000 : 15000 });
  const branches = useApi<Branches>(`/api/repositories/${id}/branches`);
  const embeddings = useApi<EmbeddingSettings>('/api/settings/embeddings');
  const [busy, setBusy] = useState(false);
  const [confirming, setConfirming] = useState(false);

  const repo = detail.data?.repository;
  const isActive = repo?.activeJob != null;
  if (detail.data && isActive !== active) setActive(isActive);

  if (!repo) {
    return (
      <div className="sl-page">
        <Link className="sl-link" to="/repositories">
          Repositories
        </Link>
        {detail.error ? <Notice tone="error">{detail.error.message}</Notice> : <Loading />}
      </div>
    );
  }

  async function sync(force: boolean) {
    setBusy(true);
    try {
      await api.post(`/api/repositories/${id}/sync`, { force });
      notify(force ? 'Rebuild started' : 'Sync started');
      await detail.reload();
    } catch (e) {
      notify(errorMessage(e), 'error');
    } finally {
      setBusy(false);
    }
  }

  async function changeBranch(value: string) {
    setBusy(true);
    try {
      await api.patch(`/api/repositories/${id}`, { branch: value === '' ? null : value });
      notify(value === '' ? 'Following the default branch' : `Branch set to ${value}`);
      await detail.reload();
    } catch (e) {
      notify(errorMessage(e), 'error');
    } finally {
      setBusy(false);
    }
  }

  async function disconnect() {
    if (!repo) return;
    setBusy(true);
    try {
      await api.del(`/api/repositories/${id}`);
      notify(`Disconnected ${repo.fullName}`);
      navigate('/repositories');
    } catch (e) {
      notify(errorMessage(e), 'error');
      setBusy(false);
      setConfirming(false);
    }
  }

  const status = repoStatus(repo);
  const job = repo.activeJob;
  const progress = job?.status === 'running' ? Math.min(100, jobProgress(job, embeddings.data?.enabled ?? false)) : null;
  const branchOptions = branches.data?.branches ?? [repo.effectiveBranch];
  const connected = repo.state === 'connected';

  return (
    <div className="sl-page">
      <Link className="sl-link" to="/repositories">
        Repositories
      </Link>
      <PageHeader
        title={repo.fullName}
        mono
        action={
          <button type="button" className="sl-button sl-button-primary" disabled={busy || !connected || job !== null} onClick={() => void sync(false)}>
            Sync now
          </button>
        }
      />
      {repo.state === 'disconnected' ? <Notice tone="error">{repo.stateReason ?? 'GitHub no longer shares this repository.'} Search and API keys skip it until access returns.</Notice> : null}

      <div className="sl-facts-row">
        <StatusBlock status={status} />
        <dl className="sl-facts">
          <div className="sl-fact">
            <dt className="sl-fact-label">Branch</dt>
            <dd className="sl-fact-value">{repo.effectiveBranch}</dd>
          </div>
          <div className="sl-fact">
            <dt className="sl-fact-label">Commit</dt>
            <dd className="sl-fact-value sl-mono">{repo.indexedCommit ? shortSha(repo.indexedCommit) : 'None yet'}</dd>
          </div>
          <div className="sl-fact">
            <dt className="sl-fact-label">Files</dt>
            <dd className="sl-fact-value">{count(repo.fileCount)}</dd>
          </div>
          <div className="sl-fact">
            <dt className="sl-fact-label">Chunks</dt>
            <dd className="sl-fact-value">{count(repo.chunkCount)}</dd>
          </div>
          <div className="sl-fact">
            <dt className="sl-fact-label">Embedded chunks</dt>
            <dd className="sl-fact-value">{count(repo.embeddedCount)}</dd>
          </div>
          {repo.indexedAt ? (
            <div className="sl-fact">
              <dt className="sl-fact-label">Indexed</dt>
              <dd className="sl-fact-value">{ago(repo.indexedAt)}</dd>
            </div>
          ) : null}
        </dl>
      </div>

      {job ? (
        <div className="sl-progress" style={{ '--p': `${progress ?? 0}%` } as CSSProperties}>
          <div className="sl-progress-track" role="progressbar" aria-label="Indexing progress" aria-valuemin={0} aria-valuemax={100} aria-valuenow={Math.round(progress ?? 0)}>
            <div className="sl-progress-fill" />
          </div>
          <p className="sl-progress-text">{job.status === 'queued' ? 'Waiting for a free worker.' : jobProgressText(job)}</p>
        </div>
      ) : null}

      <section className="sl-section" aria-labelledby="branch-heading">
        <h2 className="sl-section-title" id="branch-heading">
          Branch and index
        </h2>
        <div className="sl-toolbar">
          <Field label="Branch">
            {(props) => (
              <select {...props} className="sl-select" value={repo.branch ?? ''} disabled={busy || !connected} onChange={(e) => void changeBranch(e.target.value)}>
                <option value="">Follow default branch ({repo.defaultBranch})</option>
                {branchOptions
                  .filter((name) => name !== '')
                  .map((name) => (
                    <option key={name} value={name}>
                      {name}
                    </option>
                  ))}
              </select>
            )}
          </Field>
          <div className="sl-actions">
            <button type="button" className="sl-button sl-button-secondary" disabled={busy || !connected || job !== null} onClick={() => void sync(true)}>
              Rebuild index
            </button>
            <button type="button" className="sl-button sl-button-danger" disabled={busy} onClick={() => setConfirming(true)}>
              Disconnect
            </button>
          </div>
        </div>
      </section>

      <section className="sl-section" aria-labelledby="activity-heading">
        <h2 className="sl-section-title" id="activity-heading">
          Activity
        </h2>
        {detail.data && detail.data.jobs.length > 0 ? (
          <div className="sl-table-wrap" tabIndex={0} role="region" aria-labelledby="activity-heading">
            <table className="sl-table">
              <thead>
                <tr className="sl-tr">
                  <th className="sl-th">Trigger</th>
                  <th className="sl-th">Commit</th>
                  <th className="sl-th sl-num">Changed</th>
                  <th className="sl-th sl-num">Deleted</th>
                  <th className="sl-th sl-num">Skipped</th>
                  <th className="sl-th sl-num">Duration</th>
                  <th className="sl-th">Status</th>
                </tr>
              </thead>
              <tbody>
                {detail.data.jobs.map((j) => (
                  <JobRows key={j.id} job={j} />
                ))}
              </tbody>
            </table>
          </div>
        ) : (
          <p className="sl-muted">No index runs yet.</p>
        )}
      </section>

      <section className="sl-section" aria-labelledby="skipped-heading">
        <h2 className="sl-section-title" id="skipped-heading">
          Skipped files
        </h2>
        {detail.data && detail.data.skipped.length > 0 ? (
          <ul className="sl-counts">
            {detail.data.skipped.map((s) => (
              <li key={s.reason} className="sl-count-row">
                <span>{SKIP_REASON[s.reason] ?? s.reason}</span>
                <span className="sl-num">{count(s.count)}</span>
              </li>
            ))}
          </ul>
        ) : (
          <p className="sl-muted">No files were skipped.</p>
        )}
      </section>

      <ConfirmDialog
        open={confirming}
        title={`Disconnect ${repo.fullName}?`}
        body="Its index is deleted. API keys keep working for other repositories."
        confirmLabel="Disconnect repository"
        busy={busy}
        onConfirm={() => void disconnect()}
        onClose={() => setConfirming(false)}
      />
    </div>
  );
}

function JobRows({ job }: { job: Job }) {
  const cell = job.error ? 'sl-td sl-td-joined' : 'sl-td';
  return (
    <>
      <tr className="sl-tr">
        <td className={cell}>{TRIGGER[job.trigger]}</td>
        <td className={`${cell} sl-mono`}>{shortSha(job.targetCommit)}</td>
        <td className={`${cell} sl-num`}>{count(job.filesChanged)}</td>
        <td className={`${cell} sl-num`}>{count(job.filesDeleted)}</td>
        <td className={`${cell} sl-num`}>{count(job.filesSkipped)}</td>
        <td className={`${cell} sl-num`}>{duration(job.startedAt, job.finishedAt)}</td>
        <td className={job.status === 'failed' ? `${cell} sl-text-red` : cell}>
          {JOB_STATUS[job.status]}
          <span className="sl-td-sub">{ago(job.createdAt)}</span>
        </td>
      </tr>
      {job.error ? (
        <tr className="sl-tr">
          <td className="sl-td sl-text-red" colSpan={7}>
            {job.error}
          </td>
        </tr>
      ) : null}
    </>
  );
}

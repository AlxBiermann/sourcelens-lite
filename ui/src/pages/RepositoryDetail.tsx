import { useState, type CSSProperties, type FormEvent } from 'react';
import { Link, useNavigate, useParams } from 'react-router';
import { api, type Job, type JobTrigger, type RepositoryDetail as Detail } from '../api';
import { ConfirmDialog } from '../components/Dialog';
import { Field } from '../components/Field';
import { LoadState } from '../components/LoadState';
import { CellLabel, Notice, PageHeader } from '../components/Shell';
import { StatusBlock, clampedProgress, jobProgressText, repoStatus, useEmbeddingsOn } from '../components/StatusBlock';
import { useAction } from '../hooks/useAction';
import { useApi } from '../hooks/useApi';
import { ago, count, duration, sentence, shortSha } from '../format';

const TRIGGER: Record<JobTrigger, string> = {
  connect: 'Connect',
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
  const [active, setActive] = useState(false);
  const detail = useApi<Detail>(`/api/repositories/${id}`, { pollMs: active ? 2000 : 15000 });
  const branches = useApi<Branches>(`/api/repositories/${id}/branches`);
  const embeddingsOn = useEmbeddingsOn();
  const action = useAction();
  const [confirming, setConfirming] = useState(false);
  // The branch picked in the select; null = unchanged. Applied only by the "Change branch" button.
  const [branchChoice, setBranchChoice] = useState<string | null>(null);

  const repo = detail.data?.repository;
  const isActive = repo?.activeJob != null;
  if (detail.data && isActive !== active) setActive(isActive);

  if (!repo) {
    return (
      <div className="sl-page">
        <Link className="sl-link" to="/repositories">
          Repositories
        </Link>
        <LoadState state={detail} />
      </div>
    );
  }

  function sync(force: boolean) {
    void action.run(
      async () => {
        await api.post(`/api/repositories/${id}/sync`, { force });
        await detail.reload();
      },
      { key: force ? 'rebuild' : 'sync', success: force ? 'Rebuild started' : 'Sync started' },
    );
  }

  function changeBranch(event: FormEvent) {
    event.preventDefault();
    if (branchChoice === null) return;
    const value = branchChoice;
    void action.run(
      async () => {
        await api.patch(`/api/repositories/${id}`, { branch: value === '' ? null : value });
        setBranchChoice(null);
        await detail.reload();
      },
      { key: 'branch', success: value === '' ? 'Following the default branch' : `Branch set to ${value}` },
    );
  }

  async function disconnect() {
    if (!repo) return;
    const name = repo.fullName;
    const done = await action.run(() => api.del(`/api/repositories/${id}`), { key: 'disconnect', success: `Disconnected ${name}` });
    if (done) navigate('/repositories');
    else setConfirming(false);
  }

  const status = repoStatus(repo);
  const job = repo.activeJob;
  const progress = job?.status === 'running' ? clampedProgress(job, embeddingsOn) : 0;
  // GET …/branches returns only GitHub's first page; keep the tracked branch selectable when it is not on it.
  const listed = branches.data?.branches ?? [repo.effectiveBranch];
  const branchOptions = repo.branch !== null && !listed.includes(repo.branch) ? [repo.branch, ...listed] : listed;
  const connected = repo.state === 'connected';
  const savedBranch = repo.branch ?? '';
  const selectedBranch = branchChoice ?? savedBranch;
  const busy = action.busy !== null;

  return (
    <div className="sl-page">
      <Link className="sl-link" to="/repositories">
        Repositories
      </Link>
      <PageHeader
        title={repo.fullName}
        mono
        action={
          <button type="button" className="sl-button sl-button-primary" disabled={busy || job !== null} onClick={() => sync(false)}>
            Sync now
          </button>
        }
      />
      <LoadState state={detail} />
      {repo.state === 'disconnected' ? (
        <Notice tone="error">
          {sentence(repo.stateReason ?? 'GitHub no longer shares this repository')} Search and API keys skip it until access returns.
        </Notice>
      ) : null}
      {status === 'failed' && repo.lastJob?.error ? <Notice tone="error">{repo.lastJob.error}</Notice> : null}

      <div className="sl-facts-row">
        <StatusBlock status={status} />
        <dl className="sl-facts">
          <div className="sl-fact">
            <dt className="sl-fact-label">Branch</dt>
            <dd className="sl-fact-value sl-break">{repo.effectiveBranch}</dd>
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
        <div className="sl-progress" style={{ '--p': `${progress}%` } as CSSProperties}>
          <div className="sl-progress-track" role="progressbar" aria-label="Indexing progress" aria-valuemin={0} aria-valuemax={100} aria-valuenow={Math.round(progress)}>
            <div className="sl-progress-fill" />
          </div>
          <p className="sl-progress-text">{jobProgressText(job)}</p>
        </div>
      ) : null}

      <section className="sl-section" aria-labelledby="branch-heading">
        <h2 className="sl-section-title" id="branch-heading">
          Branch and index
        </h2>
        <div className="sl-toolbar">
          <form className="sl-inline-form" onSubmit={changeBranch}>
            <Field label="Branch">
              {(props) => (
                <select {...props} className="sl-select" value={selectedBranch} disabled={!connected} onChange={(e) => setBranchChoice(e.target.value === savedBranch ? null : e.target.value)}>
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
            <button type="submit" className="sl-button sl-button-secondary" disabled={!connected || branchChoice === null || busy}>
              Change branch
            </button>
          </form>
          <div className="sl-actions">
            <button type="button" className="sl-button sl-button-secondary" disabled={busy || job !== null} onClick={() => sync(true)}>
              Rebuild index
            </button>
            <button type="button" className="sl-button sl-button-quiet-danger" disabled={busy} onClick={() => setConfirming(true)}>
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
              <thead className="sl-thead">
                <tr className="sl-tr">
                  <th className="sl-th">Trigger</th>
                  <th className="sl-th">Status</th>
                  <th className="sl-th">Commit</th>
                  <th className="sl-th sl-num">Changed</th>
                  <th className="sl-th sl-num">Deleted</th>
                  <th className="sl-th sl-num">Skipped</th>
                  <th className="sl-th sl-num">Duration</th>
                </tr>
              </thead>
              <tbody className="sl-tbody">
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
      <tr className={job.error ? 'sl-tr sl-tr-joined' : 'sl-tr'}>
        <td className={cell}>
          <CellLabel>Trigger</CellLabel>
          {TRIGGER[job.trigger]}
        </td>
        <td className={job.status === 'failed' ? `${cell} sl-text-red` : cell}>
          <CellLabel>Status</CellLabel>
          <span>
            {JOB_STATUS[job.status]}
            <span className="sl-td-sub">{ago(job.createdAt)}</span>
          </span>
        </td>
        <td className={`${cell} sl-mono`}>
          <CellLabel>Commit</CellLabel>
          {shortSha(job.targetCommit)}
        </td>
        <td className={`${cell} sl-num`}>
          <CellLabel>Changed</CellLabel>
          {count(job.filesChanged)}
        </td>
        <td className={`${cell} sl-num`}>
          <CellLabel>Deleted</CellLabel>
          {count(job.filesDeleted)}
        </td>
        <td className={`${cell} sl-num`}>
          <CellLabel>Skipped</CellLabel>
          {count(job.filesSkipped)}
        </td>
        <td className={`${cell} sl-num`}>
          <CellLabel>Duration</CellLabel>
          {duration(job.startedAt, job.finishedAt)}
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

import { useId, useRef, useState, type FormEvent, type KeyboardEvent } from 'react';
import { api, type ApiKey, type Repository } from '../api';
import { Snippet } from '../components/CodeBlock';
import { CopyButton } from '../components/CopyButton';
import { ConfirmDialog, Dialog } from '../components/Dialog';
import { Choice, Field } from '../components/Field';
import { LoadState } from '../components/LoadState';
import { CellLabel, PageHeader } from '../components/Shell';
import { useAction } from '../hooks/useAction';
import { useApi } from '../hooks/useApi';
import { useSession } from '../hooks/useSession';
import { ago, date, plural } from '../format';

const EXPIRY: { label: string; days: number | null }[] = [
  { label: 'Never', days: null },
  { label: '30 days', days: 30 },
  { label: '90 days', days: 90 },
  { label: '1 year', days: 365 },
];

type Created = { key: ApiKey; secret: string };

function snippets(url: string, secret: string) {
  const header = { Authorization: `Bearer ${secret}` };
  const cursor = JSON.stringify({ mcpServers: { sourcelens: { url, headers: header } } }, null, 2);
  return [
    { id: 'claude', label: 'Claude Code', caption: 'Run in a terminal.', text: `claude mcp add --transport http sourcelens ${url} --header "Authorization: Bearer ${secret}"` },
    { id: 'cursor', label: 'Cursor', caption: 'Add to ~/.cursor/mcp.json.', text: cursor },
    { id: 'vscode', label: 'VS Code', caption: 'Add to .vscode/mcp.json.', text: JSON.stringify({ servers: { sourcelens: { type: 'http', url, headers: header } } }, null, 2) },
    { id: 'generic', label: 'Generic MCP client', caption: 'Streamable HTTP with a bearer token.', text: cursor },
  ];
}

function AccessPicker({ all, setAll, selected, setSelected, repos }: { all: boolean; setAll: (all: boolean) => void; selected: number[]; setSelected: (ids: number[]) => void; repos: Repository[] }) {
  return (
    <fieldset className="sl-fieldset">
      <legend className="sl-legend">Access</legend>
      <div className="sl-choices">
        <Choice type="radio" name="access" checked={all} onChange={() => setAll(true)} label="All repositories, including ones added later" />
        <Choice type="radio" name="access" checked={!all} onChange={() => setAll(false)} label="Selected repositories" />
      </div>
      {!all ? (
        repos.length > 0 ? (
          <div className="sl-checklist">
            {repos.map((repo) => (
              <Choice
                key={repo.id}
                type="checkbox"
                checked={selected.includes(repo.id)}
                onChange={(checked) => setSelected(checked ? [...selected, repo.id] : selected.filter((id) => id !== repo.id))}
                label={repo.fullName}
              />
            ))}
          </div>
        ) : (
          <p className="sl-hint">No repositories are connected yet.</p>
        )
      ) : null}
    </fieldset>
  );
}

function CreatedKey({ created, publicUrl, titleId }: { created: Created; publicUrl: string; titleId: string }) {
  const tabs = snippets(`${publicUrl}/mcp`, created.secret);
  const [tab, setTab] = useState(tabs[0].id);
  const current = tabs.find((t) => t.id === tab) ?? tabs[0];
  const baseId = useId();
  const secretRef = useRef<HTMLElement>(null);
  const snippetRef = useRef<HTMLPreElement>(null);
  const tabRefs = useRef<(HTMLButtonElement | null)[]>([]);

  function onTabKey(event: KeyboardEvent<HTMLButtonElement>, index: number) {
    const last = tabs.length - 1;
    const next = event.key === 'ArrowRight' ? (index === last ? 0 : index + 1) : event.key === 'ArrowLeft' ? (index === 0 ? last : index - 1) : event.key === 'Home' ? 0 : event.key === 'End' ? last : null;
    if (next === null) return;
    event.preventDefault();
    setTab(tabs[next].id);
    tabRefs.current[next]?.focus();
  }

  return (
    <div className="sl-stack">
      <div className="sl-secret">
        <code className="sl-secret-value" ref={secretRef}>
          {created.secret}
        </code>
        <CopyButton text={created.secret} label="Copy key" className="sl-button sl-button-primary sl-button-small" target={secretRef} what="key" autoFocus describedBy={titleId} />
      </div>
      <div className="sl-stack-tight">
        <div className="sl-tabs" role="tablist" aria-label="Client configuration">
          {tabs.map((t, index) => (
            <button
              key={t.id}
              ref={(el) => {
                tabRefs.current[index] = el;
              }}
              type="button"
              role="tab"
              id={`${baseId}-tab-${t.id}`}
              aria-selected={t.id === tab}
              aria-controls={`${baseId}-panel`}
              tabIndex={t.id === tab ? 0 : -1}
              className={t.id === tab ? 'sl-tab sl-tab-active' : 'sl-tab'}
              onClick={() => setTab(t.id)}
              onKeyDown={(event) => onTabKey(event, index)}
            >
              {t.label}
            </button>
          ))}
        </div>
        <div className="sl-stack-tight" role="tabpanel" id={`${baseId}-panel`} aria-labelledby={`${baseId}-tab-${current.id}`}>
          <div className="sl-snippet-head">
            <p className="sl-hint">{current.caption}</p>
            <CopyButton text={current.text} target={snippetRef} what="configuration" />
          </div>
          <Snippet text={current.text} ref={snippetRef} />
        </div>
      </div>
    </div>
  );
}

export function ApiKeys() {
  const { state } = useSession();
  const keys = useApi<ApiKey[]>('/api/keys');
  const repos = useApi<Repository[]>('/api/repositories');
  const action = useAction();
  const titleId = useId();

  const [editing, setEditing] = useState<ApiKey | 'new' | null>(null);
  const [created, setCreated] = useState<Created | null>(null);
  const [revoking, setRevoking] = useState<ApiKey | null>(null);
  const [name, setName] = useState('');
  const [expiry, setExpiry] = useState<number | null>(null);
  const [all, setAll] = useState(true);
  const [selected, setSelected] = useState<number[]>([]);
  const [formError, setFormError] = useState<string | null>(null);

  const repoList = repos.data ?? [];
  const busy = action.busy !== null;

  function open(target: ApiKey | 'new') {
    setEditing(target);
    setCreated(null);
    setFormError(null);
    setName(target === 'new' ? '' : target.name);
    setExpiry(null);
    setAll(target === 'new' ? true : target.allRepositories);
    setSelected(target === 'new' ? [] : target.repositoryIds);
  }

  function close() {
    setEditing(null);
    setCreated(null);
  }

  function save(event: FormEvent) {
    event.preventDefault();
    if (!name.trim()) {
      setFormError('Enter a name.');
      return;
    }
    if (!all && selected.length === 0) {
      setFormError('Select at least one repository.');
      return;
    }
    setFormError(null);
    const repositoryIds = all ? null : selected;
    if (editing === 'new') {
      // The dialog itself shows the result, so no toast.
      void action.run(
        async () => {
          setCreated(await api.post<Created>('/api/keys', { name: name.trim(), expiresInDays: expiry, repositoryIds }));
          await keys.reload();
        },
        { onError: setFormError },
      );
    } else if (editing) {
      const id = editing.id;
      void action.run(
        async () => {
          await api.patch<ApiKey>(`/api/keys/${id}`, { name: name.trim(), repositoryIds });
          close();
          await keys.reload();
        },
        { success: 'Key saved', onError: setFormError },
      );
    }
  }

  function revoke() {
    if (!revoking) return;
    const id = revoking.id;
    void action.run(
      async () => {
        await api.del(`/api/keys/${id}`);
        setRevoking(null);
        await keys.reload();
      },
      { success: 'Key revoked' },
    );
  }

  const now = Date.now() / 1000;
  const createButton = (
    <button type="button" className="sl-button sl-button-primary" onClick={() => open('new')}>
      Create key
    </button>
  );

  return (
    <div className="sl-page">
      <PageHeader title="API keys" action={keys.data?.length === 0 ? undefined : createButton} />
      <p className="sl-prose sl-muted">Agents use API keys to search your repositories over MCP. Every key is read-only.</p>
      <LoadState state={keys} />
      {keys.data && keys.data.length === 0 ? (
        <div className="sl-empty">
          <p className="sl-empty-text">No API keys. Create one to connect an agent over MCP.</p>
          {createButton}
        </div>
      ) : null}
      {keys.data && keys.data.length > 0 ? (
        <div className="sl-table-wrap" tabIndex={0} role="region" aria-label="API keys">
          <table className="sl-table">
            <thead className="sl-thead">
              <tr className="sl-tr">
                <th className="sl-th">Name</th>
                <th className="sl-th">Access</th>
                <th className="sl-th">Last used</th>
                <th className="sl-th">Expires</th>
                <th className="sl-th">
                  <span className="sl-visually-hidden">Actions</span>
                </th>
              </tr>
            </thead>
            <tbody className="sl-tbody">
              {keys.data.map((key) => {
                const expired = key.expiresAt !== null && key.expiresAt < now;
                return (
                  <tr key={key.id} className="sl-tr">
                    <td className="sl-td">
                      <span>
                        <span className="sl-td-main">{key.name}</span>
                        <span className="sl-td-sub sl-mono">sl_…{key.last4}</span>
                      </span>
                    </td>
                    <td className="sl-td">
                      <CellLabel>Access</CellLabel>
                      {key.allRepositories ? 'All repositories' : plural(key.repositoryIds.length, 'repository', 'repositories')}
                    </td>
                    <td className="sl-td">
                      <CellLabel>Last used</CellLabel>
                      {key.lastUsedAt ? ago(key.lastUsedAt) : 'Never'}
                    </td>
                    <td className={expired ? 'sl-td sl-text-red' : 'sl-td'}>
                      <CellLabel>Expires</CellLabel>
                      {key.expiresAt === null ? 'Never' : expired ? `Expired ${date(key.expiresAt)}` : date(key.expiresAt)}
                    </td>
                    <td className="sl-td sl-td-actions">
                      <div className="sl-cell-buttons">
                        <button type="button" className="sl-button sl-button-secondary sl-button-small" onClick={() => open(key)}>
                          Edit
                        </button>
                        <button type="button" className="sl-button sl-button-quiet-danger sl-button-small" onClick={() => setRevoking(key)}>
                          Revoke
                        </button>
                      </div>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      ) : null}

      <Dialog
        open={editing !== null}
        title={created ? 'Copy your key now. It is not shown again.' : editing === 'new' ? 'Create key' : 'Edit key'}
        titleId={titleId}
        onClose={close}
        wide={created !== null}
        dismissible={created === null}
        actions={
          created ? (
            <button type="button" className="sl-button sl-button-secondary" onClick={close}>
              Done
            </button>
          ) : (
            <>
              <button type="submit" form="key-form" className="sl-button sl-button-primary" disabled={busy}>
                {editing === 'new' ? 'Create key' : 'Save key'}
              </button>
              <button type="button" className="sl-button sl-button-secondary" onClick={close}>
                Cancel
              </button>
            </>
          )
        }
      >
        {created && state ? (
          <CreatedKey created={created} publicUrl={state.publicUrl} titleId={titleId} />
        ) : (
          <form id="key-form" className="sl-form" onSubmit={save}>
            <Field label="Name" hint="Name it after the agent or machine that uses it.">
              {(props) => <input {...props} className="sl-input" value={name} onChange={(e) => setName(e.target.value)} autoComplete="off" maxLength={100} />}
            </Field>
            {editing === 'new' ? (
              <fieldset className="sl-fieldset">
                <legend className="sl-legend">Expires</legend>
                <div className="sl-segmented">
                  {EXPIRY.map((option) => (
                    <Choice key={option.label} type="radio" name="expiry" checked={expiry === option.days} onChange={() => setExpiry(option.days)} label={option.label} />
                  ))}
                </div>
              </fieldset>
            ) : null}
            <AccessPicker all={all} setAll={setAll} selected={selected} setSelected={setSelected} repos={repoList} />
            {formError ? (
              <p className="sl-field-error" role="alert">
                {formError}
              </p>
            ) : null}
          </form>
        )}
      </Dialog>

      <ConfirmDialog
        open={revoking !== null}
        title={`Revoke ${revoking?.name ?? ''}?`}
        body="Agents using it lose access immediately."
        confirmLabel="Revoke key"
        busy={busy}
        onConfirm={revoke}
        onClose={() => setRevoking(null)}
      />
    </div>
  );
}

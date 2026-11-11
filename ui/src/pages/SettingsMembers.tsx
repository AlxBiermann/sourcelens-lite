import { useState, type FormEvent } from 'react';
import { Navigate } from 'react-router';
import { api, type Member } from '../api';
import { ConfirmDialog } from '../components/Dialog';
import { Field } from '../components/Field';
import { LoadState } from '../components/LoadState';
import { CellLabel, PageHeader, SettingsTabs } from '../components/Shell';
import { useAction } from '../hooks/useAction';
import { useApi } from '../hooks/useApi';
import { useUser } from '../hooks/useSession';
import { ago } from '../format';

export function SettingsMembers() {
  const user = useUser();
  const owner = user.role === 'owner';
  const members = useApi<Member[]>(owner ? '/api/members' : null);
  const action = useAction();
  const [login, setLogin] = useState('');
  const [formError, setFormError] = useState<string | null>(null);
  const [removing, setRemoving] = useState<Member | null>(null);

  if (!owner) return <Navigate to="/settings/embeddings" replace />;

  const busy = action.busy !== null;

  function add(event: FormEvent) {
    event.preventDefault();
    const value = login.trim().replace(/^@/, '');
    if (!/^[A-Za-z0-9-]{1,39}$/.test(value)) {
      setFormError('Enter a GitHub username: letters, digits and hyphens, at most 39 characters.');
      return;
    }
    setFormError(null);
    void action.run(
      async () => {
        await api.post<Member>('/api/members', { login: value });
        setLogin('');
        await members.reload();
      },
      { key: 'add', success: 'Member added', onError: setFormError },
    );
  }

  function remove() {
    if (!removing) return;
    const id = removing.id;
    void action.run(
      async () => {
        await api.del(`/api/members/${id}`);
        setRemoving(null);
        await members.reload();
      },
      { key: 'remove', success: 'Member removed' },
    );
  }

  return (
    <div className="sl-page sl-page-settings">
      <PageHeader title="Settings" />
      <SettingsTabs owner />
      <p className="sl-prose sl-muted">Members sign in with GitHub. Members can search every connected repository and manage all API keys. Only you manage members and embedding settings.</p>

      <form className="sl-inline-form" onSubmit={add}>
        <Field label="GitHub username" error={formError}>
          {(props) => <input {...props} className="sl-input sl-input-mono" value={login} onChange={(e) => setLogin(e.target.value)} autoComplete="off" spellCheck={false} />}
        </Field>
        <button type="submit" className="sl-button sl-button-primary" disabled={busy || !login.trim()}>
          Add member
        </button>
      </form>

      <LoadState state={members} />
      {members.data ? (
        <div className="sl-table-wrap" tabIndex={0} role="region" aria-label="Members">
          <table className="sl-table">
            <thead className="sl-thead">
              <tr className="sl-tr">
                <th className="sl-th">GitHub username</th>
                <th className="sl-th">Role</th>
                <th className="sl-th">Last sign-in</th>
                <th className="sl-th">
                  <span className="sl-visually-hidden">Actions</span>
                </th>
              </tr>
            </thead>
            <tbody className="sl-tbody">
              {members.data.map((member) => (
                <tr key={member.id} className="sl-tr">
                  <td className="sl-td">
                    <span className="sl-member">
                      {member.avatarUrl ? <img className="sl-avatar" src={member.avatarUrl} alt="" width={32} height={32} /> : <span className="sl-avatar" aria-hidden="true" />}
                      <span>
                        <span className="sl-td-main">{member.login}</span>
                        {member.name ? <span className="sl-td-sub">{member.name}</span> : null}
                      </span>
                    </span>
                  </td>
                  <td className="sl-td">
                    <CellLabel>Role</CellLabel>
                    <span>
                      {member.role === 'owner' ? 'Owner' : 'Member'}
                      {member.pending ? <span className="sl-pending">Invited</span> : null}
                    </span>
                  </td>
                  <td className="sl-td">
                    <CellLabel>Last sign-in</CellLabel>
                    {member.lastLoginAt ? ago(member.lastLoginAt) : 'Never'}
                  </td>
                  <td className="sl-td sl-td-actions">
                    {member.role === 'owner' ? null : (
                      <div className="sl-cell-buttons">
                        <button type="button" className="sl-button sl-button-quiet-danger sl-button-small" onClick={() => setRemoving(member)}>
                          Remove
                        </button>
                      </div>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      ) : null}

      <ConfirmDialog
        open={removing !== null}
        title={`Remove ${removing?.login ?? ''}?`}
        body="They are signed out and can no longer use SourceLens. The API keys they created stop working."
        confirmLabel="Remove member"
        busy={busy}
        onConfirm={remove}
        onClose={() => setRemoving(null)}
      />
    </div>
  );
}

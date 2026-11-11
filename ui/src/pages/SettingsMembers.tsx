import { useState, type FormEvent } from 'react';
import { Navigate } from 'react-router';
import { api, errorMessage, type Member } from '../api';
import { ConfirmDialog } from '../components/Dialog';
import { Field } from '../components/Field';
import { Loading, Notice, PageHeader, SettingsTabs } from '../components/Shell';
import { useToast } from '../components/Toast';
import { useApi } from '../hooks/useApi';
import { useUser } from '../hooks/useSession';
import { ago } from '../format';

export function SettingsMembers() {
  const user = useUser();
  const owner = user.role === 'owner';
  const members = useApi<Member[]>(owner ? '/api/members' : null);
  const notify = useToast();
  const [login, setLogin] = useState('');
  const [formError, setFormError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [removing, setRemoving] = useState<Member | null>(null);

  if (!owner) return <Navigate to="/settings/embeddings" replace />;

  async function add(event: FormEvent) {
    event.preventDefault();
    const value = login.trim().replace(/^@/, '');
    if (!/^[A-Za-z0-9-]{1,39}$/.test(value)) {
      setFormError('Enter a GitHub username: letters, digits and hyphens, at most 39 characters.');
      return;
    }
    setBusy(true);
    setFormError(null);
    try {
      await api.post<Member>('/api/members', { login: value });
      notify('Member added');
      setLogin('');
      await members.reload();
    } catch (e) {
      setFormError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  }

  async function remove() {
    if (!removing) return;
    setBusy(true);
    try {
      await api.del(`/api/members/${removing.id}`);
      notify('Member removed');
      setRemoving(null);
      await members.reload();
    } catch (e) {
      notify(errorMessage(e), 'error');
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="sl-page">
      <PageHeader title="Settings" />
      <SettingsTabs owner />
      <p className="sl-prose sl-muted">Members sign in with GitHub. They can connect repositories and create API keys; only you manage members and embedding settings.</p>

      <form className="sl-inline-form" onSubmit={(e) => void add(e)}>
        <Field label="GitHub username" error={formError}>
          {(props) => <input {...props} className="sl-input sl-input-mono" value={login} onChange={(e) => setLogin(e.target.value)} autoComplete="off" spellCheck={false} />}
        </Field>
        <button type="submit" className="sl-button sl-button-primary" disabled={busy || !login.trim()}>
          Add member
        </button>
      </form>

      {members.loading && !members.data ? <Loading /> : null}
      {members.error && !members.data ? <Notice tone="error">{members.error.message}</Notice> : null}
      {members.data ? (
        <div className="sl-table-wrap" tabIndex={0} role="region" aria-label="Members">
          <table className="sl-table">
            <thead>
              <tr className="sl-tr">
                <th className="sl-th">Login</th>
                <th className="sl-th">Role</th>
                <th className="sl-th">Last sign-in</th>
                <th className="sl-th">
                  <span className="sl-visually-hidden">Actions</span>
                </th>
              </tr>
            </thead>
            <tbody>
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
                    {member.role === 'owner' ? 'Owner' : 'Member'}
                    {member.pending ? <span className="sl-pending">Invited</span> : null}
                  </td>
                  <td className="sl-td">{member.lastLoginAt ? ago(member.lastLoginAt) : 'Never'}</td>
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
        body="They are signed out and can no longer use SourceLens. API keys they created keep working."
        confirmLabel="Remove member"
        busy={busy}
        onConfirm={() => void remove()}
        onClose={() => setRemoving(null)}
      />
    </div>
  );
}

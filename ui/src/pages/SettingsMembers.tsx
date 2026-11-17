import { useEffect, useRef, useState, type FormEvent } from 'react';
import { Navigate } from 'react-router';
import { api, type Member, type Role } from '../api';
import { ConfirmDialog, Dialog } from '../components/Dialog';
import { Field } from '../components/Field';
import { LoadState } from '../components/LoadState';
import { CellLabel, PageHeader, SettingsTabs } from '../components/Shell';
import { useAction } from '../hooks/useAction';
import { useApi } from '../hooks/useApi';
import { useSession, useUser } from '../hooks/useSession';
import { ago } from '../format';

/** Where focus goes once an action settled: a member's row actions, or the table when the row is gone. */
type FocusTarget = { member: number } | 'table';

function removeCopy(member: Member) {
  if (member.awaitingApproval) {
    return {
      title: `Reject ${member.login}?`,
      body: `${member.login} does not get access. They can request it again by signing in with SSO.`,
      confirm: 'Reject',
      success: 'Request rejected',
    };
  }
  return {
    title: `Remove ${member.login}?`,
    body:
      member.provider === 'sso'
        ? 'They are signed out and their MCP clients lose access. If they sign in with SSO again, they wait for approval again.'
        : 'They are signed out and can no longer use SourceLens. The API keys they created stop working.',
    confirm: 'Remove member',
    success: 'Member removed',
  };
}

function badge(member: Member) {
  if (member.awaitingApproval) return <span className="sl-pending">Awaiting approval</span>;
  if (!member.pending) return null;
  return <span className="sl-pending">{member.provider === 'github' ? 'Invited' : 'Approved, not signed in yet'}</span>;
}

export function SettingsMembers() {
  const user = useUser();
  const session = useSession();
  const owner = user.role === 'owner';
  const members = useApi<Member[]>(owner ? '/api/members' : null);
  const action = useAction();
  const [login, setLogin] = useState('');
  const [formError, setFormError] = useState<string | null>(null);
  const [removing, setRemoving] = useState<Member | null>(null);
  const [roleChange, setRoleChange] = useState<{ member: Member; role: Role } | null>(null);
  const tableRef = useRef<HTMLDivElement>(null);
  const focusAfter = useRef<FocusTarget | null>(null);

  // Runs after every render: once the action settled (its buttons are enabled again), move focus where it was asked for.
  useEffect(() => {
    const target = focusAfter.current;
    if (target === null || action.busy !== null) return;
    focusAfter.current = null;
    const table = tableRef.current;
    const button = target === 'table' ? null : table?.querySelector<HTMLButtonElement>(`[data-member="${target.member}"] .sl-cell-buttons button`);
    (button ?? table)?.focus();
  });

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
        await Promise.all([members.reload(), session.reload()]);
        focusAfter.current = 'table';
      },
      { key: 'remove', success: removeCopy(removing).success },
    );
  }

  function approve(member: Member) {
    void action.run(
      async () => {
        await api.post<Member>(`/api/members/${member.id}/approve`);
        await Promise.all([members.reload(), session.reload()]);
        focusAfter.current = { member: member.id };
      },
      { key: member.id, success: 'Member approved' },
    );
  }

  function changeRole() {
    if (!roleChange) return;
    const { member, role } = roleChange;
    void action.run(
      async () => {
        await api.patch<Member>(`/api/members/${member.id}`, { role });
        setRoleChange(null);
        await members.reload();
        focusAfter.current = { member: member.id };
      },
      { key: member.id, success: role === 'owner' ? `${member.login} is now an admin` : `${member.login} is now a member` },
    );
  }

  function actions(member: Member) {
    if (member.awaitingApproval) {
      return (
        <>
          <button type="button" className="sl-button sl-button-primary sl-button-small" disabled={busy} onClick={() => approve(member)}>
            Approve
          </button>
          <button type="button" className="sl-button sl-button-quiet-danger sl-button-small" disabled={busy} onClick={() => setRemoving(member)}>
            Reject
          </button>
        </>
      );
    }
    if (member.provider === 'sso' && member.role === 'member') {
      return (
        <>
          <button type="button" className="sl-button sl-button-secondary sl-button-small" disabled={busy} onClick={() => setRoleChange({ member, role: 'owner' })}>
            Make admin
          </button>
          <button type="button" className="sl-button sl-button-quiet-danger sl-button-small" disabled={busy} onClick={() => setRemoving(member)}>
            Remove
          </button>
        </>
      );
    }
    if (member.provider === 'sso' && member.id !== user.id) {
      return (
        <button type="button" className="sl-button sl-button-secondary sl-button-small" disabled={busy} onClick={() => setRoleChange({ member, role: 'member' })}>
          Make member
        </button>
      );
    }
    if (member.provider === 'github' && member.role === 'member') {
      return (
        <button type="button" className="sl-button sl-button-quiet-danger sl-button-small" disabled={busy} onClick={() => setRemoving(member)}>
          Remove
        </button>
      );
    }
    return null;
  }

  const removal = removing ? removeCopy(removing) : null;
  const promotion =
    roleChange?.role === 'owner'
      ? { title: `Make ${roleChange.member.login} an admin?`, body: 'Admins manage members, settings and all API keys.', confirm: 'Make admin' }
      : roleChange
        ? { title: `Make ${roleChange.member.login} a member?`, body: 'They can no longer manage members or settings. API keys they created stop working.', confirm: 'Make member' }
        : null;

  return (
    <div className="sl-page sl-page-settings">
      <PageHeader title="Settings" />
      <SettingsTabs owner />
      <p className="sl-prose sl-muted">
        Add GitHub users by username. People who sign in with SSO appear here and wait for approval. SSO members can add and sync repositories and connect agents with OAuth, but cannot see API keys. Admins manage members and settings. Only SSO members can be made admins; GitHub accounts keep their role.
      </p>

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
        <div ref={tableRef} className="sl-table-wrap" tabIndex={0} role="region" aria-label="Members">
          <table className="sl-table">
            <thead className="sl-thead">
              <tr className="sl-tr">
                <th className="sl-th">User</th>
                <th className="sl-th">Sign-in</th>
                <th className="sl-th">Role</th>
                <th className="sl-th">Last sign-in</th>
                <th className="sl-th">
                  <span className="sl-visually-hidden">Actions</span>
                </th>
              </tr>
            </thead>
            <tbody className="sl-tbody">
              {members.data.map((member) => {
                const rowActions = actions(member);
                const detail = member.name ?? (member.email !== member.login ? member.email : null);
                return (
                  <tr key={member.id} className="sl-tr" data-member={member.id}>
                    <td className="sl-td">
                      <span className="sl-member">
                        {member.avatarUrl ? <img className="sl-avatar" src={member.avatarUrl} alt="" width={32} height={32} /> : <span className="sl-avatar" aria-hidden="true" />}
                        <span>
                          <span className="sl-td-main sl-break">{member.login}</span>
                          {detail ? <span className="sl-td-sub sl-break">{detail}</span> : null}
                        </span>
                      </span>
                    </td>
                    <td className="sl-td">
                      <CellLabel>Sign-in</CellLabel>
                      {member.provider === 'sso' ? 'SSO' : 'GitHub'}
                    </td>
                    <td className="sl-td">
                      <CellLabel>Role</CellLabel>
                      <span>
                        {member.role === 'owner' ? 'Admin' : 'Member'}
                        {badge(member)}
                      </span>
                    </td>
                    <td className="sl-td">
                      <CellLabel>Last sign-in</CellLabel>
                      {member.lastLoginAt ? ago(member.lastLoginAt) : 'Never'}
                    </td>
                    <td className="sl-td sl-td-actions">{rowActions ? <div className="sl-cell-buttons">{rowActions}</div> : null}</td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      ) : null}

      <ConfirmDialog
        open={removal !== null}
        title={removal?.title ?? ''}
        body={removal?.body ?? ''}
        confirmLabel={removal?.confirm ?? ''}
        busy={busy}
        onConfirm={remove}
        onClose={() => setRemoving(null)}
      />

      <Dialog
        open={promotion !== null}
        title={promotion?.title ?? ''}
        onClose={() => setRoleChange(null)}
        actions={
          <>
            <button type="button" className="sl-button sl-button-primary" disabled={busy} onClick={changeRole}>
              {promotion?.confirm}
            </button>
            <button type="button" className="sl-button sl-button-secondary" onClick={() => setRoleChange(null)} data-autofocus>
              Cancel
            </button>
          </>
        }
      >
        <p className="sl-prose">{promotion?.body}</p>
      </Dialog>
    </div>
  );
}

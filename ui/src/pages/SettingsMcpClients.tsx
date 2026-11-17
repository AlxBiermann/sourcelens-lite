import { useRef, useState } from 'react';
import { api, type OAuthGrant } from '../api';
import { Snippet } from '../components/CodeBlock';
import { CopyButton } from '../components/CopyButton';
import { ConfirmDialog } from '../components/Dialog';
import { LoadState } from '../components/LoadState';
import { CellLabel, PageHeader, SettingsTabs } from '../components/Shell';
import { useAction } from '../hooks/useAction';
import { useApi } from '../hooks/useApi';
import { useSession, useUser } from '../hooks/useSession';
import { ago, date } from '../format';

export function SettingsMcpClients() {
  const user = useUser();
  const { state } = useSession();
  const grants = useApi<OAuthGrant[]>('/api/oauth/grants');
  const action = useAction();
  const [revoking, setRevoking] = useState<OAuthGrant | null>(null);
  const urlRef = useRef<HTMLElement>(null);
  const commandRef = useRef<HTMLPreElement>(null);
  const mcpUrl = state ? `${state.publicUrl}/mcp` : null;
  const command = mcpUrl ? `claude mcp add --transport http sourcelens ${mcpUrl}` : null;

  function revoke() {
    if (!revoking) return;
    const id = revoking.id;
    void action.run(
      async () => {
        await api.del(`/api/oauth/grants/${id}`);
        setRevoking(null);
        await grants.reload();
      },
      { success: 'Client revoked' },
    );
  }

  return (
    <div className="sl-page sl-page-settings">
      <PageHeader title="Settings" />
      <SettingsTabs owner={user.role === 'owner'} />
      {mcpUrl ? (
        <dl className="sl-settings-list">
          <div className="sl-settings-item">
            <dt className="sl-settings-label">MCP URL</dt>
            <dd className="sl-settings-value sl-settings-copy">
              <code className="sl-mono sl-break" ref={urlRef}>
                {mcpUrl}
              </code>
              <CopyButton text={mcpUrl} target={urlRef} what="URL" />
            </dd>
          </div>
        </dl>
      ) : null}
      <p className="sl-prose sl-muted">Agents that support OAuth connect with this URL alone: they open the browser, you sign in and allow the client.</p>
      <LoadState state={grants} />
      {grants.data && grants.data.length === 0 ? (
        <div className="sl-empty">
          <p className="sl-empty-text">No MCP client is connected yet. Add the MCP URL above to your agent; it opens this browser to sign in.</p>
          {command ? (
            <div className="sl-stack-tight">
              <div className="sl-snippet-head">
                <p className="sl-hint">Claude Code: run in a terminal.</p>
                <CopyButton text={command} target={commandRef} what="command" />
              </div>
              <Snippet text={command} ref={commandRef} />
            </div>
          ) : null}
        </div>
      ) : null}
      {grants.data && grants.data.length > 0 ? (
        <div className="sl-table-wrap" tabIndex={0} role="region" aria-label="MCP clients">
          <table className="sl-table">
            <thead className="sl-thead">
              <tr className="sl-tr">
                <th className="sl-th">Client</th>
                <th className="sl-th">User</th>
                <th className="sl-th">Connected</th>
                <th className="sl-th">Last used</th>
                <th className="sl-th">
                  <span className="sl-visually-hidden">Actions</span>
                </th>
              </tr>
            </thead>
            <tbody className="sl-tbody">
              {grants.data.map((grant) => (
                <tr key={grant.id} className="sl-tr">
                  <td className="sl-td">
                    <span className="sl-td-main">{grant.clientName}</span>
                  </td>
                  <td className="sl-td">
                    <CellLabel>User</CellLabel>
                    {grant.user}
                  </td>
                  <td className="sl-td">
                    <CellLabel>Connected</CellLabel>
                    {date(grant.createdAt)}
                  </td>
                  <td className="sl-td">
                    <CellLabel>Last used</CellLabel>
                    {grant.lastUsedAt ? ago(grant.lastUsedAt) : 'Never'}
                  </td>
                  <td className="sl-td sl-td-actions">
                    <div className="sl-cell-buttons">
                      <button type="button" className="sl-button sl-button-quiet-danger sl-button-small" onClick={() => setRevoking(grant)}>
                        Revoke
                      </button>
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      ) : null}

      <ConfirmDialog
        open={revoking !== null}
        title={revoking && revoking.user !== user.login ? `Revoke ${revoking.clientName} for ${revoking.user}?` : `Revoke ${revoking?.clientName ?? ''}?`}
        body="The client loses access immediately. It can connect again once someone allows it."
        confirmLabel="Revoke client"
        busy={action.busy !== null}
        onConfirm={revoke}
        onClose={() => setRevoking(null)}
      />
    </div>
  );
}

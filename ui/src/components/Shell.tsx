import { useState, type ReactNode } from 'react';
import { NavLink, Outlet } from 'react-router';
import { api } from '../api';
import { useUser } from '../hooks/useSession';
import { ThemeToggle } from './ThemeToggle';

const NAV = [
  { to: '/repositories', label: 'Repositories' },
  { to: '/playground', label: 'Playground' },
  { to: '/keys', label: 'API keys' },
  { to: '/settings', label: 'Settings' },
];

const tabClass = ({ isActive }: { isActive: boolean }) => (isActive ? 'sl-tab sl-tab-active' : 'sl-tab');

export function Shell() {
  const user = useUser();
  const [signingOut, setSigningOut] = useState(false);

  async function signOut() {
    setSigningOut(true);
    try {
      await api.post('/api/logout');
    } finally {
      window.location.assign('/login');
    }
  }

  return (
    <div className="sl-shell">
      <a className="sl-skip" href="#main">
        Skip to content
      </a>
      <header className="sl-rail">
        <NavLink className="sl-wordmark" to="/repositories">
          SourceLens
        </NavLink>
        <nav className="sl-nav" aria-label="Main">
          {NAV.map((item) => (
            <NavLink key={item.to} to={item.to} className={({ isActive }) => (isActive ? 'sl-nav-link sl-nav-link-active' : 'sl-nav-link')}>
              {item.label}
            </NavLink>
          ))}
        </nav>
        <div className="sl-rail-footer">
          <ThemeToggle />
          <div className="sl-rail-user">
            <span className="sl-rail-login">@{user.login}</span>
            <button type="button" className="sl-rail-signout" disabled={signingOut} onClick={() => void signOut()}>
              Sign out
            </button>
          </div>
        </div>
      </header>
      <main className="sl-main" id="main">
        <Outlet />
      </main>
    </div>
  );
}

export function PageHeader({ title, action, mono }: { title: string; action?: ReactNode; mono?: boolean }) {
  return (
    <div className="sl-page-header">
      <h1 className={mono ? 'sl-title sl-title-long' : 'sl-title'} title={mono ? title : undefined}>
        {title}
      </h1>
      {action ? <div className="sl-page-action">{action}</div> : null}
    </div>
  );
}

export function SettingsTabs({ owner }: { owner: boolean }) {
  return (
    <nav className="sl-tabs" aria-label="Settings">
      <NavLink to="/settings/embeddings" className={tabClass}>
        Embeddings
      </NavLink>
      <NavLink to="/settings/github" className={tabClass}>
        GitHub
      </NavLink>
      {owner ? (
        <NavLink to="/settings/members" className={tabClass}>
          Members
        </NavLink>
      ) : null}
    </nav>
  );
}

export function Notice({ tone = 'warning', children }: { tone?: 'warning' | 'error'; children: ReactNode }) {
  return (
    <div className={tone === 'error' ? 'sl-notice sl-notice-error' : 'sl-notice'} role={tone === 'error' ? 'alert' : undefined}>
      {children}
    </div>
  );
}

export function Loading() {
  return <p className="sl-muted">Loading…</p>;
}

/** Label for a table cell; shown only where tables become stacked blocks on narrow screens. */
export function CellLabel({ children }: { children: string }) {
  return (
    <span className="sl-td-label" aria-hidden="true">
      {children}
    </span>
  );
}

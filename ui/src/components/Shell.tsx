import { useEffect, useRef, useState, type ReactNode } from 'react';
import { NavLink, Outlet, useLocation } from 'react-router';
import { api, canManageApiKeys, handle } from '../api';
import { useDocumentTitle } from '../hooks/useDocumentTitle';
import { useSession, useUser } from '../hooks/useSession';
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
  const devMode = useSession().state?.devMode ?? false;
  const [signingOut, setSigningOut] = useState(false);
  const { pathname } = useLocation();
  const main = useRef<HTMLElement>(null);
  const shownPath = useRef(pathname);

  // After navigating, start screen readers and keyboard focus on the new page instead of the clicked link.
  useEffect(() => {
    if (shownPath.current === pathname) return;
    shownPath.current = pathname;
    main.current?.focus();
  }, [pathname]);

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
          {NAV.filter((item) => item.to !== '/keys' || canManageApiKeys(user)).map((item) => (
            <NavLink key={item.to} to={item.to} className={({ isActive }) => (isActive ? 'sl-nav-link sl-nav-link-active' : 'sl-nav-link')}>
              {item.label}
            </NavLink>
          ))}
        </nav>
        <div className="sl-rail-footer">
          <ThemeToggle />
          <div className="sl-rail-user">
            {devMode ? <span className="sl-pending">Dev mode</span> : null}
            <span className="sl-rail-login">{handle(user)}</span>
            <button type="button" className="sl-rail-signout" disabled={signingOut} onClick={() => void signOut()}>
              Sign out
            </button>
          </div>
        </div>
      </header>
      <main className="sl-main" id="main" ref={main} tabIndex={-1}>
        <Outlet />
      </main>
    </div>
  );
}

export function PageHeader({ title, action, mono }: { title: string; action?: ReactNode; mono?: boolean }) {
  useDocumentTitle(title);
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
  const { pendingApprovals } = useUser();
  const { pathname } = useLocation();
  const ref = useRef<HTMLElement>(null);
  const [more, setMore] = useState(false);

  // Centred, so the active tab never sits under the fade at the right edge.
  useEffect(() => {
    ref.current?.querySelector('.sl-tab-active')?.scrollIntoView({ inline: 'center', block: 'nearest' });
  }, [pathname]);

  // `more`: tabs continue past the right edge; on phones the row then fades out there (see `.sl-tabs-more`).
  useEffect(() => {
    const nav = ref.current;
    if (!nav) return;
    const update = () => setMore(nav.scrollLeft + nav.clientWidth < nav.scrollWidth - 1);
    update();
    nav.addEventListener('scroll', update, { passive: true });
    const observer = new ResizeObserver(update);
    observer.observe(nav);
    return () => {
      nav.removeEventListener('scroll', update);
      observer.disconnect();
    };
  }, [owner, pendingApprovals]);

  return (
    <nav ref={ref} className={more ? 'sl-tabs sl-tabs-more' : 'sl-tabs'} aria-label="Settings">
      <NavLink to="/settings/embeddings" className={tabClass}>
        Embeddings
      </NavLink>
      <NavLink to="/settings/github" className={tabClass}>
        GitHub
      </NavLink>
      <NavLink to="/settings/sync" className={tabClass}>
        Sync
      </NavLink>
      <NavLink to="/settings/mcp-clients" className={tabClass}>
        MCP clients
      </NavLink>
      {owner ? (
        <>
          <NavLink to="/settings/members" className={tabClass}>
            {pendingApprovals > 0 ? `Members (${pendingApprovals})` : 'Members'}
          </NavLink>
          <NavLink to="/settings/sso" className={tabClass}>
            Sign-in
          </NavLink>
        </>
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

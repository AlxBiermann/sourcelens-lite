import type { ReactNode } from 'react';
import { ThemeToggle } from './ThemeToggle';

/** Layout of the pages shown before sign-in: theme toggle, wordmark, optional lead sentence, content. */
export function EntryPage({ lead, children }: { lead?: string; children: ReactNode }) {
  return (
    <div className="sl-entry-page">
      <div className="sl-entry-top">
        <ThemeToggle />
      </div>
      <main className="sl-entry-content">
        <h1 className="sl-entry-wordmark">SourceLens</h1>
        {lead ? <p className="sl-entry-lead">{lead}</p> : null}
        {children}
      </main>
    </div>
  );
}

import { useState, type FormEvent } from 'react';
import { api, errorMessage, type Repository, type SearchHit, type SearchResult } from '../api';
import { CodeBlock } from '../components/CodeBlock';
import { Field } from '../components/Field';
import { Notice, PageHeader } from '../components/Shell';
import { useApi } from '../hooks/useApi';
import { plural } from '../format';

const LANGUAGES = ['c', 'cpp', 'csharp', 'go', 'java', 'javascript', 'kotlin', 'php', 'python', 'ruby', 'rust', 'scala', 'typescript', 'markdown'];

function Tags({ hit }: { hit: SearchHit }) {
  const symbol = hit.matchedBy.includes('symbol');
  const text = hit.matchedBy.includes('text_all') || hit.matchedBy.includes('text_any');
  const semantic = hit.matchedBy.includes('semantic');
  return (
    <span className="sl-tags">
      {symbol ? <span className="sl-tag">symbol</span> : null}
      {text ? <span className="sl-tag">text</span> : null}
      {semantic ? <span className="sl-tag sl-tag-semantic">semantic</span> : null}
    </span>
  );
}

export function Playground() {
  const repos = useApi<Repository[]>('/api/repositories');
  const [query, setQuery] = useState('');
  const [repoId, setRepoId] = useState('');
  const [language, setLanguage] = useState('');
  const [pathPrefix, setPathPrefix] = useState('');
  const [result, setResult] = useState<SearchResult | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const searchable = (repos.data ?? []).filter((repo) => repo.state === 'connected' && repo.indexedCommit !== null);

  async function search(event: FormEvent) {
    event.preventDefault();
    if (!query.trim()) return;
    setBusy(true);
    setError(null);
    try {
      setResult(
        await api.post<SearchResult>('/api/search', {
          query: query.trim(),
          repositoryIds: repoId ? [Number(repoId)] : undefined,
          languages: language ? [language] : undefined,
          pathPrefix: pathPrefix.trim() || undefined,
        }),
      );
    } catch (e) {
      setResult(null);
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="sl-page">
      <PageHeader title="Playground" />
      <form className="sl-search" role="search" onSubmit={(e) => void search(e)}>
        <div className="sl-search-bar">
          <label className="sl-visually-hidden" htmlFor="sl-query">
            Search query
          </label>
          <input
            id="sl-query"
            className="sl-search-input"
            type="search"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder="A name, keywords or a question"
            autoComplete="off"
            spellCheck={false}
          />
          <button type="submit" className="sl-button sl-button-primary sl-search-button" disabled={busy || !query.trim()}>
            Search
          </button>
        </div>
        <div className="sl-filters">
          <Field label="Repository">
            {(props) => (
              <select {...props} className="sl-select" value={repoId} onChange={(e) => setRepoId(e.target.value)}>
                <option value="">All repositories</option>
                {searchable.map((repo) => (
                  <option key={repo.id} value={repo.id}>
                    {repo.fullName}
                  </option>
                ))}
              </select>
            )}
          </Field>
          <Field label="Language">
            {(props) => (
              <select {...props} className="sl-select" value={language} onChange={(e) => setLanguage(e.target.value)}>
                <option value="">Any language</option>
                {LANGUAGES.map((name) => (
                  <option key={name} value={name}>
                    {name}
                  </option>
                ))}
              </select>
            )}
          </Field>
          <Field label="Path prefix">
            {(props) => <input {...props} className="sl-input sl-input-mono" value={pathPrefix} onChange={(e) => setPathPrefix(e.target.value)} placeholder="src/" autoComplete="off" spellCheck={false} />}
          </Field>
        </div>
      </form>

      {error ? <Notice tone="error">{error}</Notice> : null}

      {result === null && !error ? <p className="sl-muted">Search across {plural(searchable.length, 'indexed repository', 'indexed repositories')}.</p> : null}

      {result ? (
        <section className="sl-results" aria-label="Search results">
          <p className="sl-results-summary" aria-live="polite">
            {plural(result.results.length, 'result')}, {result.mode} search
          </p>
          {result.results.map((hit, index) => {
            const semanticOnly = hit.matchedBy.length > 0 && hit.matchedBy.every((channel) => channel === 'semantic');
            return (
              <article key={`${hit.repository}:${hit.path}:${hit.startLine}:${index}`} className="sl-hit">
                <header className="sl-hit-head">
                  <span className="sl-hit-location">
                    {hit.repository} {hit.path}:{hit.startLine}-{hit.endLine}
                  </span>
                  <span className="sl-hit-symbol">
                    {hit.kind}
                    {hit.symbolPath ? ` ${hit.symbolPath}` : ''}
                  </span>
                  <Tags hit={hit} />
                </header>
                <CodeBlock code={hit.content} startLine={hit.startLine} terms={semanticOnly ? [] : result.terms} semanticOnly={semanticOnly} label={`${hit.path} lines ${hit.startLine} to ${hit.endLine}`} />
              </article>
            );
          })}
        </section>
      ) : null}
    </div>
  );
}

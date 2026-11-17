import { useEffect, useRef, useState, type FormEvent } from 'react';
import { Link, useSearchParams } from 'react-router';
import { api, errorMessage, type Repository, type SearchHit, type SearchLanguages, type SearchResult, type SemanticUnavailable } from '../api';
import { CodeBlock } from '../components/CodeBlock';
import { Field } from '../components/Field';
import { LoadState } from '../components/LoadState';
import { Notice, PageHeader } from '../components/Shell';
import { useEmbeddingsOn } from '../components/StatusBlock';
import { useApi } from '../hooks/useApi';
import { plural } from '../format';

const SEMANTIC_REASON: Record<SemanticUnavailable, string> = {
  timeout: 'timed out',
  provider_error: 'the embeddings provider failed',
  internal: 'internal error',
};

/** Chunks without a symbol path: show the symbols that start inside them instead. */
const NAMED_KINDS: Record<string, true> = { block: true, block_part: true };

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

function symbolLabel(hit: SearchHit): string {
  const detail = NAMED_KINDS[hit.kind] && hit.names.length > 0 ? hit.names.join(', ') : hit.symbolPath;
  return detail ? `${hit.kind} ${detail}` : hit.kind;
}

/** The hit's lines on GitHub at the indexed commit, so the link shows exactly what was searched. */
function githubUrl(repo: Repository | undefined, hit: SearchHit): string | null {
  if (!repo?.indexedCommit) return null;
  const path = hit.path.split('/').map(encodeURIComponent).join('/');
  return `https://github.com/${repo.fullName}/blob/${repo.indexedCommit}/${path}#L${hit.startLine}-L${hit.endLine}`;
}

/** Outcome of the search identified by `key`. */
type Settled = { key: string; result: SearchResult | null; error: string | null };

export function Playground() {
  const repos = useApi<Repository[]>('/api/repositories');
  const languages = useApi<SearchLanguages>('/api/search/languages');
  const embeddingsOn = useEmbeddingsOn();
  const [params, setParams] = useSearchParams();
  // The URL holds the executed search, so reload, links and back/forward show it again.
  const q = params.get('q')?.trim() ?? '';
  const repoParam = params.get('repo') ?? '';
  const langParam = params.get('lang') ?? '';
  const pathParam = params.get('path') ?? '';

  const [query, setQuery] = useState(q);
  const [repoId, setRepoId] = useState(repoParam);
  const [language, setLanguage] = useState(langParam);
  const [pathPrefix, setPathPrefix] = useState(pathParam);
  // The inputs follow the URL when it changes (back/forward, submit).
  const urlKey = JSON.stringify([q, repoParam, langParam, pathParam]);
  const [inputsFor, setInputsFor] = useState(urlKey);
  if (inputsFor !== urlKey) {
    setInputsFor(urlKey);
    setQuery(q);
    setRepoId(repoParam);
    setLanguage(langParam);
    setPathPrefix(pathParam);
  }

  // `run` changes on every submit, so submitting an unchanged search searches again.
  const [run, setRun] = useState(0);
  const searchKey = JSON.stringify([q, repoParam, langParam, pathParam, run]);
  const [settled, setSettled] = useState<Settled | null>(null);
  const requestCount = useRef(0);

  useEffect(() => {
    // Responses of superseded requests are dropped; only the latest one settles.
    const current = ++requestCount.current;
    if (!q) return;
    void api
      .post<SearchResult>('/api/search', {
        query: q,
        repositoryIds: repoParam ? [Number(repoParam)] : undefined,
        languages: langParam ? [langParam] : undefined,
        pathPrefix: pathParam.trim() || undefined,
      })
      .then(
        (result) => {
          if (current === requestCount.current) setSettled({ key: searchKey, result, error: null });
        },
        (e: unknown) => {
          if (current === requestCount.current) setSettled({ key: searchKey, result: null, error: errorMessage(e) });
        },
      );
  }, [searchKey, q, repoParam, langParam, pathParam]);

  const busy = q !== '' && settled?.key !== searchKey;
  const shown = q !== '' ? settled : null;
  const result = shown?.result ?? null;
  const error = busy ? null : (shown?.error ?? null);

  const searchable = (repos.data ?? []).filter((repo) => repo.state === 'connected' && repo.indexedCommit !== null);
  const byName = new Map((repos.data ?? []).map((repo): [string, Repository] => [repo.fullName, repo]));

  function submit(event: FormEvent) {
    event.preventDefault();
    const trimmed = query.trim();
    if (!trimmed) return;
    const next = new URLSearchParams({ q: trimmed });
    if (repoId) next.set('repo', repoId);
    if (language) next.set('lang', language);
    if (pathPrefix.trim()) next.set('path', pathPrefix.trim());
    // Pushing the unchanged URL would only add a duplicate history entry.
    if (next.toString() !== params.toString()) setParams(next);
    setRun((n) => n + 1);
  }

  // One permanent live region, so the first result count is announced too. Errors are announced by their notice.
  const status = busy ? 'Searching…' : result ? `${plural(result.results.length, 'result')}, ${result.mode} search` : '';

  return (
    <div className="sl-page">
      <PageHeader title="Playground" />
      <form className="sl-search" role="search" onSubmit={submit}>
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
            placeholder="Name, keywords or question"
            autoComplete="off"
            spellCheck={false}
          />
          <button type="submit" className="sl-button sl-button-primary sl-search-button" disabled={!query.trim()}>
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
                {(languages.data?.languages ?? []).map((name) => (
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

      {repos.error ? <LoadState state={repos} /> : null}
      {languages.error ? <LoadState state={languages} /> : null}
      {error ? <Notice tone="error">{error}</Notice> : null}

      <p className={result ? 'sl-results-summary' : 'sl-visually-hidden'} role="status">
        {status}
      </p>

      {result === null && !error && repos.data ? <p className="sl-muted">Search across {plural(searchable.length, 'indexed repository', 'indexed repositories')}.</p> : null}

      {result?.mode === 'lexical' && result.semanticUnavailable ? (
        <Notice>Search by meaning was not available for this search ({SEMANTIC_REASON[result.semanticUnavailable]}). These are keyword results.</Notice>
      ) : null}
      {result?.mode === 'lexical' && !result.semanticUnavailable && !embeddingsOn ? (
        <p className="sl-muted">
          Keyword search only. Search by meaning needs embeddings:{' '}
          <Link className="sl-link" to="/settings/embeddings">
            Settings → Embeddings
          </Link>
          .
        </p>
      ) : null}

      {result && result.results.length === 0 ? <p className="sl-empty-text">No matches. Try fewer words or clear the filters.</p> : null}

      {result && result.results.length > 0 ? (
        <section className="sl-results" aria-label="Search results">
          {result.results.map((hit, index) => {
            const semanticOnly = hit.matchedBy.length > 0 && hit.matchedBy.every((channel) => channel === 'semantic');
            const repo = byName.get(hit.repository);
            const location = `${hit.path}:${hit.startLine}-${hit.endLine}`;
            const url = githubUrl(repo, hit);
            return (
              <article key={`${hit.repository}:${hit.path}:${hit.startLine}:${index}`} className="sl-hit">
                <header className="sl-hit-head">
                  <Tags hit={hit} />
                  <span className="sl-hit-repo">
                    {repo ? (
                      <Link className="sl-link" to={`/repositories/${repo.id}`}>
                        {hit.repository}
                      </Link>
                    ) : (
                      hit.repository
                    )}
                  </span>
                  <span className="sl-hit-location">
                    {url ? (
                      <a className="sl-link" href={url} target="_blank" rel="noreferrer">
                        {location}
                      </a>
                    ) : (
                      location
                    )}
                  </span>
                  <span className="sl-hit-symbol">{symbolLabel(hit)}</span>
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

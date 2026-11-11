import { Fragment, useMemo, type ReactNode, type Ref } from 'react';

function escapeRegExp(text: string): string {
  return text.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

function highlighter(terms: string[]): RegExp | null {
  const usable = [...new Set(terms.filter((term) => term.length >= 3).map((term) => term.toLowerCase()))].sort((a, b) => b.length - a.length);
  if (usable.length === 0) return null;
  return new RegExp(`(${usable.map(escapeRegExp).join('|')})`, 'gi');
}

function markLine(line: string, pattern: RegExp | null): ReactNode {
  if (!pattern || line === '') return line;
  const parts = line.split(pattern);
  if (parts.length === 1) return line;
  // `split` with one capture group alternates plain text and matches.
  return parts.map((part, index) =>
    index % 2 === 1 ? (
      <mark key={index} className="sl-mark">
        {part}
      </mark>
    ) : (
      part
    ),
  );
}

export function CodeBlock({ code, startLine = 1, terms = [], semanticOnly = false, label }: { code: string; startLine?: number; terms?: string[]; semanticOnly?: boolean; label?: string }) {
  const pattern = useMemo(() => highlighter(terms), [terms]);
  const lines = useMemo(() => code.replace(/\n$/, '').split('\n'), [code]);
  const width = String(startLine + lines.length - 1).length;

  return (
    <pre className={semanticOnly ? 'sl-code sl-code-semantic' : 'sl-code'} aria-label={label} tabIndex={0}>
      <code className="sl-code-inner">
        {lines.map((line, index) => (
          <Fragment key={index}>
            <span className="sl-code-number" aria-hidden="true">
              {`${String(startLine + index).padStart(width, ' ')}  `}
            </span>
            {markLine(line, pattern)}
            {'\n'}
          </Fragment>
        ))}
      </code>
    </pre>
  );
}

export function Snippet({ text, ref }: { text: string; ref?: Ref<HTMLPreElement> }) {
  return (
    <pre className="sl-code" tabIndex={0} ref={ref}>
      <code className="sl-snippet">{text}</code>
    </pre>
  );
}

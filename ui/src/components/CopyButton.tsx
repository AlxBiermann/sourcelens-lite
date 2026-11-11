import { useEffect, useRef, useState, type RefObject } from 'react';

type CopyState = 'idle' | 'copied' | 'failed';

async function writeClipboard(text: string, button: HTMLElement): Promise<boolean> {
  if (navigator.clipboard) {
    try {
      await navigator.clipboard.writeText(text);
      return true;
    } catch {
      // Denied or unavailable; try the legacy path below.
    }
  }
  // Outside an open modal dialog everything is inert, so the helper must live inside it.
  const host = button.closest('dialog') ?? document.body;
  const area = document.createElement('textarea');
  area.value = text;
  area.setAttribute('readonly', '');
  area.className = 'sl-copy-buffer';
  host.append(area);
  area.select();
  let ok = false;
  try {
    ok = document.execCommand('copy');
  } catch {
    ok = false;
  }
  area.remove();
  button.focus();
  return ok;
}

export function CopyButton({
  text,
  label = 'Copy',
  className = 'sl-button sl-button-secondary sl-button-small',
  target,
  what = 'text',
  autoFocus,
  describedBy,
}: {
  text: string;
  label?: string;
  className?: string;
  /** Element holding the text; it is selected when copying fails so the user can copy it by hand. */
  target?: RefObject<HTMLElement | null>;
  /** Noun for the failure message, e.g. "key". */
  what?: string;
  autoFocus?: boolean;
  describedBy?: string;
}) {
  const [state, setState] = useState<CopyState>('idle');
  const button = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    if (state !== 'copied') return;
    const timer = window.setTimeout(() => setState('idle'), 2000);
    return () => window.clearTimeout(timer);
  }, [state]);

  useEffect(() => {
    if (autoFocus) button.current?.focus();
  }, [autoFocus]);

  async function copy() {
    if (!button.current) return;
    const ok = await writeClipboard(text, button.current);
    setState(ok ? 'copied' : 'failed');
    const el = target?.current;
    if (!ok && el) window.getSelection()?.selectAllChildren(el);
  }

  return (
    <>
      <button ref={button} type="button" className={className} onClick={() => void copy()} aria-describedby={describedBy}>
        <span aria-live="polite">{state === 'copied' ? 'Copied' : label}</span>
      </button>
      {state === 'failed' ? (
        <p className="sl-field-error" role="alert">
          Copy failed. Select the {what} and copy it manually.
        </p>
      ) : null}
    </>
  );
}

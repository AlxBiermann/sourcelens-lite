import { useEffect, useState } from 'react';

export function CopyButton({ text, label = 'Copy', className = 'sl-button sl-button-secondary sl-button-small' }: { text: string; label?: string; className?: string }) {
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    if (!copied) return;
    const timer = window.setTimeout(() => setCopied(false), 2000);
    return () => window.clearTimeout(timer);
  }, [copied]);

  async function copy() {
    try {
      await navigator.clipboard.writeText(text);
    } catch {
      const area = document.createElement('textarea');
      area.value = text;
      document.body.append(area);
      area.select();
      document.execCommand('copy');
      area.remove();
    }
    setCopied(true);
  }

  return (
    <button type="button" className={className} onClick={() => void copy()} aria-live="polite">
      {copied ? 'Copied' : label}
    </button>
  );
}

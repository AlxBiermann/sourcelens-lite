import { useId, type ReactNode } from 'react';

type ControlProps = { id: string; 'aria-describedby'?: string; 'aria-invalid'?: boolean };

export function Field({ label, hint, error, children }: { label: string; hint?: ReactNode; error?: string | null; children: (props: ControlProps) => ReactNode }) {
  const id = useId();
  const hintId = `${id}-hint`;
  const errorId = `${id}-error`;
  const describedBy = [hint ? hintId : null, error ? errorId : null].filter(Boolean).join(' ') || undefined;

  return (
    <div className="sl-field">
      <label className="sl-label" htmlFor={id}>
        {label}
      </label>
      {children({ id, 'aria-describedby': describedBy, 'aria-invalid': error ? true : undefined })}
      {hint ? (
        <p className="sl-hint" id={hintId}>
          {hint}
        </p>
      ) : null}
      {error ? (
        <p className="sl-field-error" id={errorId}>
          {error}
        </p>
      ) : null}
    </div>
  );
}

export function Choice({ type, name, checked, onChange, label, disabled }: { type: 'radio' | 'checkbox'; name?: string; checked: boolean; onChange: (checked: boolean) => void; label: ReactNode; disabled?: boolean }) {
  return (
    <label className="sl-choice">
      <input type={type} name={name} className="sl-choice-input" checked={checked} disabled={disabled} onChange={(e) => onChange(e.target.checked)} />
      <span className="sl-choice-label">{label}</span>
    </label>
  );
}

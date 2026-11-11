import { useEffect, useId, useRef, type ReactNode } from 'react';

export function Dialog({ open, title, onClose, wide, children, actions }: { open: boolean; title: string; onClose: () => void; wide?: boolean; children: ReactNode; actions?: ReactNode }) {
  const ref = useRef<HTMLDialogElement>(null);
  const titleId = useId();

  useEffect(() => {
    const dialog = ref.current;
    if (!dialog) return;
    if (open && !dialog.open) dialog.showModal();
    if (!open && dialog.open) dialog.close();
  }, [open]);

  return (
    <dialog
      ref={ref}
      className={wide ? 'sl-dialog sl-dialog-wide' : 'sl-dialog'}
      aria-labelledby={titleId}
      onCancel={(event) => {
        event.preventDefault();
        onClose();
      }}
    >
      {open ? (
        <div className="sl-dialog-body">
          <h2 className="sl-dialog-title" id={titleId}>
            {title}
          </h2>
          <div className="sl-dialog-content">{children}</div>
          {actions ? <div className="sl-dialog-actions">{actions}</div> : null}
        </div>
      ) : null}
    </dialog>
  );
}

export function ConfirmDialog({ open, title, body, confirmLabel, onConfirm, onClose, busy }: { open: boolean; title: string; body: string; confirmLabel: string; onConfirm: () => void; onClose: () => void; busy?: boolean }) {
  return (
    <Dialog
      open={open}
      title={title}
      onClose={onClose}
      actions={
        <>
          <button type="button" className="sl-button sl-button-danger" disabled={busy} onClick={onConfirm}>
            {confirmLabel}
          </button>
          <button type="button" className="sl-button sl-button-secondary" onClick={onClose}>
            Cancel
          </button>
        </>
      }
    >
      <p className="sl-prose">{body}</p>
    </Dialog>
  );
}

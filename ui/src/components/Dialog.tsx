import { useEffect, useId, useRef, type ReactNode } from 'react';

/**
 * Fired on `window` with the dialog element as `detail` when a modal dialog opens or closes. Everything outside an
 * open modal dialog is inert, so the toast region moves into the dialog while it is open.
 */
export const DIALOG_OPENED_EVENT = 'sl-dialog-opened';
export const DIALOG_CLOSED_EVENT = 'sl-dialog-closed';

type DialogProps = {
  open: boolean;
  title: string;
  onClose: () => void;
  wide?: boolean;
  /** False: Escape does not close the dialog; only its own buttons do. */
  dismissible?: boolean;
  /** Id for the title element, for controls that refer to it. */
  titleId?: string;
  children: ReactNode;
  actions?: ReactNode;
};

/** Opens the dialog and focuses the first element marked `data-autofocus` (else the browser's choice). */
function openModal(dialog: HTMLDialogElement) {
  dialog.showModal();
  dialog.querySelector<HTMLElement>('[data-autofocus]')?.focus();
  window.dispatchEvent(new CustomEvent(DIALOG_OPENED_EVENT, { detail: dialog }));
}

export function Dialog({ open, title, onClose, wide, dismissible = true, titleId, children, actions }: DialogProps) {
  const ref = useRef<HTMLDialogElement>(null);
  const generatedId = useId();
  const headingId = titleId ?? generatedId;

  useEffect(() => {
    const dialog = ref.current;
    if (!dialog) return;
    if (open && !dialog.open) openModal(dialog);
    if (!open && dialog.open) dialog.close();
  }, [open]);

  return (
    <dialog
      ref={ref}
      className={wide ? 'sl-dialog sl-dialog-wide' : 'sl-dialog'}
      aria-labelledby={headingId}
      onCancel={(event) => {
        if (!dismissible) event.preventDefault();
      }}
      // Every native close (Escape, a non-cancelable second Escape, form method=dialog) reports back here,
      // so React state and the element cannot drift apart.
      onClose={() => {
        const dialog = ref.current;
        window.dispatchEvent(new CustomEvent(DIALOG_CLOSED_EVENT, { detail: dialog }));
        if (!open || !dialog) return;
        if (!dismissible) {
          openModal(dialog);
          return;
        }
        onClose();
      }}
    >
      {open ? (
        <div className="sl-dialog-body">
          <h2 className="sl-dialog-title" id={headingId}>
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
          <button type="button" className="sl-button sl-button-secondary" onClick={onClose} data-autofocus>
            Cancel
          </button>
        </>
      }
    >
      <p className="sl-prose">{body}</p>
    </Dialog>
  );
}

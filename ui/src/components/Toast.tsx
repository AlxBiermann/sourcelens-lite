import { createContext, useCallback, useContext, useEffect, useRef, useState, type ReactNode } from 'react';
import { createPortal } from 'react-dom';
import { DIALOG_CLOSED_EVENT, DIALOG_OPENED_EVENT } from './Dialog';

type Tone = 'info' | 'error';
type ToastItem = { id: number; message: string; tone: Tone };
type Notify = (message: string, tone?: Tone) => void;

const INFO_MS = 4000;

const ToastContext = createContext<Notify | null>(null);

export function ToastProvider({ children }: { children: ReactNode }) {
  const [items, setItems] = useState<ToastItem[]>([]);
  const [paused, setPaused] = useState(false);
  const nextId = useRef(1);
  const region = useRef<HTMLDivElement>(null);
  // An open modal dialog makes everything outside it inert, so the region is rendered inside that dialog.
  const [host, setHost] = useState<HTMLElement | null>(null);

  const dismiss = useCallback((id: number) => {
    setItems((current) => current.filter((item) => item.id !== id));
  }, []);

  const notify = useCallback<Notify>((message, tone = 'info') => {
    const id = nextId.current++;
    setItems((current) => [...current.slice(-2), { id, message, tone }]);
  }, []);

  // Info toasts close themselves unless the pointer or focus is on the region; error toasts stay until dismissed.
  useEffect(() => {
    if (paused) return;
    const timers = items.filter((item) => item.tone === 'info').map((item) => window.setTimeout(() => dismiss(item.id), INFO_MS));
    return () => timers.forEach((timer) => window.clearTimeout(timer));
  }, [items, paused, dismiss]);

  // The region is a manual popover so it lives in the top layer, above the page and the dialog that hosts it.
  // Re-showing it moves it to the top again; skipped while focus is inside, because hiding would drop that focus.
  const raise = useCallback(() => {
    const el = region.current;
    if (!el || el.contains(document.activeElement)) return;
    if (el.matches(':popover-open')) el.hidePopover();
    el.showPopover();
  }, []);

  const newest = items.length > 0 ? items[items.length - 1].id : 0;
  useEffect(() => {
    raise();
  }, [newest, host, raise]);

  useEffect(() => {
    const opened = (event: Event) => setHost((event as CustomEvent<HTMLElement>).detail);
    const closed = (event: Event) => setHost((current) => (current === (event as CustomEvent<HTMLElement>).detail ? null : current));
    window.addEventListener(DIALOG_OPENED_EVENT, opened);
    window.addEventListener(DIALOG_CLOSED_EVENT, closed);
    return () => {
      window.removeEventListener(DIALOG_OPENED_EVENT, opened);
      window.removeEventListener(DIALOG_CLOSED_EVENT, closed);
    };
  }, []);

  // Removing a focused toast fires no blur, so an empty region must not stay paused.
  useEffect(() => {
    if (items.length === 0) setPaused(false);
  }, [items.length]);

  return (
    <ToastContext.Provider value={notify}>
      {children}
      {createPortal(
        <div
          ref={region}
          popover="manual"
          className="sl-toasts"
          role="status"
          aria-live="polite"
          onMouseEnter={() => setPaused(true)}
          onMouseLeave={() => setPaused(false)}
          onFocus={() => setPaused(true)}
          onBlur={(event) => {
            if (!event.currentTarget.contains(event.relatedTarget)) setPaused(false);
          }}
        >
          {items.map((item) => (
            <div key={item.id} className={item.tone === 'error' ? 'sl-toast sl-toast-error' : 'sl-toast'}>
              <span className="sl-toast-text">{item.message}</span>
              <button type="button" className="sl-toast-close" onClick={() => dismiss(item.id)}>
                Dismiss
              </button>
            </div>
          ))}
        </div>,
        host ?? document.body,
      )}
    </ToastContext.Provider>
  );
}

export function useToast(): Notify {
  const notify = useContext(ToastContext);
  if (!notify) throw new Error('useToast must be used inside ToastProvider');
  return notify;
}

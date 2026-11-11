import { createContext, useCallback, useContext, useEffect, useRef, useState, type ReactNode } from 'react';

type Tone = 'info' | 'error';
type ToastItem = { id: number; message: string; tone: Tone };
type Notify = (message: string, tone?: Tone) => void;

const ToastContext = createContext<Notify | null>(null);

export function ToastProvider({ children }: { children: ReactNode }) {
  const [items, setItems] = useState<ToastItem[]>([]);
  const nextId = useRef(1);

  const dismiss = useCallback((id: number) => {
    setItems((current) => current.filter((item) => item.id !== id));
  }, []);

  const notify = useCallback<Notify>(
    (message, tone = 'info') => {
      const id = nextId.current++;
      setItems((current) => [...current.slice(-2), { id, message, tone }]);
      window.setTimeout(() => dismiss(id), tone === 'error' ? 8000 : 4000);
    },
    [dismiss],
  );

  // The region is a manual popover so it lives in the top layer: toasts stay above (and usable over) modal
  // dialogs. Re-opening it on every new toast moves it above a dialog opened after it.
  const region = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const el = region.current;
    if (!el) return;
    if (el.matches(':popover-open')) el.hidePopover();
    el.showPopover();
  }, [items.length]);

  return (
    <ToastContext.Provider value={notify}>
      {children}
      <div ref={region} popover="manual" className="sl-toasts" role="status" aria-live="polite">
        {items.map((item) => (
          <div key={item.id} className={item.tone === 'error' ? 'sl-toast sl-toast-error' : 'sl-toast'}>
            <span className="sl-toast-text">{item.message}</span>
            <button type="button" className="sl-toast-close" onClick={() => dismiss(item.id)}>
              Dismiss
            </button>
          </div>
        ))}
      </div>
    </ToastContext.Provider>
  );
}

export function useToast(): Notify {
  const notify = useContext(ToastContext);
  if (!notify) throw new Error('useToast must be used inside ToastProvider');
  return notify;
}

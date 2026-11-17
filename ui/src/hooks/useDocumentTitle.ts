import { useEffect } from 'react';

/** Names the browser tab after the current page; pages without a title fall back to the product name. */
export function useDocumentTitle(title: string) {
  useEffect(() => {
    document.title = `${title} · SourceLens`;
    return () => {
      document.title = 'SourceLens';
    };
  }, [title]);
}

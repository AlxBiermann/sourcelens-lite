import type { ReactNode } from 'react';
import { ago, sentence } from '../format';
import type { ApiState } from '../hooks/useApi';
import { Loading, Notice } from './Shell';

/**
 * Loading, error and stale states of one `useApi` resource. Renders nothing while the data is current.
 * `describe` replaces the error message, e.g. for a known error code.
 */
export function LoadState({ state, describe }: { state: ApiState<unknown>; describe?: (error: Error) => ReactNode }) {
  const { data, error, loading, updatedAt } = state;
  if (error) {
    const message = describe ? describe(error) : sentence(error.message);
    if (data === null) return <Notice tone="error">{message}</Notice>;
    return (
      <Notice>
        Could not refresh. {message} Last updated {updatedAt ? ago(updatedAt) : 'earlier'}.
      </Notice>
    );
  }
  if (loading && data === null) return <Loading />;
  return null;
}

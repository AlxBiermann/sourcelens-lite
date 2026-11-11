export type Role = 'owner' | 'member';

export type SessionUser = { id: number; login: string; name: string | null; avatarUrl: string | null; role: Role };

export type AppStateDto = {
  version: string;
  publicUrl: string;
  setup: { appConfigured: boolean; ownerExists: boolean; appSlug: string | null; installUrl: string | null };
  user: SessionUser | null;
};

export type JobStatus = 'queued' | 'running' | 'succeeded' | 'failed';
export type JobTrigger = 'connect' | 'webhook' | 'manual' | 'startup' | 'settings';
export type JobPhase = 'resolve' | 'index' | 'embed';

export type Job = {
  id: number;
  status: JobStatus;
  trigger: JobTrigger;
  phase: JobPhase | null;
  targetCommit: string | null;
  filesTotal: number;
  filesDone: number;
  filesChanged: number;
  filesDeleted: number;
  filesSkipped: number;
  embedTotal: number;
  embedDone: number;
  error: string | null;
  createdAt: number;
  startedAt: number | null;
  finishedAt: number | null;
};

export type Repository = {
  id: number;
  fullName: string;
  private: boolean;
  defaultBranch: string;
  branch: string | null;
  effectiveBranch: string;
  state: 'connected' | 'disconnected';
  stateReason: string | null;
  indexedCommit: string | null;
  indexedAt: number | null;
  fileCount: number;
  chunkCount: number;
  embeddedCount: number;
  activeJob: Job | null;
  lastJob: Job | null;
};

export type RepositoryDetail = { repository: Repository; jobs: Job[]; skipped: { reason: string; count: number }[] };

export type GhRepository = { githubId: number; fullName: string; private: boolean; defaultBranch: string; connected: boolean };

export type Installation = {
  installationId: number;
  account: { login: string; avatarUrl: string | null };
  manageUrl: string;
  truncated: boolean;
  repositories: GhRepository[];
};

export type ApiKey = {
  id: number;
  name: string;
  last4: string;
  allRepositories: boolean;
  repositoryIds: number[];
  createdBy: string | null;
  createdAt: number;
  expiresAt: number | null;
  lastUsedAt: number | null;
};

export type EmbeddingSettings = {
  enabled: boolean;
  baseUrl: string;
  model: string;
  requestDimensions: number | null;
  queryPrefix: string;
  documentPrefix: string;
  batchSize: number;
  dimensions: number | null;
  apiKeySet: boolean;
  embeddedChunks: number;
  totalChunks: number;
  failedChunks: number;
};

export type EmbeddingSettingsInput = {
  enabled: boolean;
  baseUrl: string;
  model: string;
  requestDimensions: number | null;
  queryPrefix: string;
  documentPrefix: string;
  batchSize: number;
  apiKey?: string;
  clearApiKey?: boolean;
};

export type EmbeddingTestResult = { ok: true; dimensions: number; latencyMs: number } | { ok: false; error: string };

export type GithubSettings = {
  source: 'manifest' | 'env';
  slug: string;
  appUrl: string;
  installUrl: string;
  webhookUrl: string;
  webhookActive: boolean;
  lastDeliveryAt: number | null;
  lastDeliveryEvent: string | null;
};

export type Member = {
  id: number;
  login: string;
  name: string | null;
  avatarUrl: string | null;
  role: Role;
  pending: boolean;
  lastLoginAt: number | null;
};

export type MatchChannel = 'symbol' | 'text_all' | 'text_any' | 'semantic';

export type SearchHit = {
  repository: string;
  path: string;
  language: string | null;
  kind: string;
  symbolPath: string;
  startLine: number;
  endLine: number;
  content: string;
  score: number;
  matchedBy: MatchChannel[];
};

export type SearchResult = { mode: 'lexical' | 'hybrid'; terms: string[]; results: SearchHit[] };

export class ApiError extends Error {
  readonly code: string;
  readonly status: number;

  constructor(status: number, code: string, message: string) {
    super(message);
    this.status = status;
    this.code = code;
  }
}

const SIGN_IN_AGAIN: Record<string, true> = { unauthenticated: true, github_reauth_required: true };

export async function request<T>(method: string, path: string, body?: unknown): Promise<T> {
  let response: Response;
  try {
    response = await fetch(path, {
      method,
      credentials: 'include',
      headers: body === undefined ? { accept: 'application/json' } : { accept: 'application/json', 'content-type': 'application/json' },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
  } catch {
    throw new ApiError(0, 'network', 'SourceLens is not reachable. Check your connection and try again.');
  }

  if (response.status === 204) return undefined as T;

  const text = await response.text();
  let payload: unknown = null;
  if (text) {
    try {
      payload = JSON.parse(text);
    } catch {
      payload = null;
    }
  }

  if (!response.ok) {
    const error = (payload as { error?: { code?: string; message?: string } } | null)?.error;
    const code = error?.code ?? 'http_error';
    const message = error?.message ?? `The server answered ${response.status}.`;
    if (response.status === 401 && SIGN_IN_AGAIN[code] && window.location.pathname !== '/login') {
      window.location.assign('/login');
    }
    throw new ApiError(response.status, code, message);
  }

  return payload as T;
}

export const api = {
  get: <T>(path: string) => request<T>('GET', path),
  post: <T>(path: string, body?: unknown) => request<T>('POST', path, body ?? {}),
  put: <T>(path: string, body: unknown) => request<T>('PUT', path, body),
  patch: <T>(path: string, body: unknown) => request<T>('PATCH', path, body),
  del: (path: string) => request<void>('DELETE', path),
};

export function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  return 'Something went wrong. Try again.';
}

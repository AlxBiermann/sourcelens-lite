export type Role = 'owner' | 'member';

export type Provider = 'github' | 'sso';

/** `pendingApprovals`: accounts waiting for approval; always 0 for members. */
export type SessionUser = { id: number; login: string; name: string | null; avatarUrl: string | null; role: Role; provider: Provider; pendingApprovals: number };

export type AppStateDto = {
  version: string;
  publicUrl: string;
  setup: { appConfigured: boolean; ownerExists: boolean; appSlug: string | null; installUrl: string | null };
  sso: { name: string } | null;
  user: SessionUser | null;
  /** SOURCELENS_DEV_MODE: dev sign-in and demo data, no setup required. */
  devMode: boolean;
};

/** API keys are open to admins and to members who sign in with GitHub. */
export function canManageApiKeys(user: SessionUser): boolean {
  return user.provider !== 'sso' || user.role === 'owner';
}

/** GitHub logins are shown with a leading @; SSO logins are usually e-mail addresses and are shown as they are. */
export function handle(user: Pick<SessionUser, 'login' | 'provider'>): string {
  return user.provider === 'github' ? `@${user.login}` : user.login;
}

export type JobStatus = 'queued' | 'running' | 'succeeded' | 'failed';
export type JobTrigger = 'connect' | 'webhook' | 'manual' | 'startup' | 'settings' | 'schedule';
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

export type SyncSettings = {
  enabled: boolean;
  cron: string;
  description: string | null;
  nextRunAt: number | null;
};

export type SsoSettings = {
  enabled: boolean;
  name: string;
  issuer: string;
  clientId: string;
  clientSecretSet: boolean;
  redirectUrl: string;
};

export type SsoSettingsInput = {
  enabled: boolean;
  name: string;
  issuer: string;
  clientId: string;
  clientSecret?: string;
};

/**
 * Consent page data. `redirectTarget` is where the browser goes afterwards, for display: a host, `host:port` for an
 * app on this computer (`redirectLocal`), or `scheme://host` for an app link. `error` and `redirectTo` are set when
 * the request is invalid and can only be returned to the client; the page never follows `redirectTo` by itself.
 */
export type OAuthAuthorizeView = { clientName: string; redirectTarget: string; redirectLocal: boolean; redirectTo: string | null; error: string | null };

export type OAuthGrant = { id: number; clientName: string; user: string; createdAt: number; lastUsedAt: number | null };

export type Member = {
  id: number;
  login: string;
  name: string | null;
  avatarUrl: string | null;
  role: Role;
  provider: Provider;
  email: string | null;
  pending: boolean;
  awaitingApproval: boolean;
  lastLoginAt: number | null;
};

export type MatchChannel = 'symbol' | 'text_all' | 'text_any' | 'semantic';

export type SearchHit = {
  repository: string;
  path: string;
  language: string | null;
  kind: string;
  symbolPath: string;
  /** Up to three symbols that start inside the chunk; shown for `block` / `block_part` chunks. */
  names: string[];
  startLine: number;
  endLine: number;
  content: string;
  score: number;
  matchedBy: MatchChannel[];
};

export type SearchResult = { mode: 'lexical' | 'hybrid'; terms: string[]; results: SearchHit[] };

export type SearchLanguages = { languages: string[] };

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

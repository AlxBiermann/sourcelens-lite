// Browser session handling: access tokens, refresh and sign-out.
import { httpClient } from "../api/client";

const REFRESH_MARGIN_MS = 60_000;

export interface Tokens {
  accessToken: string;
  refreshToken: string;
  expiresAt: number;
}

/** Keeps the current tokens in local storage so a reload stays signed in. */
export class SessionStore {
  private static readonly KEY = "shopflow.session";

  load(): Tokens | null {
    const raw = window.localStorage.getItem(SessionStore.KEY);
    return raw ? (JSON.parse(raw) as Tokens) : null;
  }

  save(tokens: Tokens): void {
    window.localStorage.setItem(SessionStore.KEY, JSON.stringify(tokens));
  }

  clear(): void {
    window.localStorage.removeItem(SessionStore.KEY);
  }
}

/** True when the access token expires within the refresh margin. */
export function isSessionExpired(tokens: Tokens, now: number = Date.now()): boolean {
  return tokens.expiresAt - REFRESH_MARGIN_MS <= now;
}

/**
 * Exchanges the refresh token for a new access token. A rejected refresh token signs the user
 * out, because the session was revoked on the server.
 */
export async function refreshAccessToken(store: SessionStore): Promise<Tokens | null> {
  const current = store.load();
  if (!current) {
    return null;
  }
  const response = await httpClient.post("/oauth/token", {
    body: { grant_type: "refresh_token", refresh_token: current.refreshToken },
  });
  if (response.status === 400 || response.status === 401) {
    store.clear();
    return null;
  }
  const tokens: Tokens = {
    accessToken: response.body.access_token,
    refreshToken: response.body.refresh_token ?? current.refreshToken,
    expiresAt: Date.now() + response.body.expires_in * 1000,
  };
  store.save(tokens);
  return tokens;
}

/** Revokes the refresh token on the server and forgets the local session. */
export async function signOut(store: SessionStore): Promise<void> {
  const current = store.load();
  if (current) {
    await httpClient.post("/oauth/revoke", { body: { token: current.refreshToken } });
  }
  store.clear();
  window.location.assign("/login");
}

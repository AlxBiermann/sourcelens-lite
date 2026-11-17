// Account endpoints of the storefront: profile lookup, listing and email changes.
import { httpClient, ApiError } from "./client";
import { UserCache } from "../cache/userCache";

export interface UserProfile {
  id: string;
  displayName: string;
  email: string;
  emailVerified: boolean;
  createdAt: Date;
  roles: string[];
}

const cache = new UserCache(500);

/** Loads one user profile, from the in-memory cache when it is still fresh. */
export async function getUserById(userId: string): Promise<UserProfile | null> {
  const cached = cache.get(userId);
  if (cached) {
    return cached;
  }
  try {
    const response = await httpClient.get(`/v1/users/${encodeURIComponent(userId)}`);
    const profile = toProfile(response.body);
    cache.set(userId, profile);
    return profile;
  } catch (err) {
    if (err instanceof ApiError && err.status === 404) {
      return null;
    }
    throw err;
  }
}

/** One page of users who signed in during the last `days` days, newest first. */
export async function listActiveUsers(days: number, page: number): Promise<UserProfile[]> {
  const since = new Date(Date.now() - days * 24 * 60 * 60 * 1000).toISOString();
  const response = await httpClient.get("/v1/users", {
    query: { lastSeenAfter: since, page: String(page), sort: "-lastSeen" },
  });
  return response.body.items.map(toProfile);
}

/**
 * Changes the email address of an account. The new address stays unverified until the user
 * clicks the confirmation link, so the cached profile is dropped.
 */
export async function updateUserEmail(userId: string, newEmail: string): Promise<void> {
  if (!/^[^@\s]+@[^@\s]+\.[^@\s]+$/.test(newEmail)) {
    throw new Error(`Invalid email address: ${newEmail}`);
  }
  await httpClient.patch(`/v1/users/${encodeURIComponent(userId)}`, {
    body: { email: newEmail },
  });
  cache.delete(userId);
}

function toProfile(raw: Record<string, unknown>): UserProfile {
  return {
    id: String(raw.id),
    displayName: String(raw.display_name ?? raw.login),
    email: String(raw.email),
    emailVerified: raw.email_verified === true,
    createdAt: new Date(String(raw.created_at)),
    roles: Array.isArray(raw.roles) ? raw.roles.map(String) : [],
  };
}

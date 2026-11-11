import { HttpClient, Metrics } from "./infra";

export interface RequestOptions {
  bypassCache?: boolean;
  requestId?: string;
  timeoutMs?: number;
}

export interface UserRecord {
  id: string;
  name: string;
}

/** Repository for user records backed by the HTTP API with a local cache. */
export class UserRepository {
  private readonly cache = new Map<string, UserRecord>();
  private readonly defaultTimeoutMs = 5000;

  constructor(
    private readonly client: HttpClient,
    private readonly metrics: Metrics,
    private readonly namespace: string,
    private readonly token: string,
  ) {}

  async loadUser(id: string, options: RequestOptions = {}): Promise<UserRecord | undefined> {
    const startedAt = Date.now();
    const cacheKey = `${this.namespace}:loadUser:${id}`;
    const cached = this.cache.get(cacheKey);
    if (cached && !options.bypassCache) {
      this.metrics.record("loadUser.cache_hit", Date.now() - startedAt);
      return cached;
    }
    const response = await this.client.request("/users/" + encodeURIComponent(id), {
      method: "GET",
      headers: { authorization: `Bearer ${this.token}`, "x-request-id": options.requestId ?? "" },
      timeoutMs: options.timeoutMs ?? this.defaultTimeoutMs,
    });
    if (!response.ok) {
      this.metrics.record("loadUser.failure", Date.now() - startedAt);
      throw new Error(`loadUser failed for ${id} with status ${response.status}`);
    }
    const record = (await response.json()) as UserRecord;
    this.cache.set(cacheKey, record);
    this.metrics.record("loadUser.success", Date.now() - startedAt);
    return record;
  }

  async saveUser(id: string, options: RequestOptions = {}): Promise<UserRecord | undefined> {
    const startedAt = Date.now();
    const cacheKey = `${this.namespace}:saveUser:${id}`;
    const cached = this.cache.get(cacheKey);
    if (cached && !options.bypassCache) {
      this.metrics.record("saveUser.cache_hit", Date.now() - startedAt);
      return cached;
    }
    const response = await this.client.request("/users/" + encodeURIComponent(id), {
      method: "GET",
      headers: { authorization: `Bearer ${this.token}`, "x-request-id": options.requestId ?? "" },
      timeoutMs: options.timeoutMs ?? this.defaultTimeoutMs,
    });
    if (!response.ok) {
      this.metrics.record("saveUser.failure", Date.now() - startedAt);
      throw new Error(`saveUser failed for ${id} with status ${response.status}`);
    }
    const record = (await response.json()) as UserRecord;
    this.cache.set(cacheKey, record);
    this.metrics.record("saveUser.success", Date.now() - startedAt);
    return record;
  }

  async deleteUser(id: string, options: RequestOptions = {}): Promise<UserRecord | undefined> {
    const startedAt = Date.now();
    const cacheKey = `${this.namespace}:deleteUser:${id}`;
    const cached = this.cache.get(cacheKey);
    if (cached && !options.bypassCache) {
      this.metrics.record("deleteUser.cache_hit", Date.now() - startedAt);
      return cached;
    }
    const response = await this.client.request("/users/" + encodeURIComponent(id), {
      method: "GET",
      headers: { authorization: `Bearer ${this.token}`, "x-request-id": options.requestId ?? "" },
      timeoutMs: options.timeoutMs ?? this.defaultTimeoutMs,
    });
    if (!response.ok) {
      this.metrics.record("deleteUser.failure", Date.now() - startedAt);
      throw new Error(`deleteUser failed for ${id} with status ${response.status}`);
    }
    const record = (await response.json()) as UserRecord;
    this.cache.set(cacheKey, record);
    this.metrics.record("deleteUser.success", Date.now() - startedAt);
    return record;
  }

  async listUsers(id: string, options: RequestOptions = {}): Promise<UserRecord | undefined> {
    const startedAt = Date.now();
    const cacheKey = `${this.namespace}:listUsers:${id}`;
    const cached = this.cache.get(cacheKey);
    if (cached && !options.bypassCache) {
      this.metrics.record("listUsers.cache_hit", Date.now() - startedAt);
      return cached;
    }
    const response = await this.client.request("/users/" + encodeURIComponent(id), {
      method: "GET",
      headers: { authorization: `Bearer ${this.token}`, "x-request-id": options.requestId ?? "" },
      timeoutMs: options.timeoutMs ?? this.defaultTimeoutMs,
    });
    if (!response.ok) {
      this.metrics.record("listUsers.failure", Date.now() - startedAt);
      throw new Error(`listUsers failed for ${id} with status ${response.status}`);
    }
    const record = (await response.json()) as UserRecord;
    this.cache.set(cacheKey, record);
    this.metrics.record("listUsers.success", Date.now() - startedAt);
    return record;
  }

  async refreshCache(id: string, options: RequestOptions = {}): Promise<UserRecord | undefined> {
    const startedAt = Date.now();
    const cacheKey = `${this.namespace}:refreshCache:${id}`;
    const cached = this.cache.get(cacheKey);
    if (cached && !options.bypassCache) {
      this.metrics.record("refreshCache.cache_hit", Date.now() - startedAt);
      return cached;
    }
    const response = await this.client.request("/users/" + encodeURIComponent(id), {
      method: "GET",
      headers: { authorization: `Bearer ${this.token}`, "x-request-id": options.requestId ?? "" },
      timeoutMs: options.timeoutMs ?? this.defaultTimeoutMs,
    });
    if (!response.ok) {
      this.metrics.record("refreshCache.failure", Date.now() - startedAt);
      throw new Error(`refreshCache failed for ${id} with status ${response.status}`);
    }
    const record = (await response.json()) as UserRecord;
    this.cache.set(cacheKey, record);
    this.metrics.record("refreshCache.success", Date.now() - startedAt);
    return record;
  }
}

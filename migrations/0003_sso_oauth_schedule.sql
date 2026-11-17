-- SSO accounts are identified by issuer + subject and wait for approval as 'pending'.
ALTER TABLE users ADD COLUMN sso_issuer TEXT;
ALTER TABLE users ADD COLUMN sso_sub TEXT;
ALTER TABLE users ADD COLUMN email TEXT;
ALTER TABLE users ADD COLUMN status TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active','pending'));
CREATE UNIQUE INDEX users_sso ON users(sso_issuer, sso_sub) WHERE sso_sub IS NOT NULL;

-- Sessions of SSO accounts have no GitHub token: gh_token becomes nullable.
CREATE TABLE sessions_new (
  id_hash TEXT PRIMARY KEY,
  user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  gh_token BLOB,                                  -- sealed GhToken JSON; NULL for SSO sessions
  created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL);
INSERT INTO sessions_new (id_hash, user_id, gh_token, created_at, expires_at)
  SELECT id_hash, user_id, gh_token, created_at, expires_at FROM sessions;
DROP TABLE sessions;
ALTER TABLE sessions_new RENAME TO sessions;

-- jobs.trigger gains 'schedule'.
CREATE TABLE jobs_new (
  id INTEGER PRIMARY KEY,
  repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
  status TEXT NOT NULL DEFAULT 'queued' CHECK (status IN ('queued','running','succeeded','failed')),
  trigger TEXT NOT NULL CHECK (trigger IN ('connect','webhook','manual','startup','settings','schedule')),
  target_commit TEXT,
  force INTEGER NOT NULL DEFAULT 0,
  phase TEXT,
  files_total INTEGER NOT NULL DEFAULT 0, files_done INTEGER NOT NULL DEFAULT 0,
  files_changed INTEGER NOT NULL DEFAULT 0, files_deleted INTEGER NOT NULL DEFAULT 0, files_skipped INTEGER NOT NULL DEFAULT 0,
  embed_total INTEGER NOT NULL DEFAULT 0, embed_done INTEGER NOT NULL DEFAULT 0,
  error TEXT,
  created_at INTEGER NOT NULL, started_at INTEGER, finished_at INTEGER);
INSERT INTO jobs_new (id, repo_id, status, trigger, target_commit, force, phase, files_total, files_done,
    files_changed, files_deleted, files_skipped, embed_total, embed_done, error, created_at, started_at, finished_at)
  SELECT id, repo_id, status, trigger, target_commit, force, phase, files_total, files_done,
    files_changed, files_deleted, files_skipped, embed_total, embed_done, error, created_at, started_at, finished_at FROM jobs;
DROP TABLE jobs;
ALTER TABLE jobs_new RENAME TO jobs;
CREATE UNIQUE INDEX jobs_one_queued ON jobs(repo_id) WHERE status = 'queued';
CREATE INDEX jobs_repo ON jobs(repo_id, id DESC);

-- OAuth grants of MCP clients: one row per (user, client), tokens stored as sha256 hex.
CREATE TABLE oauth_grants (
  id INTEGER PRIMARY KEY,
  user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  client_hash TEXT NOT NULL,                      -- sha256 hex of the client_id
  client_name TEXT NOT NULL,
  access_hash TEXT NOT NULL UNIQUE, access_expires_at INTEGER NOT NULL,
  refresh_hash TEXT NOT NULL UNIQUE, refresh_expires_at INTEGER NOT NULL,
  prev_refresh_hash TEXT UNIQUE,                  -- refresh token the last rotation replaced; presenting it revokes the grant
  created_at INTEGER NOT NULL, last_used_at INTEGER);
CREATE INDEX oauth_grants_user ON oauth_grants(user_id);

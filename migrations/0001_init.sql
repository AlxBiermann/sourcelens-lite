CREATE TABLE settings (key TEXT PRIMARY KEY, value BLOB NOT NULL, updated_at INTEGER NOT NULL);

CREATE TABLE users (
  id INTEGER PRIMARY KEY,
  github_id INTEGER UNIQUE,                       -- NULL = invited, not yet signed in
  login TEXT NOT NULL UNIQUE COLLATE NOCASE,
  name TEXT, avatar_url TEXT,
  role TEXT NOT NULL CHECK (role IN ('owner','member')),
  created_at INTEGER NOT NULL, last_login_at INTEGER);

CREATE TABLE sessions (
  id_hash TEXT PRIMARY KEY,                       -- sha256 of the cookie value
  user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  gh_token BLOB NOT NULL,                         -- sealed JSON {accessToken, refreshToken, expiresAt}
  created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL);

CREATE TABLE repositories (
  id INTEGER PRIMARY KEY,
  github_id INTEGER NOT NULL UNIQUE,
  installation_id INTEGER NOT NULL,
  full_name TEXT NOT NULL COLLATE NOCASE,
  private INTEGER NOT NULL,
  default_branch TEXT NOT NULL,
  branch TEXT,                                    -- NULL = follow default_branch
  state TEXT NOT NULL DEFAULT 'connected' CHECK (state IN ('connected','disconnected','deleting')),
  state_reason TEXT,
  indexed_commit TEXT, indexed_at INTEGER,
  chunker_version INTEGER NOT NULL DEFAULT 0,
  file_count INTEGER NOT NULL DEFAULT 0, chunk_count INTEGER NOT NULL DEFAULT 0,
  added_by INTEGER REFERENCES users(id) ON DELETE SET NULL,
  created_at INTEGER NOT NULL);
CREATE INDEX repositories_full_name ON repositories(full_name);
CREATE INDEX repositories_installation ON repositories(installation_id);

CREATE TABLE files (
  id INTEGER PRIMARY KEY,
  repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
  path TEXT NOT NULL,
  blob_sha TEXT NOT NULL,
  language TEXT,
  size INTEGER NOT NULL,
  line_count INTEGER NOT NULL DEFAULT 0,
  skip_reason TEXT,                               -- NULL = indexed; 'binary' | 'minified' | 'generated' | 'not_in_archive'
  content BLOB,                                   -- LF-normalised UTF-8; NULL when skipped
  UNIQUE (repo_id, path));

CREATE TABLE chunks (
  id INTEGER PRIMARY KEY,
  repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
  file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
  kind TEXT NOT NULL,
  symbol_path TEXT NOT NULL DEFAULT '',           -- segments joined with ' > '
  start_line INTEGER NOT NULL, end_line INTEGER NOT NULL,
  start_byte INTEGER NOT NULL, end_byte INTEGER NOT NULL,   -- offsets into files.content
  embed_hash TEXT NOT NULL,
  embed_state INTEGER NOT NULL DEFAULT 0);        -- 0 pending, 1 embedded, 2 failed
CREATE INDEX chunks_file ON chunks(file_id, start_line);
CREATE INDEX chunks_pending ON chunks(repo_id) WHERE embed_state = 0;

CREATE VIRTUAL TABLE chunks_fts USING fts5(
  names, path, body, idents,
  content='', contentless_delete=1, tokenize='unicode61 remove_diacritics 2');   -- rowid = chunks.id

CREATE TABLE symbols (
  id INTEGER PRIMARY KEY,
  repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
  file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
  kind TEXT NOT NULL,
  name TEXT NOT NULL COLLATE NOCASE,
  qualified_name TEXT NOT NULL,
  signature TEXT,
  start_line INTEGER NOT NULL, end_line INTEGER NOT NULL,
  depth INTEGER NOT NULL DEFAULT 0);
CREATE INDEX symbols_name ON symbols(name, repo_id);
CREATE INDEX symbols_file ON symbols(file_id, start_line);
CREATE VIRTUAL TABLE symbols_fts USING fts5(name, content='', contentless_delete=1, tokenize='trigram');  -- rowid = symbols.id

CREATE TABLE jobs (
  id INTEGER PRIMARY KEY,
  repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
  status TEXT NOT NULL DEFAULT 'queued' CHECK (status IN ('queued','running','succeeded','failed')),
  trigger TEXT NOT NULL CHECK (trigger IN ('connect','webhook','manual','startup','settings')),
  target_commit TEXT,
  force INTEGER NOT NULL DEFAULT 0,
  phase TEXT,                                     -- 'resolve' | 'index' | 'embed'
  files_total INTEGER NOT NULL DEFAULT 0, files_done INTEGER NOT NULL DEFAULT 0,
  files_changed INTEGER NOT NULL DEFAULT 0, files_deleted INTEGER NOT NULL DEFAULT 0, files_skipped INTEGER NOT NULL DEFAULT 0,
  embed_total INTEGER NOT NULL DEFAULT 0, embed_done INTEGER NOT NULL DEFAULT 0,
  error TEXT,
  created_at INTEGER NOT NULL, started_at INTEGER, finished_at INTEGER);
CREATE UNIQUE INDEX jobs_one_queued ON jobs(repo_id) WHERE status = 'queued';
CREATE INDEX jobs_repo ON jobs(repo_id, id DESC);

CREATE TABLE api_keys (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL,
  key_hash TEXT NOT NULL UNIQUE,                  -- sha256 hex of the full key
  last4 TEXT NOT NULL,
  all_repos INTEGER NOT NULL DEFAULT 1,
  created_by INTEGER REFERENCES users(id) ON DELETE SET NULL,
  created_at INTEGER NOT NULL, expires_at INTEGER, last_used_at INTEGER);
CREATE TABLE api_key_repos (
  key_id INTEGER NOT NULL REFERENCES api_keys(id) ON DELETE CASCADE,
  repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
  PRIMARY KEY (key_id, repo_id));

CREATE TABLE webhook_deliveries (delivery_id TEXT PRIMARY KEY, event TEXT NOT NULL, received_at INTEGER NOT NULL);

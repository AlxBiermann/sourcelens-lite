-- Per-repository counts and FK cascades need an index leading with repo_id.
DROP INDEX chunks_pending;
CREATE INDEX chunks_repo_state ON chunks(repo_id, embed_state);
CREATE INDEX symbols_repo ON symbols(repo_id);
-- Invitations are bound to a GitHub account id now; invitations by login alone are dropped.
DELETE FROM users WHERE github_id IS NULL;

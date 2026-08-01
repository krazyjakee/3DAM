-- Tables added by the accounts/shares release, applied after pre_accounts.sql.
CREATE TABLE account (
  account_id TEXT PRIMARY KEY, username TEXT NOT NULL UNIQUE COLLATE NOCASE,
  display_name TEXT, password_hash TEXT, role TEXT NOT NULL,
  disabled INTEGER NOT NULL DEFAULT 0, created INTEGER NOT NULL, last_login INTEGER
);
CREATE TABLE session (
  session_id TEXT PRIMARY KEY,
  account_id TEXT NOT NULL REFERENCES account(account_id) ON DELETE CASCADE,
  secret_hash TEXT NOT NULL, csrf TEXT NOT NULL, created INTEGER NOT NULL,
  last_seen INTEGER NOT NULL, absolute_exp INTEGER NOT NULL, user_agent TEXT
);
CREATE INDEX session_account ON session(account_id);
CREATE TABLE login_failure (username TEXT NOT NULL COLLATE NOCASE, at INTEGER NOT NULL);
CREATE INDEX login_failure_user ON login_failure(username, at);
CREATE TABLE group_ (
  group_id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE COLLATE NOCASE, created INTEGER NOT NULL
);
CREATE TABLE group_member (
  group_id TEXT NOT NULL REFERENCES group_(group_id) ON DELETE CASCADE,
  account_id TEXT NOT NULL REFERENCES account(account_id) ON DELETE CASCADE,
  PRIMARY KEY (group_id, account_id)
);
CREATE TABLE share (
  share_id TEXT PRIMARY KEY, resource TEXT NOT NULL, resource_id TEXT NOT NULL,
  account_id TEXT REFERENCES account(account_id) ON DELETE CASCADE,
  group_id TEXT REFERENCES group_(group_id) ON DELETE CASCADE,
  access TEXT NOT NULL, granted_by TEXT NOT NULL, created INTEGER NOT NULL,
  CHECK ((account_id IS NULL) != (group_id IS NULL))
);
CREATE INDEX share_resource ON share(resource, resource_id);

INSERT INTO account VALUES
  ('account-1', 'owner', 'Fixture Owner', '$argon2id$fixture-password-hash',
   'admin', 0, 201, 202);
INSERT INTO session VALUES
  ('session-1', 'account-1', 'session-secret-hash', 'csrf-secret', 203, 204,
   9999999999999, 'fixture-agent');
INSERT INTO login_failure VALUES ('OWNER', 205);
INSERT INTO group_ VALUES ('group-1', 'Reviewers', 206);
INSERT INTO group_member VALUES ('group-1', 'account-1');
INSERT INTO share VALUES
  ('share-1', 'source', 'source-1', NULL, 'group-1', 'read', 'account-1', 207);

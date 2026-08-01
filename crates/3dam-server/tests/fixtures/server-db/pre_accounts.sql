-- The original unversioned server.db: flags, bearer credentials, and audit history only.
CREATE TABLE feature_flag (
  key TEXT PRIMARY KEY, value TEXT NOT NULL, version INTEGER NOT NULL,
  updated_at INTEGER NOT NULL, updated_by TEXT
);
CREATE TABLE token (
  token_id TEXT PRIMARY KEY, label TEXT NOT NULL, secret_hash TEXT NOT NULL UNIQUE,
  scopes TEXT NOT NULL, created INTEGER NOT NULL, expires INTEGER, last_used INTEGER
);
CREATE INDEX token_secret ON token(secret_hash);
CREATE TABLE audit_log (
  id INTEGER PRIMARY KEY AUTOINCREMENT, at INTEGER NOT NULL, actor TEXT NOT NULL,
  action TEXT NOT NULL, target TEXT, detail TEXT
);

INSERT INTO feature_flag VALUES
  ('authentication', '"token"', 7, 101, 'fixture-admin');
INSERT INTO token VALUES
  ('token-1', 'automation', 'token-secret-hash', '{"read":true,"admin":true}',
   102, 9999999999999, 103);
INSERT INTO audit_log (at, actor, action, target, detail) VALUES
  (104, 'fixture-admin', 'token.create', 'token-1', '{"ip":"192.0.2.1"}');

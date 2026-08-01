-- Tables added by the OIDC release, applied after pre_accounts.sql + accounts_shares.sql.
CREATE TABLE oidc_provider (
  id INTEGER PRIMARY KEY CHECK (id = 1), issuer TEXT NOT NULL, client_id TEXT NOT NULL,
  client_secret TEXT, redirect_url TEXT NOT NULL, scopes TEXT NOT NULL,
  provisioning TEXT NOT NULL, updated_at INTEGER NOT NULL, updated_by TEXT
);
CREATE TABLE oidc_identity (
  issuer TEXT NOT NULL, subject TEXT NOT NULL,
  account_id TEXT NOT NULL REFERENCES account(account_id) ON DELETE CASCADE,
  linked_at INTEGER NOT NULL, PRIMARY KEY (issuer, subject)
);
CREATE INDEX oidc_identity_account ON oidc_identity(account_id);
CREATE TABLE oidc_login (
  state TEXT PRIMARY KEY, nonce TEXT NOT NULL, pkce_verifier TEXT NOT NULL,
  return_to TEXT, browser_hash TEXT NOT NULL, created INTEGER NOT NULL
);

INSERT INTO oidc_provider VALUES
  (1, 'https://issuer.example', 'fixture-client', 'oidc-client-secret',
   'https://dam.example/api/v1/auth/oidc/callback', '["groups"]', 'linked',
   301, 'fixture-admin');
INSERT INTO oidc_identity VALUES
  ('https://issuer.example', 'subject-1', 'account-1', 302);
INSERT INTO oidc_login VALUES
  ('state-secret', 'nonce-secret', 'pkce-verifier-secret', '/settings',
   'browser-cookie-hash', 303);

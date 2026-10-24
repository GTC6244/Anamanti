-- Anamanti website: waitlist signups and contact messages (D1 database "anamanti-site").
-- Apply with: npx wrangler d1 migrations apply anamanti-site --remote

CREATE TABLE IF NOT EXISTS waitlist (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  email       TEXT    NOT NULL UNIQUE COLLATE NOCASE,
  name        TEXT,
  interest    TEXT,              -- what they'd use it for / which hardware (free text, optional)
  source      TEXT,              -- page the form was submitted from
  ip_hash     TEXT    NOT NULL,  -- salted SHA-256 of the client IP, for rate limiting only
  created_at  TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

CREATE TABLE IF NOT EXISTS contact (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  name        TEXT,
  email       TEXT    NOT NULL,
  topic       TEXT,
  message     TEXT    NOT NULL,
  ip_hash     TEXT    NOT NULL,
  created_at  TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
  handled     INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS waitlist_ip_time ON waitlist (ip_hash, created_at);
CREATE INDEX IF NOT EXISTS contact_ip_time  ON contact  (ip_hash, created_at);

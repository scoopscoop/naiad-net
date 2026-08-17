-- Squashed baseline for the simple client/server model (spec §3, ADR 0021).
-- Zero real deployments exist, so the migration chain is squashed to one fresh
-- baseline rather than carrying twelve incremental steps forward.

CREATE TABLE accounts (
  pubkey     TEXT PRIMARY KEY,
  role       TEXT NOT NULL DEFAULT 'contributor' CHECK (role IN ('contributor','moderator')),
  banned     INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL,
  note       TEXT
);

CREATE TABLE submissions (            -- signed append-only log (kept internal; seeds future mirroring)
  seq        INTEGER PRIMARY KEY AUTOINCREMENT,
  op         TEXT NOT NULL CHECK (op IN ('add','remove')),
  hash       TEXT NOT NULL,
  tag        TEXT NOT NULL,
  author     TEXT NOT NULL,           -- submitter pubkey (server-internal; never on the wire)
  signature  TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE INDEX idx_submissions_key ON submissions(hash, tag, seq);

CREATE TABLE repo_mappings (          -- current view: (hash, tag, status, seq)
  hash   TEXT NOT NULL,
  tag    TEXT NOT NULL,
  status TEXT NOT NULL CHECK (status IN ('current','deleted')),
  seq    INTEGER NOT NULL,
  PRIMARY KEY (hash, tag)
);
CREATE INDEX idx_repo_mappings_hash ON repo_mappings(hash);
CREATE INDEX idx_repo_mappings_seq  ON repo_mappings(seq);

-- Signed, tombstone-able tag relations (siblings/parents). Includes seq column
-- from 0006, excludes origin column from 0011.
CREATE TABLE relations (
  kind       TEXT NOT NULL,
  from_tag   TEXT NOT NULL,
  to_tag     TEXT NOT NULL,
  author     TEXT NOT NULL,
  status     TEXT NOT NULL CHECK (status IN ('current','deleted')),
  signature  BLOB NOT NULL,
  created_at INTEGER NOT NULL,
  seq        INTEGER NOT NULL DEFAULT 0,
  UNIQUE(kind, from_tag, to_tag, author)
);
CREATE INDEX relations_seq ON relations(seq);

CREATE TABLE reports (
  id              INTEGER PRIMARY KEY AUTOINCREMENT,
  hash            TEXT NOT NULL,
  tag             TEXT NOT NULL,
  reporter_pubkey TEXT NOT NULL,
  note            TEXT,
  created_at      INTEGER NOT NULL,
  status          TEXT NOT NULL DEFAULT 'open' CHECK (status IN ('open','closed'))
);
CREATE INDEX idx_reports_status ON reports(status, created_at);

-- Which contributor identity a shared service signs with. 'legacy' = the old
-- global naiad.key (continuity for repos already used before this upgrade);
-- 'derived' = BLAKE3-derived from the master seed + repo_anchor (unlinkable
-- across repos, ADR 0020 §6). repo_anchor is the frozen derivation anchor: the
-- repo's GENESIS identity key (root of its #83 rotation chain, via #84's
-- genesis_key), or the normalized URL when no repo_key is advertised.
-- NULL until first resolved; written once, never updated (a #83 rotation moves
-- the verification pin, never this anchor).
ALTER TABLE services ADD COLUMN contributor_mode TEXT NOT NULL DEFAULT 'derived'; -- 'legacy'|'derived'
ALTER TABLE services ADD COLUMN repo_anchor      TEXT;

-- CONTINUITY: every shared service that already exists at upgrade keeps the old
-- global key so repos that already know the user's pubkey see no discontinuity
-- (probation/ownership/history preserved). New subscriptions default 'derived'.
UPDATE services SET contributor_mode = 'legacy' WHERE scope = 'shared';

-- What the client filed and where it stands. Local-only bookkeeping; the
-- petition itself lives at the origin repo. status mirrors the repo's
-- lifecycle: 'open' on file -> 'approved'/'rejected' as the origin's outcome
-- arrives via the signed status poll -> 'open' again if a Lift reopens
-- (ADR 0015 re-petition cycle).
CREATE TABLE filed_petitions (
    service_id  INTEGER NOT NULL REFERENCES services(id),
    hash        TEXT    NOT NULL,   -- content hash as filed (petitioner may not still own the file)
    tag         TEXT    NOT NULL,
    petitioner  TEXT    NOT NULL,   -- the per-repo derived pubkey used to file
    reason      TEXT,
    status      TEXT    NOT NULL DEFAULT 'open',  -- 'open'|'approved'|'rejected'
    filed_at    INTEGER NOT NULL,
    resolved_at INTEGER,            -- when a terminal outcome was observed
    PRIMARY KEY (service_id, hash, tag)
);

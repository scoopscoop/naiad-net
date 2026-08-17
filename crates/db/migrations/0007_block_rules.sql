-- Client-side moderation: a local block list (ADR 0006). Rules suppress pulled
-- tags at read time. Purely local state — never federated, no status column
-- (a rule is present or absent; removal is a row delete).
CREATE TABLE block_rules (
    id         INTEGER PRIMARY KEY,
    kind       TEXT NOT NULL,   -- 'tag' | 'tag_pattern' | 'author'
    target     TEXT NOT NULL,   -- tag: 'ns:subtag'; pattern: glob; author: 64-hex pubkey
    note       TEXT,            -- optional human reason, nullable
    created_at INTEGER NOT NULL,
    UNIQUE(kind, target)
);

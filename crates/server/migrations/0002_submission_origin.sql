-- Generation-source origin on the server (#162, ADR 0026). Additive, no index.
-- Origin is filtering/display metadata: it must NEVER key a query, so no index
-- is created here — the seq/hash indexes remain the only access paths.

-- The signed append-only log gains origin for audit/mirroring parity with the
-- wire submission.
ALTER TABLE submissions   ADD COLUMN origin TEXT;

-- The CURRENT-VIEW table is what pulls actually read (bucket / bucket_delta
-- select FROM repo_mappings). It must carry origin too, or pulls could never
-- surface it. NULL = manual = honest; pre-existing rows read as manual.
ALTER TABLE repo_mappings ADD COLUMN origin TEXT;

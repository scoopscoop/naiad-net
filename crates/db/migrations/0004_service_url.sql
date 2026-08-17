-- A shared service is bound to a repository URL it pulls from (README §4/§6).
-- NULL for the seeded local-only service, which has no network path.
ALTER TABLE services ADD COLUMN url TEXT;

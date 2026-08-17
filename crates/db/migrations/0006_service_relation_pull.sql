-- When a service last had its relation graph bulk-pulled (authoritative replace).
-- NULL = never relation-pulled. Stamped only by merge_pulled_relations; mapping
-- pulls deliberately do not touch it.
ALTER TABLE services ADD COLUMN last_relation_pull_at INTEGER;

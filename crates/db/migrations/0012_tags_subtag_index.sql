-- Tag completion (#22): prefix scan on subtag across all namespaces.
-- Namespace-scoped and namespace-name matches already use UNIQUE(namespace, subtag).
CREATE INDEX idx_tags_subtag ON tags (subtag);

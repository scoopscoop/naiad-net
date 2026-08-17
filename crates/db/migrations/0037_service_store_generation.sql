-- Per-service store-generation id (#194). Records the store_generation token
-- the client last successfully synced against. NULL until the server first
-- advertises one, or on a pre-#194 client talking to a pre-#194 server.
--
-- Nullable: a NULL value means "never seen a generation from this service";
-- the client only resets cursors when the stored value is non-NULL and differs
-- from the advertised one (first-sight just records; unchanged just carries on).
ALTER TABLE services ADD COLUMN store_generation TEXT;

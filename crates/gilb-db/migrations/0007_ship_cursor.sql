-- Shipper cursor: which actions have left the device for gilb-web.
-- NULL = not yet shipped. The gilb-shipper crate reads WHERE shipped_at IS NULL,
-- sends a JSONL batch to POST /api/v1/ingest, then sets shipped_at on ack.
-- Idempotent end-to-end: the server dedups by event_id (= actions.id), so a
-- retry of the same batch after an ambiguous/failed send is safe.
--
-- Additive nullable column + a partial index over the unshipped set (the
-- shipper's hot query). Second consumer apps/gilb-mcp is unaffected (it does
-- not read shipped_at).

ALTER TABLE actions ADD COLUMN shipped_at TEXT;
CREATE INDEX idx_actions_unshipped ON actions(shipped_at) WHERE shipped_at IS NULL;

-- Enrich `actions` clipboard rows with an operation kind and a content hash.
-- Both columns are nullable/additive and set only on `kind = 'clipboard'` rows,
-- so existing rows and all other kinds stay NULL (backward-compatible).
--
-- `clipboard_op` — "copy"/"cut"/"paste". Step-1 always "copy": the macOS
--   pasteboard does not distinguish copy from cut, so cut/paste detection is
--   deferred to a later card.
-- `content_hash` — sha256 (hex) of the raw clipboard text BEFORE PII
--   redaction, so copy↔paste linking still works when the shipped
--   `text_content` has been redacted by `redact_pii`.
--
-- Second consumer: apps/gilb-mcp reads `actions` (see help.md). The new
-- columns are nullable and documented there; no existing query breaks.

ALTER TABLE actions ADD COLUMN clipboard_op TEXT;
ALTER TABLE actions ADD COLUMN content_hash TEXT;

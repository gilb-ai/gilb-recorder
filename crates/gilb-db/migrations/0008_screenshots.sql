-- Sample screenshots captured on focus boundaries (GILB-80). The encoded image
-- bytes live on disk (`image_path`); this table holds only metadata + the
-- shipper cursor (`shipped_at`, GILB-93 — mirrors actions.shipped_at from 0007).
--
-- `screenshot_id` is a stable id the server links to nearby actions. Rows are
-- opt-in (only written when CAPTURE_SCREENSHOTS is on) and never contain the
-- image itself. Second consumer apps/gilb-mcp does not read this table.

CREATE TABLE screenshots (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id    INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    captured_at   TEXT    NOT NULL,
    app_bundle_id TEXT,
    app_name      TEXT,
    window_title  TEXT,
    screenshot_id TEXT    NOT NULL,
    image_path    TEXT    NOT NULL,
    width         INTEGER NOT NULL,
    height        INTEGER NOT NULL,
    shipped_at    TEXT
);

-- Hot query for the shipper: the unshipped set (GILB-93).
CREATE INDEX idx_screenshots_unshipped ON screenshots(shipped_at) WHERE shipped_at IS NULL;

-- Add browser_url to tree_snapshots so SPA navigation in the same window
-- is distinguishable in the persisted tree (Gmail / Linear / Crunchbase
-- keep window_title stable across URL transitions).
--
-- Mirrors the actions.browser_url column added in 0002_browser_url.sql;
-- populated by the same frontmost_app_with_window path.

ALTER TABLE tree_snapshots ADD COLUMN browser_url TEXT;

CREATE INDEX idx_tree_snapshots_browser_url
    ON tree_snapshots(browser_url)
    WHERE browser_url IS NOT NULL;

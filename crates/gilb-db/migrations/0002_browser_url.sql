-- Add the URL of the focused browser tab to each captured action.
-- Populated by the macOS recorder when the foreground app is one of the
-- known browsers (Chrome, Safari, Firefox, Edge, Brave, Arc, …); NULL
-- otherwise. Sourced from AXDocument on the focused window, with a
-- shallow AXTextField / AXComboBox walk as fallback.
--
-- See prior-art/src/tree/macos.rs `extract_browser_url` for the
-- reference implementation we adapted.

ALTER TABLE actions ADD COLUMN browser_url TEXT;

CREATE INDEX idx_actions_browser_url
    ON actions(browser_url)
    WHERE browser_url IS NOT NULL;

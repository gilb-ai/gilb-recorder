-- gilb v0 schema — minimal, multimodal tables added in later migrations.

CREATE TABLE sessions (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    started_at      TEXT    NOT NULL,
    stopped_at      TEXT,
    gilb_version    TEXT    NOT NULL,
    host_os         TEXT    NOT NULL,
    stop_reason     TEXT
);

CREATE INDEX idx_sessions_started_at ON sessions(started_at);

CREATE TABLE tree_snapshots (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id      INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    captured_at     TEXT    NOT NULL,
    app_bundle_id   TEXT,
    app_name        TEXT,
    window_title    TEXT,
    simhash         INTEGER NOT NULL,
    root_json       TEXT    NOT NULL
);

CREATE INDEX idx_tree_snapshots_session_time
    ON tree_snapshots(session_id, captured_at);
CREATE INDEX idx_tree_snapshots_simhash
    ON tree_snapshots(simhash);

CREATE TABLE actions (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id        INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    captured_at       TEXT    NOT NULL,
    kind              TEXT    NOT NULL,
    app_bundle_id     TEXT,
    app_name          TEXT,
    app_pid           INTEGER,
    window_title      TEXT,
    element_role      TEXT,
    element_name      TEXT,
    element_value     TEXT,
    element_help      TEXT,
    element_id        TEXT,
    element_frame     TEXT,
    text_content      TEXT,
    password_flag     INTEGER NOT NULL DEFAULT 0 CHECK (password_flag IN (0, 1)),
    tree_snapshot_id  INTEGER REFERENCES tree_snapshots(id) ON DELETE SET NULL,
    extra_json        TEXT
);

CREATE INDEX idx_actions_session_time
    ON actions(session_id, captured_at);
CREATE INDEX idx_actions_kind_time
    ON actions(kind, captured_at);
CREATE INDEX idx_actions_app_time
    ON actions(app_bundle_id, captured_at);

CREATE TABLE app_budgets (
    bundle_id     TEXT PRIMARY KEY,
    tier          TEXT NOT NULL DEFAULT 'moderate',
    walks_per_min INTEGER NOT NULL DEFAULT 30,
    last_updated  TEXT NOT NULL
);

CREATE TABLE health_events (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    captured_at   TEXT    NOT NULL,
    kind          TEXT    NOT NULL,
    details_json  TEXT
);

CREATE INDEX idx_health_events_time
    ON health_events(captured_at);

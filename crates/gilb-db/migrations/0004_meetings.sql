-- Meetings recorded alongside a11y capture (video/audio call sessions).
-- A meeting groups the actions captured while it was active; the optional
-- actions.meeting_id back-link lets a recorded action be attributed to the
-- meeting it happened during. Timestamps are epoch-ms (INTEGER).
-- video_path / audio_path point at the on-disk recordings, NULL until/unless
-- the corresponding stream is captured.

CREATE TABLE meetings (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    started_at  INTEGER NOT NULL,
    ended_at    INTEGER,
    app         TEXT    NOT NULL,
    video_path  TEXT,
    audio_path  TEXT,
    status      TEXT    NOT NULL
                        CHECK (status IN ('recording', 'completed', 'cancelled', 'failed'))
                        DEFAULT 'recording'
);

CREATE INDEX idx_meetings_started_at ON meetings(started_at);

ALTER TABLE actions
    ADD COLUMN meeting_id INTEGER REFERENCES meetings(id) ON DELETE SET NULL;

CREATE INDEX idx_actions_meeting_id ON actions(meeting_id);

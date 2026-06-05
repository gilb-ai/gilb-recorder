-- Batch transcript for a finished meeting, 1:1 with `meetings`. Produced by
-- gilb-transcribe by uploading the meeting's `.audio.wav` to the OpenAI Whisper
-- API after the recording finalizes.
--
-- `text` / `segments_json` hold the transcription on success (segments_json is
-- the verbose_json `segments` array, stored inline). `error` holds the last
-- failure message after retries are exhausted; on success it stays NULL. A row
-- is keyed by meeting_id (PRIMARY KEY) and dropped with its meeting.

CREATE TABLE meeting_transcripts (
    meeting_id    INTEGER PRIMARY KEY
                          REFERENCES meetings(id) ON DELETE CASCADE,
    text          TEXT,
    segments_json TEXT,
    model         TEXT,
    created_at    INTEGER NOT NULL,
    error         TEXT
);

-- Extra Discord messages mirroring one Slack message, so edits and deletes
-- reach all of them: the 2nd+ parts of a message split for Discord's
-- 2000-char limit ('part'), and the main-channel copy of an "also send to
-- channel" thread reply ('broadcast').
CREATE TABLE IF NOT EXISTS message_extras (
    slack_channel_id  TEXT NOT NULL,
    slack_ts          TEXT NOT NULL,
    role              TEXT NOT NULL,
    idx               INTEGER NOT NULL,
    discord_msg_id    TEXT NOT NULL,
    discord_thread_id TEXT,
    PRIMARY KEY (slack_channel_id, slack_ts, role, idx)
);

-- Slack files uploaded for a Discord message (comma-separated ids), so
-- deleting it on Discord also deletes the files on Slack.
ALTER TABLE messages ADD COLUMN slack_file_ids TEXT;

-- Uploads used to be mapped under an empty ts; those rows are unusable.
DELETE FROM messages WHERE slack_ts = '';

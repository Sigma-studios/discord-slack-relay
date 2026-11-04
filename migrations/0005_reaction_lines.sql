-- The Slack reaction line last written on a message's Discord copy (see
-- src/reactions.rs), so unchanged reactions skip the edit and Slack edits of
-- the message keep the line.
CREATE TABLE IF NOT EXISTS reaction_lines (
    slack_channel_id TEXT NOT NULL,
    slack_ts         TEXT NOT NULL,
    line             TEXT NOT NULL,
    PRIMARY KEY (slack_channel_id, slack_ts)
);

-- Last topic written to each mirrored Discord channel, so topic syncs only edit
-- the channel when it actually changed (Discord rate-limits channel edits hard).
CREATE TABLE IF NOT EXISTS channel_topics (
    slack_channel_id TEXT PRIMARY KEY,
    topic            TEXT NOT NULL
);

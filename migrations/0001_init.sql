-- Channel pairing: one row per mirrored Slack channel.
-- Membership of the bot in the Slack channel is the source of truth; a row
-- exists while the bot is in that Slack channel.
CREATE TABLE IF NOT EXISTS channel_pairs (
    slack_channel_id   TEXT PRIMARY KEY,
    slack_channel_name TEXT NOT NULL,
    discord_channel_id TEXT NOT NULL,
    -- Webhook used to post Slack→Discord with per-sender name/avatar.
    discord_webhook_id    TEXT NOT NULL,
    discord_webhook_token TEXT NOT NULL,
    is_private         INTEGER NOT NULL DEFAULT 0,
    archived           INTEGER NOT NULL DEFAULT 0,
    created_at         TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Message id mapping, for edits, deletes, and reply/thread resolution.
CREATE TABLE IF NOT EXISTS messages (
    slack_channel_id TEXT NOT NULL,
    slack_ts         TEXT NOT NULL,          -- Slack message timestamp (its id)
    discord_msg_id   TEXT NOT NULL,
    -- 'slack' = originated in Slack, mirrored to Discord;
    -- 'discord' = originated in Discord, mirrored to Slack.
    origin           TEXT NOT NULL,
    PRIMARY KEY (slack_channel_id, slack_ts)
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_messages_discord ON messages(discord_msg_id);

-- Thread mapping: Slack thread_ts ↔ Discord thread channel id.
CREATE TABLE IF NOT EXISTS threads (
    slack_channel_id  TEXT NOT NULL,
    slack_thread_ts   TEXT NOT NULL,
    discord_thread_id TEXT NOT NULL,
    PRIMARY KEY (slack_channel_id, slack_thread_ts)
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_threads_discord ON threads(discord_thread_id);

-- Dedup guard: ts of messages WE posted into Slack via the user token, so the
-- resulting Slack event is not mirrored back to Discord (loop prevention).
CREATE TABLE IF NOT EXISTS self_posted_slack (
    slack_ts   TEXT PRIMARY KEY,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Discord user ↔ Slack user alias, for tagging across the bridge.
CREATE TABLE IF NOT EXISTS user_aliases (
    alias            TEXT PRIMARY KEY,       -- e.g. 'alice', used as @d:alice / @s:alice
    discord_user_id  TEXT,
    slack_user_id    TEXT
);

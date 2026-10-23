-- Thread context per mirrored message, needed to edit/delete webhook messages
-- that live inside a Discord thread, and to route Discord replies into the
-- right Slack thread.
ALTER TABLE messages ADD COLUMN slack_thread_ts TEXT;
ALTER TABLE messages ADD COLUMN discord_thread_id TEXT;

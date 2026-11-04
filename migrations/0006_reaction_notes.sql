-- Messages written on Discord can't be edited by the bot, so their Slack
-- reactions live in a bot message ("note") in the message's Discord thread.
-- created_thread: the relay opened that thread just for the note, so it is
-- removed again once the note goes and nobody else wrote in it.
ALTER TABLE reaction_lines ADD COLUMN note_msg_id TEXT;
ALTER TABLE reaction_lines ADD COLUMN note_channel_id TEXT;
ALTER TABLE reaction_lines ADD COLUMN created_thread INTEGER NOT NULL DEFAULT 0;

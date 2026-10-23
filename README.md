# discord-slack-relay

Two-way relay between Slack channels you're a member of and your own Discord
server. Slack channel membership of the bot is the source of truth: add the bot
to a Slack channel and a matching Discord text channel is created; remove it and
mirroring stops.

- **Slack → Discord**: real-time via Socket Mode. Each Slack sender appears in
  Discord under their own name/avatar (posted through a per-channel webhook).
  Threads become Discord threads; media is re-uploaded (link fallback > ~19 MB).
- **Discord → Slack**: posted by the bot under the Discord sender's name and
  avatar (e.g. `alice (Discord)`), carrying the app's APP badge.
- **Tagging**: `@d:alias` in Slack pings a Discord user; `@s:alias` in Discord
  pings a Slack user. Register aliases from Discord:
  - `!alias <name>` — map `<name>` to your Discord account
  - `!slackalias <name> <U0123>` — map `<name>` to a Slack user id

## Status

Fully wired and building against slack-morphism 2.31 / serenity 0.12:

- Socket Mode push callbacks: message, member_joined/left, channel_rename,
  channel_archive/unarchive.
- Slack→Discord: name + avatar via the event profile (users.info fallback),
  thread mirroring, inline media re-upload (≤ ~19 MB), mention/markup rewrite.
- Discord→Slack: posts via the bot under the sender's name/avatar, thread
  replies, attachment re-upload via Slack's external-upload flow, alias pings.
- Loop prevention: the relay's own posts are dropped by their Slack bot id.

Not yet implemented (intentionally): edit/delete propagation, reactions, DMs,
history backfill. `cargo test` covers the markup conversions.

**Untested against live services.** It compiles and the pure logic is tested,
but nothing exercising real Slack/Discord tokens has been run — expect to shake
out runtime issues (intents, scope gaps, thread edge cases) during setup.

## Slack app setup (internal app)

1. Create an app at <https://api.slack.com/apps> in your workspace.
2. **Socket Mode**: enable it; create an app-level token with `connections:write`
   → `SLACK_APP_TOKEN`.
3. **Bot token scopes**: `channels:history`, `groups:history`, `channels:read`,
   `groups:read`, `users:read`, `files:read`, `chat:write`,
   `chat:write.customize`, `files:write`, `pins:read`. Install → `SLACK_BOT_TOKEN`.
4. **Event subscriptions** (bot events): `message.channels`, `message.groups`,
   `member_joined_channel`, `member_left_channel`, `channel_rename`,
   `channel_archive`, `channel_unarchive`, `channel_deleted`, `group_rename`,
   `group_archive`, `group_unarchive`, `group_deleted`, `pin_added`, `pin_removed`.
5. Invite the bot to a channel: `/invite @yourbot`.

Tip: the **App Manifest** page sets all of the above (scopes, Socket Mode,
events) in one save — see `deploy/manifest.yaml`.

## Discord setup

1. Create a bot (<https://discord.com/developers>), enable the **Message Content**
   intent.
2. Invite it to your server with **Manage Channels**, **Manage Webhooks**,
   **Send Messages**, **Create Public Threads**, **Pin Messages** (for mirrored
   pins).
3. Put the server id in `DISCORD_GUILD_ID`, the token in `DISCORD_BOT_TOKEN`.

## Run locally

```sh
cp .env.example .env   # fill in tokens
cargo test             # format conversions
cargo run
```

## Deploy on façade

```sh
facade app create relay --git <this-repo> --volume data:/data
# set secrets (encrypted by default):
facade env set relay SLACK_BOT_TOKEN=... SLACK_APP_TOKEN=... \
  DISCORD_BOT_TOKEN=... DISCORD_GUILD_ID=... DATABASE_URL='sqlite:///data/relay.db?mode=rwc'
facade deploy relay
```

No inbound ports are needed — both Slack Socket Mode and the Discord gateway are
outbound websockets. The Dockerfile `HEALTHCHECK` watches the heartbeat file on
the volume, so a wedged process fails its health gate.

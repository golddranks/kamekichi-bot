# discord-bot

kamekichi-ws: [![Full Coverage](https://github.com/golddranks/kamekichi-bot/actions/workflows/coverage.yml/badge.svg)](https://github.com/golddranks/kamekichi-bot/actions/workflows/coverage.yml)

A minimal Discord bot written in Rust with no async runtime and no Discord SDK.

## Features

- **Managed messages** — post and auto-edit messages defined in config (channel readmes, rules, etc.)
- **Reaction roles** — assign/remove roles when users react to messages
- **Channel logging** — append all messages, edits, deletes, and reactions to a log file
- **Config hot-reload** — edit config.json while the bot is running; changes apply within 5 seconds

## Requirements

- Rust + Cargo
- A Discord account

## Setup

### 1. Create a Discord application and bot

1. Go to https://discord.com/developers/applications
2. Click **New Application**, give it a name
3. Go to **Bot** in the left sidebar
4. Click **Reset Token**, copy the token

### 2. Enable privileged intents

In the Developer Portal under **Bot → Privileged Gateway Intents**, enable:
- **Message Content Intent** (required for logging message content)

### 3. Invite the bot to your server

1. Go to **OAuth2 → URL Generator**
2. Under **Scopes**, check `bot`
3. Under **Bot Permissions**, check:
   - **Send Messages**
   - **Manage Roles**
   - **Read Message History**
4. Open the generated URL and select your server

### 4. Configure the bot

```
cp config.json.example config.json
```

Edit `config.json`:

```json
{
  "token": "YOUR_BOT_TOKEN",
  "log_file": "bot.log",
  "managed_messages": [
    {
      "slug": "welcome",
      "channel_id": "CHANNEL_ID",
      "content": "Welcome to the server!"
    }
  ],
  "reaction_roles": [
    {
      "guild_id": "SERVER_ID",
      "message_id": "MESSAGE_ID",
      "emoji": "✅",
      "role_id": "ROLE_ID"
    }
  ]
}
```

All fields except `token` are optional.

### 5. Build and run

```
cargo run
```

## Config reference

| Field | Description |
|---|---|
| `token` | Bot token from the Developer Portal |
| `log_file` | Path to append event logs (omit to disable logging) |
| `managed_messages` | Messages the bot posts and keeps in sync with config |
| `reaction_roles` | Emoji → role mappings on specific messages |

### Managed messages

Each entry has a `slug` (unique name), `channel_id`, and `content`. On startup, the bot posts the message if new, or edits it if the content changed. Message IDs are tracked in `state.dat`.

Edit the content in config.json while the bot is running — it will update the Discord message automatically.

### Reaction roles

To get IDs: right-click the server → **Copy Server ID**, right-click a message → **Copy Message ID**, right-click a role in Server Settings → **Copy Role ID**.

The bot's role must be positioned above any roles it manages in **Server Settings → Roles**.

## Log format

Tab-separated, one event per line:

```
<unix_timestamp>\t<event_type>\t<fields...>
```

Event types: `MESSAGE`, `EDIT`, `DELETE`, `REACTION_ADD`, `REACTION_REMOVE`.

## Notes

- The bot reconnects automatically on gateway errors
- No public IP or open ports required — outbound connections only
- `state.dat` tracks managed message IDs — don't delete it while the bot is running

# Security

## Who can talk to the bot

A Telegram bot is publicly discoverable, so Optimimer is **deny-by-default**. There are three ways in:

- **Setup code** — printed by the installer (or generated and logged on first start). The first chat to send it is
  registered as the **owner**. The code is single-use.
- **Invite code** — the owner asks the bot to "invite someone"; it mints a single-use code. Whoever sends it is
  registered as a **member**: they get their own name and timezone, use the owner's categories, and cannot invite,
  change categories or remove people. "Remove <name>" revokes a member.
- **Static allowlist** — `TELEGRAM_ALLOWED_CHAT_IDS` (comma-separated chat ids) still works, for installs from before
  pairing existed. While no owner is paired, an allowlisted chat counts as the owner.

Anyone else gets a one-line hint and nothing more; the attempt is logged with the chat id.

Paired chats live in the `chats` table of the SQLite database; delete a row to revoke, or say "remove <name>".

## What the network sees

- Backend ↔ Telegram is TLS. Updates arrive by **long polling**, so the Pi opens **no inbound port**.
- Bot chats are **not end-to-end encrypted** and cannot be — Telegram bots cannot use Secret Chats, so Telegram's
  servers see message content. Each leg (phone ↔ Telegram, Telegram ↔ Pi) is encrypted in transit.
- LLM calls go to OpenRouter over TLS; message text, voice transcripts and receipt photos are sent there for
  processing. Pick models you are comfortable with in [Configuration](configuration.md).
- The HTTP API (agent editor) binds to `127.0.0.1` only. Reach the web UI over an SSH tunnel:
  `ssh -L 8799:localhost:8799 user@pi`, then open http://localhost:8799.

## Secrets on disk

`optimimer.env` holds the bot token and API key, readable only by root and the service user. Nothing requires a
secret manager; if you use one, have it export the variables before `optimimer-setup` (which then asks nothing) or
add a systemd drop-in that injects them. Environment variables always win over the file and over stored settings.

## What is personal, and where it lives

Nothing about the owner is compiled into the binary or checked into the repository. Your name, timezone,
categories, machines, voice vocabulary, paired chats, ledger, memory graph and conversation history are all in the
SQLite file on the Pi; tasks, notes and summaries are Markdown files in your vault. Back up those two things and you
have everything.

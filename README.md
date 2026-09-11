# Optimimer

A private assistant you talk to on **Telegram**, running on your own **Raspberry Pi**. It files tasks, notes and
voice memos into your **Obsidian** vault, keeps a money ledger, remembers what you tell it, sets reminders, manages
shopping lists, watches product pages, wakes your desktop over the network, and summarises forwarded chats — with no
slash commands. You say what you want; a tool-calling model does it — OpenRouter's free models by default, Claude if
you add credits.

Everything personal stays in two things you own: the vault (Markdown + frontmatter, queryable with Obsidian Bases)
and one SQLite file on the Pi. Nothing about you is in this repository or the binary.

## Install

Prebuilt for Raspberry Pi and Debian (`.deb`), Arch (`PKGBUILD`), Docker, macOS and Windows — no toolchain needed.
On a Pi, with a bot token from [@BotFather](https://t.me/BotFather) and an [OpenRouter](https://openrouter.ai) key
at hand:

```sh
curl -LO https://github.com/arsalikhov/optimimer/releases/latest/download/optimimer-arm64.deb
sudo apt install ./optimimer-arm64.deb
sudo optimimer-setup
```

Setup asks three questions (timezone, bot token, API key), starts a systemd service, and prints a **setup code**.
Send that code to your bot from Telegram: you are paired as the owner and the bot asks your name, timezone and
categories. No ports are opened; the bot long-polls Telegram. Other platforms: [Install](docs/install.md).

Then just talk to it: "remind me to call the dentist tomorrow at 9", "spent 12.50 on lunch", "note: ideas for the
trip", or forward a conversation and get a summary.

## Documentation

- [Install](docs/install.md) — updating, secrets, Obsidian sync, uninstalling
- [Using the bot](docs/using-the-bot.md) — onboarding, everything you can ask, voice, forwards, receipts
- [The vault](docs/vault.md) — Obsidian layout, Bases views, dashboard, charts
- [Finance](docs/finance.md) — ledger, CSV import, balance
- [Memory](docs/memory.md) — bounded context and the knowledge graph
- [Security](docs/security.md) — pairing and invites, what Telegram can see
- [Configuration](docs/configuration.md) — every variable and stored setting
- [Development](docs/development.md) — architecture, running locally, cross-compiling, the API

## Stack

Rust (axum, tokio, rusqlite) as one static binary; SvelteKit flow editor for the small workflow agents; OpenRouter
for models; Telegram Bot API by long polling; Obsidian for everything you want to read.

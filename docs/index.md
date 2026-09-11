# Optimimer

A private assistant you talk to on **Telegram**, running on your own **Raspberry Pi**. It files tasks, notes and
voice memos into your **Obsidian** vault, keeps a money ledger, remembers what you tell it, sets reminders, manages
shopping lists, watches product pages, wakes your desktop over the network, and summarises forwarded chats.

There are no slash commands. You say what you want; a tool-calling model does it.

## Get started

1. [Install](install.md) — one command on the Pi, three questions, done.
2. Send the setup code to your bot from Telegram. It pairs you as the owner and asks your name, timezone and
   categories.
3. [Talk to it](using-the-bot.md).

## Guides

| Page | What it covers |
| ---- | -------------- |
| [Install](install.md) | One-line install, updating, secrets, uninstalling |
| [Using the bot](using-the-bot.md) | Onboarding, everything you can ask for, voice, forwards, receipts |
| [The vault](vault.md) | Obsidian setup, sync, Bases views, dashboard, charts |
| [Finance](finance.md) | Ledger, CSV import, receipts, balance, transfers |
| [Memory](memory.md) | How it remembers without bloating the context window |
| [Security](security.md) | Pairing, invites, what Telegram can see, network exposure |
| [Configuration](configuration.md) | Every environment variable and stored setting |
| [Development](development.md) | Architecture, running locally, cross-compiling, the HTTP API |

## Design in one paragraph

Everything personal lives in two places you own: an **Obsidian vault** (Markdown with frontmatter, so Obsidian
Bases can query it) and one **SQLite** file on the Pi (ledger, memory graph, settings, paired chats). The Rust
backend is a single static binary; LLM calls go to OpenRouter. Nothing about you is in the source or the binary —
the bot collects it during setup.

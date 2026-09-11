# Optimimer

A Lindy.ai-style AI agent builder. Design agents on a visual flow canvas, then run them — primarily from
**Telegram**, or from the web UI.

- **Frontend** — SvelteKit + `@xyflow/svelte` visual flow editor (run with **bun**).
- **Backend** — Rust (axum). Workflow CRUD, a graph execution engine, and a Telegram bot.
- **LLM** — AI Step nodes call models via **OpenRouter**.

## Blocks

| Block       | Does                                                                       |
| ----------- | -------------------------------------------------------------------------- |
| ⚡ Trigger   | Entry point. Its payload is available downstream as `{{input}}`.           |
| 🧠 AI Step  | Calls an LLM via OpenRouter. Reference earlier nodes with `{{nodeId.text}}`. |
| 🌐 HTTP     | Calls an external API. URL/body support `{{templating}}`.                  |
| 🔀 Condition | Branches the flow down the `true` / `false` handle.                        |
| ⏰ Schedule  | Send a Telegram message at a future time.                                  |
| 📤 Output   | Final result of a run (what Telegram replies with).                        |

Any string field supports `{{input}}`, `{{input.field}}`, `{{nodeId}}`, `{{nodeId.field}}` interpolation.

## Prerequisites

- [**Rust**](https://rustup.rs) (stable) — the backend.
- [**Bun**](https://bun.sh) — the frontend.
- An [**OpenRouter**](https://openrouter.ai) API key for real LLM calls (optional — without it, AI Steps
  return clearly-labelled `[mock:...]` output so everything still runs).

## Configuration

The backend reads its config from environment variables. Pick one of two ways to supply them:

### Option A — `.env` file (simplest)

```sh
cp backend/.env.example backend/.env
# edit backend/.env and fill in your tokens
```

`main.rs` loads it with `dotenvy` on startup. **This is the easy path, but less secure than a secret
manager:** your tokens sit in plaintext on disk. `backend/.env` is gitignored so it's never committed —
keep it that way, and prefer Option B for anything shared or long-lived.

### Option B — Infisical (more secure, nothing on disk)

Secrets live in [Infisical](https://infisical.com)'s **dev** environment and are injected at launch, so no
plaintext file sits on disk:

```sh
infisical secrets set OPENROUTER_API_KEY=... --env=dev   # repeat per key
bun run dev:secure                                       # = infisical run --env=dev -- bun run scripts/dev.ts
```

`backend/.env.example` is the reference list of every key the app understands.

The core keys: `TELEGRAM_BOT_TOKEN`, `OPENROUTER_API_KEY`, and `VAULT_DIR` (where the Markdown files go).
Everything personal — your name, timezone, categories, machines, who may talk to the bot — is **not** configured
here: it is collected by the bot itself during setup and stored in its SQLite database, so the source and the
shipped binary contain nothing about you.

### Security — who can use the bot

A Telegram bot is publicly discoverable, so the bot is **deny-by-default** and access works by **pairing**:

1. On start (or from `deploy/install.sh`) the bot prints a one-time **setup code** like `K7QD-3MXP` (set your own
   with `OPTIMIMER_SETUP_CODE`, or let it generate one).
2. Send that code to the bot from Telegram. That chat becomes the **owner**, and the bot walks you through setup
   (name, timezone, categories, an optional machine to wake). Everything else is refused with a short hint.
3. To let someone else in, tell the bot "invite someone": it mints a one-time **invite code**; the newcomer sends it
   and is paired as a member (their own name and timezone, but the owner's categories). "Remove <name>" revokes.

Paired chats live in the `chats` table; `TELEGRAM_ALLOWED_CHAT_IDS` still works as a static allowlist for old
installs. The HTTP API binds to `127.0.0.1` by default (override with `OPTIMIMER_BIND`), so it isn't exposed on your
LAN; reach the web UI over an SSH tunnel if you need it remotely. Transport to Telegram is TLS, but bot chats are not
end-to-end encrypted (Telegram's servers see message content — inherent to the Bot API).

Finance commands write to a `transactions` table in the SQLite db; `RENT_AMOUNTS` (comma-separated exact amounts,
off by default) enables the rent-detection rule. See `backend/.env.example` for every key the app understands.

### The vault — notes, tasks and summaries as Markdown

Tasks, notes and the conversation summaries are written as `.md` files with YAML frontmatter under `VAULT_DIR`
(`notes/`, `tasks/`, `summaries/`). The folder is meant to be an **Obsidian** vault: point `VAULT_DIR` at the folder
Obsidian Sync (or Syncthing) keeps on the Pi and every capture shows up on all your devices a moment later. Every
property the bot writes (`type`, `status`, `priority`, `due`, `project`, `tags`, `created`, …) is plain frontmatter,
so Obsidian **Bases** can filter, sort and group them — "open tasks by project", "notes tagged x" — without
plugins. Completing a task flips its `status` to `done`; listing and searching read the same files, so edits you
make in Obsidian are what the bot sees. There is no cloud dependency: the bot only ever touches local files.

Every task and note carries a **`category`** from a fixed list (default: Admin, Work, Fitness, Home, Finance,
Learning, Social, Travel, Personal; you choose your own during setup, or override with `VAULT_CATEGORIES`) and an
optional **`project`** nested under it (Fitness → Triathlon). The parsers pick both and may create projects but never
categories; the bases group by category and the Gantt uses categories as sections with the project as a prefix.

To get the vault onto your other devices without running the desktop app on the Pi, use **Obsidian Headless**
(`npm install -g obsidian-headless`, needs Node 22+): `ob login`, `ob sync-setup --vault "<remote vault>" --path
/opt/optimimer/vault`, then run `ob sync --continuous` as a service — `deploy/obsidian-sync.service` is a template
unit that the installer offers to fill in and enable for you. It uses your Obsidian Sync subscription and the same
end-to-end encryption as the app.

**Finances in Obsidian.** Every ledger row is mirrored as a small note in `finance/` (`type: transaction`, `date`,
`amount`, `direction`, `category`, `source`, `flagged`), and the bot drops a `Finances.base` into the vault root on
first run with views for this month, by month (income / spent / net sums), spending by category, transfers and
flagged duplicates. SQLite stays the source of truth: the mirror is rebuilt on start for any missing rows, and
deleting a transaction removes the note too, but edits made to those notes in Obsidian do not flow back.
Set `FINANCE_PLACEHOLDERS=1` to seed an empty ledger with labelled sample rows ("Sample · …", source
`Placeholder`) for the last three months so every view renders; they are purged the moment a real expense, receipt
or CSV row arrives.

**Starter views.** On first run the bot also drops `Tasks.base`, `Notes.base`, `Summaries.base` and a `Home.md`
dashboard (which embeds one view from each with `![[Tasks.base#Today]]`-style embeds) into the vault root. They are
written only if missing, so edit them freely. The Tasks "Board" kanban view needs Obsidian 1.14+.

**Charts.** With the community **Charts** plugin installed, `Charts.md` (regenerated by the bot after every ledger
change) shows income vs spending and net by month, spending by category for the latest month with data, a stacked
category trend over six months, and daily spend for the last 30 days. `Home.md` embeds the first two.

## Run it (development)

```sh
bun run setup        # installs frontend deps

bun run dev          # starts BOTH services, reads backend/.env if present
# or, with Infisical-managed secrets:
bun run dev:secure
```

Either command starts both services with colour-prefixed logs: backend on http://localhost:8799, frontend
on http://localhost:5173.

## Build it (production)

```sh
bun run build
# = cd frontend && bun run build   (static site -> frontend/build/)
#   cd backend  && cargo build --release   (binary -> backend/target/release/optimimer-backend)
```

Run the release backend directly — it needs only its env vars and a writable working directory for its embedded
SQLite database (`optimimer.db`, override with `OPTIMIMER_DB`):

```sh
cd backend && OPENROUTER_API_KEY=... TELEGRAM_BOT_TOKEN=... ./target/release/optimimer-backend
```

The Telegram bot uses **long-polling**, so the backend needs **no open ports and no public IP** — it dials
out to Telegram and OpenRouter. That makes it cheap to self-host on any always-on box.

### Deploy to a Raspberry Pi (or any aarch64 Linux)

The backend is a single self-contained binary, so a Pi 3 (or anything 64-bit) is plenty — the heavy LLM
work happens on OpenRouter's servers. A **prebuilt static aarch64 binary ships in
[`deploy/bin/`](deploy/bin/)**, and the whole install is one `make` target:

```sh
make deploy PI=pi@<your-pi-ip>
```

That cross-compiles a fresh binary if you have a Rust toolchain (otherwise it ships the prebuilt one),
copies it plus `agents/` to `/opt/optimimer`, then runs an **interactive installer over SSH** that asks for
your **timezone, Telegram bot token, and OpenRouter API key**, writes a private `optimimer.env` (chmod 600),
installs a systemd service that auto-starts on boot and restarts on crash, and finally prints the **setup code**
to send to your bot. The bot is long-polling, so **no ports are opened** on the Pi.

To cross-compile a fresh binary yourself you need the toolchain once on an x86_64 dev machine:

```sh
rustup target add aarch64-unknown-linux-musl
cargo install cargo-zigbuild          # also needs `zig` on PATH
make build-pi                          # refreshes deploy/bin/optimimer-backend-aarch64
```

After the first deploy, ship code/agent changes without re-running the installer or re-entering config:

```sh
make redeploy PI=pi@<ip>       # cross-compiles, copies binary + agents/, restarts the service
```

Other Makefile targets: `make logs PI=…` (follow logs), `make restart PI=…`, `make stop PI=…`,
`make uninstall PI=…`. To re-run just the secret prompts later:
`ssh -t pi@<ip> /opt/optimimer/install.sh`.

**Secrets without typing them.** The installer auto-detects any of `TELEGRAM_BOT_TOKEN`, `OPENROUTER_API_KEY`,
`DEFAULT_TZ`, `VAULT_DIR`, `OPTIMIMER_SETUP_CODE`, `TELEGRAM_ALLOWED_CHAT_IDS`, `RENT_AMOUNTS` already present in
its environment and skips prompting for those. Two ways to feed them:

- **From your laptop's Infisical (the Pi needs no infisical CLI):** pass `INFISICAL_ENV` and `make` exports
  the secrets locally and injects them into the remote installer:

  ```sh
  make deploy PI=pi@<ip> INFISICAL_ENV=dev
  ```

- **From an infisical CLI on the Pi itself:** run the installer through it —

  ```sh
  ssh -t pi@<ip> 'infisical run --env=prod -- /opt/optimimer/install.sh'
  ```

  When the `infisical` CLI is present on the Pi, the installer also offers to set `ExecStart=infisical run -- …`
  so secrets are injected fresh at every service start and **never written to disk** (only non-secret config
  lands in the env file). Give the service a machine-identity `INFISICAL_TOKEN` via
  `sudo systemctl edit optimimer`.

At runtime the backend prefers real environment variables over the `.env` file, so an injected secret always
wins.

## Telegram (primary interface)

1. Create a bot with [@BotFather](https://t.me/BotFather), copy the token into `backend/.env`.
2. Start the backend. The log shows `Telegram bot started (long polling)` and, above it, your one-time setup code.
3. Message the bot and send the code. You're paired as the owner and the bot asks for your name, timezone,
   categories and (optionally) a machine to wake — each with a one-tap default. See
   [Security](#security--who-can-use-the-bot) for inviting others.

**Just talk to it.** There are no slash commands. Every message (typed or spoken) goes to one tool-calling model
(`AGENT_MODEL`, default Claude Sonnet 4.6) that acts through tools: create or complete tasks, save notes and memos,
log money and show balances, manage shopping lists, set reminders, send email, watch stock, wake a machine, and
change its own settings (your name, timezone, categories, machines, voice vocabulary, invites — "show settings"
lists them, "run setup again" repeats the onboarding). Anything that shows a table or list (balance, transactions,
task lists, search results) is displayed directly as a rich message; destructive actions (clearing a list, deleting a
transaction) get Yes/Cancel buttons.

**It remembers.** Every turn is stored, but the model only sees the last `MEMORY_WINDOW` turns plus a rolling summary
of everything older, so context never bloats. After each exchange a cheap model (`MEMORY_MODEL`) extracts people,
projects, places, preferences and facts into a small knowledge graph in SQLite (FTS-indexed nodes plus typed edges),
and the tasks/notes the bot creates join it too. The agent calls `recall` to look things up on demand, and `remember`
when you tell it something worth keeping. The graph is mirrored to `memory/` in the vault as wikilinked notes, so it
shows up in Obsidian's graph view and in `Memory.base`.

Two bundled workflow agents remain behind tools: `agents/cmd-notify.json` (reminders) and `cmd-email.json`; the
money and vault parsers (`cmd-spent`, `cmd-earned`, `cmd-todo`, `cmd-note`) turn free text into structured JSON.

### Conversation summaries

**Forward a batch of messages** (from any chat) and the bot replies with a title, key points, action items, and the
full transcript folded into a collapsible block. Add a comment when you forward — Telegram sends it just ahead of the
messages, and the bot uses it as the brief ("what did we decide about the venue?"). Without a comment, once the batch
settles the bot asks what it's about; reply with a note or tap *Summarize as-is*. Forwarded voice messages are
transcribed into the transcript. **A voice memo** of your own is transcribed and summarized the same way instead of
going to the agent when any of these hold: it opens with "memo" / "note to self" / "voice note", it runs
`VOICE_MEMO_SECS` (default 45s) or longer. Each summary is
written to `summaries/` in the vault (an index stays in SQLite so the agent can list them). Tuning: `CONVO_MODEL`,
`FORWARD_SETTLE_SECS`, `FORWARD_PEEK_SECS`, `FORWARD_NOTE_TTL_SECS`.

### Finance tracking

Logged expenses, income and CSV imports write to the SQLite `transactions` table; all money math (balances) is done in
Rust, not the LLM. CSV import detects the account type (credit-card vs chequing) from the filename/header, classifies
each row as Expense / Income / Transfer / Refund, de-duplicates by a date+amount+payee fingerprint, and never
double-counts card payments or salary. Manually logged entries are deduped the same way, and a same-amount near-match
within a few days is flagged (⚠️ in the transaction list) for you to review and remove. Repeat CSV
parses and receipt OCRs are cached, so re-importing the same file doesn't re-call the model. A few recurring payees are
pinned deterministically — rent → Housing (exact amounts from `RENT_AMOUNTS`, off by default), NSLSC → Loans,
Wealthsimple → Savings (excluded from the net), Amex bill payments → Transfer, ATM withdrawals → Cash.

### Migrating off Notion

Earlier versions stored everything in Notion. `scripts/notion_export.py` (standard library only) dumps every database
to JSON plus one Markdown file per page, for archiving. Run it wherever the old `NOTION_TOKEN` and `*_DB_ID` variables
still live, e.g. on the Pi: `set -a; . /opt/optimimer/optimimer.env; set +a; python3 notion_export.py --out
/opt/optimimer/notion-archive`.

## API

| Method | Path                       | Purpose                          |
| ------ | -------------------------- | -------------------------------- |
| GET    | `/api/workflows`           | list agents                      |
| POST   | `/api/workflows`           | create                           |
| GET/PUT/DELETE | `/api/workflows/:id` | read / update / delete           |
| POST   | `/api/workflows/:id/run`   | run (inline or stored workflow)  |

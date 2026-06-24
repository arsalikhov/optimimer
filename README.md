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
| 🗒️ Notion   | Query / create / update Notion pages (or save to disk when Notion is unset). |
| ⏰ Schedule  | Fire a Telegram ping and/or a Notion update at a future time.               |
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

The core keys: `TELEGRAM_BOT_TOKEN`, `OPENROUTER_API_KEY`, and (optional) `NOTION_TOKEN`.

### Security — who can use the bot

A Telegram bot is publicly discoverable, so the bot is **deny-by-default**: only chat ids in
`TELEGRAM_ALLOWED_CHAT_IDS` (comma-separated) may use it; everyone else is refused. On first run leave it unset,
message the bot once, and read the `unauthorized chat <id>` line in the logs to find your own id — then set the
var. The HTTP API also binds to `127.0.0.1` by default (override with `OPTIMIMER_BIND`), so it isn't exposed on
your LAN; reach the web UI over an SSH tunnel if you need it remotely. Transport to Telegram is TLS, but bot chats
are not end-to-end encrypted (Telegram's servers see message content — inherent to the Bot API).

Finance commands write to a Notion **Finances** database (`FINANCES_DB_ID`); `RENT_AMOUNTS` (default `1500,1700`)
tunes the rent-detection rule. See `backend/.env.example` for every key the app understands.

**Notion is optional.** With `NOTION_TOKEN` unset, the app skips Notion entirely and instead writes each
"save" action to a JSON file on disk (under `OPTIMIMER_NOTION_FALLBACK_DIR`, default `notion-out/` in the
working dir — e.g. `/opt/optimimer/notion-out/` on a Pi). So you get a working capture bot with zero Notion
setup; add the token later to sync to a real database.

The bundled `/notion`-style command agents target one Notion "Tasks" database — that's the author's own
setup, kept as a worked example. Adapt the property names in `agents/*.json` and the `CATEGORY_A` /
`CATEGORY_B` labels to your own life (e.g. `CATEGORY_A=Work`, `CATEGORY_B=Home`). See `backend/.env.example`
for the full list and the example schema.

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
your **timezone, Telegram bot token, and OpenRouter API key** (Notion token and category labels optional),
writes a private `optimimer.env` (chmod 600), and installs a systemd service that auto-starts on boot and
restarts on crash. The bot is long-polling, so **no ports are opened** on the Pi.

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

**Secrets without typing them.** The installer auto-detects any of `TELEGRAM_BOT_TOKEN`,
`TELEGRAM_ALLOWED_CHAT_IDS`, `OPENROUTER_API_KEY`, `NOTION_TOKEN`, `DEFAULT_TZ`, `CATEGORY_A`, `CATEGORY_B`,
`FINANCES_DB_ID`, `RENT_AMOUNTS` (and the other Notion database ids) already present in its environment and skips
prompting for those. Two ways to feed them:

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
2. Add your chat id to `TELEGRAM_ALLOWED_CHAT_IDS` (see [Security](#security--who-can-use-the-bot)) — the bot
   refuses everyone else.
3. Restart the backend. You'll see `Telegram bot started (long polling)`.

The bot registers a native "/" menu (`setMyCommands`). Most slash commands map to bundled agents in
`agents/cmd-*.json` (re-seeded from disk on every start, so edit the JSON to change behaviour); a few are handled
directly in the bot. Multi-word commands use underscores (Telegram only links `[a-z0-9_]`). Built-ins:

- **Agents** — `/agents` list · `/use <id>` pick the active agent, then send any message to run it as `{{input}}`.
- **Capture** — `/todo` (calendar-synced Notion task) · `/note` · `/complete` · `/notify` (reminder) · `/email`.
- **Search** — `/search_notes` · `/search_tasks` · `/list_todos` · `/list_notes` (5 newest).
- **Lists** — `/buy_later` (auto-sorts grocery vs other) · `/groceries` · `/clear_groceries` · `/to_buy` ·
  `/clear_to_buy`.
- **Money** — `/spent` · `/earned` · `/set_income` · `/balance` (a week: `last` / `N`, or a month: `/balance june`).

You can also **send a voice note** (transcribed, then routed to a command), a **receipt photo** (OCR'd into an
expense), or a **CSV bank statement** (bulk-imported, de-duplicated, AI-categorized). Replies use Telegram's
rich-message formatting (real tables, links) with a plain-text fallback.

### Finance tracking

`/spent`, `/earned` and CSV imports write to a Notion **Finances** database (`FINANCES_DB_ID`); all money math
(`/balance`) is done in Rust, not the LLM. CSV import detects the account type (credit-card vs chequing) from the
filename/header, classifies each row as Expense / Income / Transfer / Refund, de-duplicates by a
date+amount+payee fingerprint, and never double-counts card payments or salary. A few recurring payees are pinned
deterministically — rent → Housing (amounts from `RENT_AMOUNTS`), NSLSC → Loans, Wealthsimple → Savings (excluded
from the net), Amex bill payments → Transfer, ATM withdrawals → Cash.

## API

| Method | Path                       | Purpose                          |
| ------ | -------------------------- | -------------------------------- |
| GET    | `/api/workflows`           | list agents                      |
| POST   | `/api/workflows`           | create                           |
| GET/PUT/DELETE | `/api/workflows/:id` | read / update / delete           |
| POST   | `/api/workflows/:id/run`   | run (inline or stored workflow)  |

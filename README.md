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

Run the release backend directly — it needs only its env vars and a writable working directory for its
JSON state:

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

Other Makefile targets: `make logs PI=…` (follow logs), `make restart PI=…`, `make stop PI=…`,
`make uninstall PI=…`. To re-run just the secret prompts later:
`ssh -t pi@<ip> /opt/optimimer/install.sh`.

**Secrets without typing them.** The installer auto-detects any of `TELEGRAM_BOT_TOKEN`,
`OPENROUTER_API_KEY`, `NOTION_TOKEN`, `DEFAULT_TZ`, `CATEGORY_A`, `CATEGORY_B` already present in its
environment and skips prompting for those. Two ways to feed them:

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
2. Restart the backend. You'll see `Telegram bot started (long polling)`.
3. In the chat:
   - `/agents` — list your saved agents
   - `/use <id>` — pick the active agent
   - send any message — it becomes `{{input}}`; the agent's Output comes back as the reply

## API

| Method | Path                       | Purpose                          |
| ------ | -------------------------- | -------------------------------- |
| GET    | `/api/workflows`           | list agents                      |
| POST   | `/api/workflows`           | create                           |
| GET/PUT/DELETE | `/api/workflows/:id` | read / update / delete           |
| POST   | `/api/workflows/:id/run`   | run (inline or stored workflow)  |

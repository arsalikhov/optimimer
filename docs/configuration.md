# Configuration

Two layers:

- **Environment variables** — from `/etc/optimimer/optimimer.env` (package install) or `/opt/optimimer/optimimer.env`
  (clone install), both written by `optimimer-setup`; a `backend/.env` file in development; or whatever your
  secret manager exports. Secrets and machine-level paths live here. **Only two are required.**
- **Stored settings** — chosen during onboarding or by talking to the bot, kept in the `settings` table of the
  SQLite database. Personal preferences live here.

An environment variable always overrides the stored setting of the same meaning.

## Required

| Variable | Purpose |
| -------- | ------- |
| `TELEGRAM_BOT_TOKEN` | From @BotFather. Without it the bot does not start (the HTTP API still does). |
| `OPENROUTER_API_KEY` | All LLM calls and voice transcription. Unset, LLM steps return `[mock:…]` output. |

## Access and onboarding

| Variable | Default | Purpose |
| -------- | ------- | ------- |
| `OPTIMIMER_SETUP_CODE` | generated | One-time pairing code for the owner; logged on start until someone pairs. |
| `TELEGRAM_ALLOWED_CHAT_IDS` | empty | Optional static allowlist of chat ids on top of pairing. |

## Stored settings (and their env overrides)

- **`owner_name`** — how the assistant addresses you. Set by onboarding or "call me …".
- **`timezone`** (env `DEFAULT_TZ`) — default timezone; each chat can override its own. Set by onboarding,
  "my timezone is …" or a location pin.
- **`categories`** (env `VAULT_CATEGORIES`) — `Name: hint, Name: hint`: the fixed list every task and note is filed
  under, catch-all last. Set by onboarding or "change the categories to …" (confirmed with a button).
- **`machines`** (env `MACHINES`) — `name=MAC@interface,…` Wake-on-LAN targets; the interface defaults to `eth0`.
  Set by onboarding or "add machine …".
- **`transcribe_vocab`** (env `TRANSCRIBE_VOCAB`) — comma-separated words the transcriber should spell correctly.
  Set by "for voice notes, learn …".
- **`invite_code`** — the pending one-time member invite, minted by "invite someone".

## Vault

| Variable | Default | Purpose |
| -------- | ------- | ------- |
| `VAULT_DIR` | `vault` | Folder of Markdown files; point it at what Obsidian syncs. Relative to the working dir. |

## Finance

| Variable | Default | Purpose |
| -------- | ------- | ------- |
| `FINANCE_PLACEHOLDERS` | unset | `1` seeds an empty ledger with sample rows (purged on the first real entry). |
| `RENT_AMOUNTS` | unset | Exact e-transfer amounts to classify as rent → Housing. |
| `OCR_MODEL` | `anthropic/claude-sonnet-4.6` | Vision model for receipt photos. |
| `FINANCE_MODEL` | `anthropic/claude-sonnet-4.6` | Classifies CSV statement rows. |

## Models and behaviour

| Variable | Default | Purpose |
| -------- | ------- | ------- |
| `AGENT_MODEL` | `anthropic/claude-sonnet-4.6` | The conversational agent; must support tool calling. |
| `MEMORY_MODEL` | `anthropic/claude-haiku-4.5` | Extracts facts after each turn; folds old turns into the summary. |
| `MEMORY_WINDOW` | `12` | Recent turns the agent sees verbatim; older ones live in the rolling summary. |
| `CONVO_MODEL` | `anthropic/claude-sonnet-4.6` | Summaries of forwarded batches and voice memos. |
| `TRANSCRIBE_MODEL` | `mistralai/voxtral-small-24b-2507` | Voice transcription. |
| `SHOPPER_MODEL` | — | Reads product pages for stock watches. |
| `VOICE_MEMO_SECS` | `45` | Voice notes this long or longer are memos. |
| `FORWARD_SETTLE_SECS` | `4` | Quiet time after the last forward before a batch is summarised. |
| `FORWARD_PEEK_SECS` | `1` | Extra poll after a plain message to catch forwards behind it (`0` = off). |
| `FORWARD_NOTE_TTL_SECS` | `600` | How long "what are these about?" waits for a reply. |
| `SHOPPER_CHECK_SECS` | `3600` | Stock re-check interval. |
| `GANTT_REFRESH_SECS` | `600` | How often the task timeline is regenerated. |

## Email

| Variable | Purpose |
| -------- | ------- |
| `RESEND_API_KEY` | Bearer token for [Resend](https://resend.com). Until both are set, email is a mocked preview. |
| `EMAIL_FROM` | Verified sender, e.g. `Me <me@yourdomain.com>`. |

## Server and data

| Variable | Default | Purpose |
| -------- | ------- | ------- |
| `OPTIMIMER_DB` | `optimimer.db` | The SQLite file: settings, chats, ledger, memory, schedules, lists, cache. |
| `OPTIMIMER_BIND` | `127.0.0.1:8799` | HTTP API bind address. Only widen behind a trusted network or proxy. |
| `OPTIMIMER_AGENTS_DIR` | `agents` | Bundled `cmd-*.json` agents, re-seeded on start. |
| `RUST_LOG` | `info` | Log filter. |

`backend/.env.example` is the annotated reference copy of this list.

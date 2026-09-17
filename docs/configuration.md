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
| `OPENROUTER_API_KEY` | All LLM calls. A key with **no credits works** (free tier is the default). Unset: `[mock:…]`. |

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
| `TRASH_DAYS` | `30` | How long a dropped memory stays in `trash/` before it goes for good. |
| `MEMORY_VAULT_DELETES` | on | `off` stops a memory note deleted in Obsidian from deleting the memory. |

## Finance

| Variable | Default | Purpose |
| -------- | ------- | ------- |
| `FINANCE_PLACEHOLDERS` | unset | `1` seeds an empty ledger with sample rows (purged on the first real entry). |
| `RENT_AMOUNTS` | unset | Exact e-transfer amounts to classify as rent → Housing. |
| `OCR_MODEL` | `anthropic/claude-sonnet-4.6` | Vision model for receipt photos. |
| `FINANCE_MODEL` | `anthropic/claude-sonnet-4.6` | Classifies CSV statement rows. |

## Models

Two tiers, picked during onboarding ("Free" is the default; the bot shows your OpenRouter balance and asks) or
later by saying "use paid models" / "use free models":

- **`free`** (default): `nvidia/nemotron-3-super-120b-a12b:free` for the agent, parsers and summaries;
  `nvidia/nemotron-3.5-lightning:free` for memory, reminders and stock watches;
  `nvidia/nemotron-3-nano-omni-30b-a3b-reasoning:free` for receipts and voice.
- **`paid`**: cheap-but-good by default, with a ladder the agent climbs only when a request needs it.
  `anthropic/claude-haiku-4.5` for the agent, summaries, CSV rows and receipts; `google/gemini-3.1-flash-lite` for
  the strict parsers, memory, reminders and stock watches; `mistralai/voxtral-small-24b-2507` for voice. When the
  agent judges a request needs real reasoning it calls `escalate` and the turn continues on
  `anthropic/claude-sonnet-5` (**strong**), or `anthropic/claude-opus-5` (**max**) for genuinely hard problems or
  when you ask for the best model. Everyday tasks, notes and money never leave the cheap rung. A fourth, separate
  lane, `x-ai/grok-4.6` (**unsafe**, unmoderated on OpenRouter, still supports tools), is used only when the owner
  explicitly asks. Routing is decided in code, not by the model: "escalate to unsafe", "use opus" or "switch to
  sonnet" pin the chat to that rung until "back to normal", and an `unsafe:` prefix routes a single message.

Free models cost nothing but are rate-limited (about 50 requests a day, 1000 once the account has ever bought $10
of credits), slower, and weaker at tool calling; voice and receipts are best-effort. Free model ids rotate on
OpenRouter; when one disappears the bot says so and you can pin another.

| Variable | Default | Purpose |
| -------- | ------- | ------- |
| `MODEL_TIER` | stored setting | `free` or `paid`; overrides what was chosen in chat. |
| `AGENT_MODEL` | tier default | The conversational agent's everyday model; must support tool calling. |
| `STRONG_MODEL` | tier default | Where `escalate("strong")` goes. |
| `MAX_MODEL` | tier default | Where `escalate("max")` goes. |
| `UNSAFE_MODEL` | tier default | Where `escalate("unsafe")` goes; must support tool calling or the turn fails. |
| `PARSER_MODEL` | tier default | Strict JSON parsers (tasks, notes, money). |
| `MEMORY_MODEL` | tier default | Fact extraction and summary folding. |
| `MEMORY_SWEEP_MODEL` | `MAX_MODEL` | The weekly memory sweep (Opus on the paid tier). |
| `CONVO_MODEL` | tier default | Summaries of forwarded batches and voice memos. |
| `FINANCE_MODEL` | tier default | CSV statement classification. |
| `OCR_MODEL` | tier default | Receipt photos (needs image input). |
| `TRANSCRIBE_MODEL` | tier default | Voice transcription (needs audio input). |
| `SHOPPER_MODEL` | tier default | Reads product pages for stock watches. |

## Decisions (Jev, optional)

[Jev](https://typesafe.ai) is TypeSafe AI's decision model: it never writes text, it answers typed questions (yes/no,
one of N options, a level on a rubric) with calibrated probabilities. With a key, the decisions below go to Jev and
only act when it is confident; without one, each falls back to the LLM prompt or keyword rule it had before.

- `/complete`: which open task you meant, with "none of these". When unsure it lists the candidates instead of
  ticking the first one.
- Stock watches: in stock, out of stock, pre-order, not a product page, or unclear. The product name comes from the
  page title, and a page whose text hasn't changed since the last check isn't judged again.
- CSV import: account type when the file doesn't say, the category of every row within its direction (low-confidence
  rows are tagged "category unsure" in their note), and whether a same-amount neighbour is really the same payment.
- `/todo`, `/note`, `/spent`, `/earned`: a Jev Decision block in each `cmd-*` agent double-checks the category.
- Chat: "forget this conversation" or "switch to Opus" however it's phrased; a hard request starts on the strong or
  max rung; a short voice note that is a thought-dump is kept as a memo; a message sent while forwards wait for a
  comment goes to the agent instead when it's plainly unrelated.
- Memory: a new name that is an existing entity under another name ("Sam" and "Sam Smith") is merged, and entity
  kinds are double-checked. Shopping-list items are re-checked grocery or not.

| Variable | Default | Purpose |
| -------- | ------- | ------- |
| `JEV_API_KEY` | unset | Key from console.typesafe.ai. Unset: every decision uses its previous path. |
| `JEV_MODEL` | `jev-latest` | Pin a versioned id (e.g. `jev-1.13.0`) so thresholds don't drift when the alias moves. |
| `JEV_BASE_URL` | `https://api.typesafe.ai` | API root, for a proxy or gateway. |
| `JEV_TRIAGE` | on | `off` stops the per-message chat triage (control phrasing, starting rung), which adds one Jev call per text message. |

## Behaviour

| Variable | Default | Purpose |
| -------- | ------- | ------- |
| `MEMORY_WINDOW` | `12` | Recent turns the agent sees verbatim; older ones live in the rolling summary. |
| `VOICE_MEMO_SECS` | `45` | Voice notes this long or longer are memos. |
| `FORWARD_SETTLE_SECS` | `4` | Quiet time after the last forward before a batch is summarised. |
| `FORWARD_PEEK_SECS` | `1` | Extra poll after a plain message to catch forwards behind it (`0` = off). |
| `FORWARD_NOTE_TTL_SECS` | `600` | How long "what are these about?" waits for a reply. |
| `SHOPPER_CHECK_SECS` | `3600` | Stock re-check interval. |
| `GANTT_REFRESH_SECS` | `600` | How often the task timeline is regenerated. |
| `MEMORY_SWEEP` | on | `off` / `0` / `false` stops the weekly sweep; "tidy up your memory" still works. |
| `MEMORY_SWEEP_DAYS` | `7` | Days between memory sweeps. |
| `MEMORY_SWEEP_MAX` | `60` | Most new entries read in one sweep — the ceiling on what a run costs. |

## Web search

| Variable | Default | Purpose |
| -------- | ------- | ------- |
| `BRAVE_API_KEY` | unset | Use [Brave Search](https://brave.com/search/api/) (free tier) instead of DuckDuckGo. |

Without a key the `web_search` tool scrapes DuckDuckGo's HTML endpoint — free and keyless, but it occasionally
rate-limits; the bot says so when that happens. `read_page` fetches any http(s) URL and hands the model its text.

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

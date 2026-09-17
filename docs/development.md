# Development

## Layout

```
backend/            Rust (axum + tokio + rusqlite) — the whole product
  src/main.rs         startup: db, config, finance, memory, vault, scheduler, bot, HTTP API
  src/config.rs       stored settings + paired chats (settings / chats tables)
  src/telegram/       the bot: mod.rs loop, agent.rs (tool-calling agent), onboarding.rs,
                      callbacks.rs, media.rs (voice/receipt/CSV), summaries.rs, notes.rs,
                      edits.rs (editing saved notes/summaries/memories), money.rs, lists.rs,
                      watch.rs, machines.rs, prefs.rs, workflows.rs
  src/memory.rs       conversation store + knowledge graph + recall
  src/sweep.rs        weekly consolidation of duplicate memory entries
  src/vault.rs        Markdown files, Bases, starter files
  src/finance.rs      ledger, CSV import, dedup;  charts.rs renders Charts.md / Gantt.md
  src/jev.rs          optional Jev client: typed noul/choice/score decisions (telegram/triage.rs uses it per message)
  src/engine.rs       graph execution engine for the workflow agents
  agents/             bundled cmd-*.json parsers (todo, note, spent, earned, notify, email)
  assets/             starter .base files and Home.canvas
frontend/           SvelteKit + @xyflow/svelte visual flow editor for the workflow agents (bun)
deploy/             optimimer-setup (the interactive setup), deb/ (Debian packaging), obsidian-sync.service
                    template, bin/ prebuilt aarch64 binary for clone installs
install.sh          clone install: copies this checkout into /opt/optimimer and runs deploy/optimimer-setup
Dockerfile          runtime image published to ghcr.io by the release workflow
.github/workflows/  ci.yml (cargo test) and release.yml (tag → .deb + tarballs + container image)
docs/               this documentation (GitHub Pages)
```

## Run locally

```sh
cp backend/.env.example backend/.env    # fill in TELEGRAM_BOT_TOKEN and OPENROUTER_API_KEY
bun run setup                           # frontend deps
bun run dev                             # backend on :8799 + frontend on :5173, colour-prefixed logs
```

Only those two keys are needed; see [Configuration](configuration.md) for the optional ones. Real environment
variables override the file, so a secret manager that exports variables (Infisical, 1Password CLI, …) works without
any special support.

Or just the backend: `cd backend && cargo run`. Tests: `cd backend && cargo test`. The repository is not
`rustfmt`-clean by design; do not run `cargo fmt` on it.

The bot starts with an empty database, generates a setup code and prints it in the log. Message your bot with it
to pair, and you are in the same onboarding a fresh install gets.

## Build

```sh
bun run build     # frontend → frontend/build/, backend → backend/target/release/optimimer-backend
```

## Cross-compile and deploy to a Pi

The Pi installer needs a static aarch64 binary. Once, on an x86_64 machine:

```sh
rustup target add aarch64-unknown-linux-musl
cargo install cargo-zigbuild             # also needs `zig` on PATH
```

Then:

```sh
make build-pi                            # refreshes deploy/bin/optimimer-backend-aarch64
make deploy PI=user@pi-host              # build, copy binary + agents + installer, run the interactive setup over SSH
make redeploy PI=user@pi-host            # build, copy, restart — no prompts, config untouched
make logs | restart | stop | uninstall PI=user@pi-host
make deploy PI=… INFISICAL_ENV=dev       # optional: export secrets from a local Infisical into the remote setup
```

`build-pi` remaps source paths (`--remap-path-prefix`) so the committed binary contains no local directories.
Committing the refreshed binary is what makes `./install.sh` on a fresh clone pick up your change.

## Releasing

```sh
git tag v0.2.0 && git push --tags
```

The release workflow cross-compiles static binaries for arm64 and amd64, builds `.deb` packages with
`deploy/deb/build.sh`, attaches them and tarballs to a GitHub Release, and pushes a multi-arch image to
`ghcr.io/<owner>/optimimer`. Build a package locally with
`deploy/deb/build.sh deploy/bin/optimimer-backend-aarch64 arm64 0.2.0 dist` (needs `dpkg-deb`).

## Workflow agents

Besides the conversational agent, a handful of small **workflow agents** (`backend/agents/cmd-*.json`) turn free
text into structured JSON: `cmd-todo`, `cmd-note`, `cmd-spent`, `cmd-earned`, and the two that act, `cmd-notify`
(reminders) and `cmd-email`. They run on the graph engine and can be edited in the web UI. Blocks: Trigger, AI Step
(OpenRouter), Jev Decision (a typed choice / noul / score; `{enabled:false}` without `JEV_API_KEY`), HTTP, Condition,
Schedule, Output; any string field supports `{{input}}`, `{{input.field}}`,
`{{nodeId}}`, `{{nodeId.field}}`. On start the bundled files are re-seeded, so a deleted agent comes back.

## HTTP API

Bound to `127.0.0.1:8799` by default (`OPTIMIMER_BIND`), no authentication.

| Method | Path | Purpose |
| ------ | ---- | ------- |
| GET | `/api/health` | liveness |
| GET / POST | `/api/workflows` | list / create agents |
| GET / PUT / DELETE | `/api/workflows/:id` | read / update / delete |
| POST | `/api/workflows/:id/run` | run a stored (or inline) workflow |

## Conventions

- Markdown is hard-wrapped at 120 columns.
- Nothing personal in source, assets, agents or docs — defaults are generic; the owner's data comes from onboarding.
- Commit messages describe the why; the history was scrubbed of personal data on 2026-09-11, so old clones must be
  re-cloned.

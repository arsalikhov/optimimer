---
name: create-optimimer-agent
description: >-
  Build an Optimimer agent — a workflow graph (Trigger → AI Step / HTTP / Condition / Notion → Output) saved as JSON
  and run from Telegram or the web UI. Use when the user wants to create, scaffold, or design an Optimimer agent /
  workflow, add nodes to a flow, or wire up a Lindy-style automation in this repo.
---

# Create an Optimimer agent

An **agent** in Optimimer is a `Workflow`: a directed graph of typed **nodes** connected by **edges**. A run starts at
the `trigger` node with a payload exposed as `{{input}}`, flows through the graph, and the `output` node's value is the
final reply (what Telegram sends back). The backend (Rust/axum) stores workflows and executes the graph; AI Step nodes
call models via OpenRouter.

Source of truth for the schema: `backend/src/models.rs` (Workflow/Node/Edge), `backend/src/engine.rs` (how each node
executes + templating), `frontend/src/lib/types.ts` (`NODE_DEFS` — the node catalog + defaults).

## Workflow JSON shape

```json
{
  "name": "Agent name",
  "nodes": [
    { "id": "trigger_1", "type": "trigger", "position": { "x": 40, "y": 160 }, "data": { "label": "When triggered" } }
  ],
  "edges": [
    { "id": "e1", "source": "trigger_1", "target": "llm_1" }
  ]
}
```

- `id` is omitted on create — the server assigns a UUID. (`updated_at` is server-set too.)
- Node `id` is any unique string; convention is `<type>_<n>` (e.g. `llm_1`). It is also the templating handle.
- `position` is for canvas layout only; it doesn't affect execution. Space nodes ~280px apart on x for a clean layout.
- Edges connect `source` → `target` by node id. Condition branches additionally need `sourceHandle` (see below).

## Node types

Each node's `data` carries type-specific fields. Defaults live in `NODE_DEFS` (`frontend/src/lib/types.ts`).

| `type`      | Key `data` fields                                                        | Run output (reference downstream as) |
| ----------- | ------------------------------------------------------------------------ | ------------------------------------ |
| `trigger`   | `label`                                                                  | — (payload is `{{input}}`)           |
| `llm`       | `label`, `model`, `system`, `prompt`                                     | `{{id.text}}`; if the model returns JSON, also `{{id.json.field}}` |
| `http`      | `label`, `method` (GET/POST/PUT/DELETE), `url`, `body`, `headers_json` (JSON object of extra headers) | `{{id.status}}`, `{{id.body}}` |
| `condition` | `label`, `left`, `op` (eq/ne/contains/gt/lt), `right`                    | `{{id.pass}}` + routes true/false    |
| `notion`    | `label`, `op` (search/query_database/create_page/create_pages/update_page/append/get_page), `query`, `database_id`, `page_id`, `block_id`, `title`, `title_prop`, `content`, `filter_json`, `properties_json` (a Notion `properties` object as JSON, merged into create/update); `create_pages` also takes `items_json` (array of `{title,properties,notes,clear_at}`) + `chat_id` and fans out one page per item, scheduling a clear for any item with a `clear_at` | Notion result object; `create_page`/`update_page` give `{{id.id}}`, `{{id.url}}`; `create_pages` gives `{{id.count}}`, `{{id.summary}}` |
| `schedule`  | `label`, `fire_at` (RFC3339; empty = no-op), `chat_id`, `message` (Telegram ping), `page_id` + `properties_json` (Notion update) | `{{id.scheduled}}`, `{{id.id}}` |
| `output`    | `label`, `value`                                                         | `{{id.value}}`                       |

The `schedule` node registers a future action with the backend scheduler (a 30s
worker fires due entries): set `message` to send a Telegram ping, and/or set
`page_id` + `properties_json` to patch a Notion page at `fire_at`. It powers
reminders and the "auto-clear a meeting an hour after it starts" rule.

**Models** (`llm.model`) — pick deliberately:

- `nvidia/nemotron-3-super-120b-a12b` — default cheap base for general steps. `:free` variant also available.
- `anthropic/claude-sonnet-4.6` — use for strict / structured / instruction-heavy steps where reliability matters.
- Also valid: `openai/gpt-4o`, `openai/gpt-4o-mini`, `google/gemini-flash-1.5`, `meta-llama/llama-3.1-70b-instruct`.

## Templating

Any string field supports `{{...}}` interpolation, resolved at run time:

- `{{input}}` — the trigger payload. `{{input.field}}` digs into a JSON payload.
- `{{nodeId}}` — that node's whole output. `{{nodeId.field}}` digs in — e.g. `{{llm_1.text}}`, `{{http_1.body.items}}`.
- `{{nodeId.json.field}}` — for `llm` nodes, dig into JSON the model returned (have it output strict JSON, then pull `{{parse.json.title}}` etc.). An object/array resolves to its JSON text, handy for `properties_json`/`body`.
- `{{env.NAME}}` — a backend env var (e.g. `{{env.LIFEOS_DB_ID}}`). Keeps ids/secrets out of saved workflow JSON.
- Slash-command agents (`cmd-*`, run from Telegram) also get `{{input.now}}`, `{{input.tz}}`, `{{input.chat_id}}`, `{{input.category}}`, `{{input.command}}` alongside `{{input.text}}`.
- Reference only **upstream** nodes (ones that run before this one along the edges).

## Condition branching

A `condition` node evaluates `left <op> right` (both interpolated) and routes to exactly one of two handles. The
outgoing edge MUST set `sourceHandle` to `"true"` or `"false"`:

```json
{ "id": "e3", "source": "cond_1", "target": "llm_yes", "sourceHandle": "true" },
{ "id": "e4", "source": "cond_1", "target": "llm_no",  "sourceHandle": "false" }
```

Nodes on the not-taken branch are marked `skipped`. `gt`/`lt` compare numerically when both sides parse as numbers.

## Build procedure

1. **Clarify the goal** if vague: what triggers it, what it should produce, any external calls (HTTP/Notion), any
   branching. Keep it to the smallest graph that satisfies the request.
2. **Sketch the path**: `trigger` → … → `output`. Every agent needs exactly one `trigger` and at least one `output`.
3. **Choose models** per AI Step (Nemotron base; Claude Sonnet for strict steps).
4. **Wire templating**: each node consumes upstream outputs via `{{id.field}}`; the `output.value` is usually
   `{{<last_llm>.text}}`.
5. **Write the JSON** to a file (e.g. `backend/agents/<slug>.json`) — never paste large JSON inline in chat.
6. **Validate** against the checklist below.
7. **Save it** (see next section).

## Saving / running

The backend runs on `http://localhost:8799`. Create via the API (server assigns the id):

```
POST /api/workflows        body = { name, nodes, edges }     → returns the saved workflow with its id
PUT  /api/workflows/:id     update          DELETE /api/workflows/:id     delete
POST /api/workflows/:id/run body = { input } → runs the stored agent; or { workflow, input } to run unsaved JSON
```

> In this repo `curl`/`wget` are intercepted — issue HTTP via `ctx_execute(language: "javascript", code: "await
> fetch('http://localhost:8799/api/workflows', { method: 'POST', headers: {'content-type':'application/json'}, body:
> JSON.stringify(wf) })")`. Outside this repo, plain `curl` is fine.

Then from Telegram: `/agents` to list, `/use <id>` to select, send a message to run it. Without API keys the backend
still runs — AI Steps return `[mock:...]` and Notion is mocked — so you can test the graph wiring offline.

## Validation checklist

- [ ] Exactly one `trigger`; at least one `output`.
- [ ] Every node `id` is unique; edges reference only existing ids.
- [ ] Graph is connected from trigger to output; no node references a downstream/non-existent node in `{{...}}`.
- [ ] Every `condition` has both a `true` and a `false` outgoing edge with matching `sourceHandle`.
- [ ] `llm.model` is one of the allowed values; `http.method` and `condition.op` use allowed enums.
- [ ] No cycles (the engine runs a topological pass).

## Minimal example — "Cheerful Rewriter"

```json
{
  "name": "Cheerful Rewriter",
  "nodes": [
    { "id": "trigger_1", "type": "trigger", "position": { "x": 40, "y": 160 }, "data": { "label": "When triggered" } },
    { "id": "llm_1", "type": "llm", "position": { "x": 320, "y": 140 },
      "data": { "label": "Rewrite", "model": "nvidia/nemotron-3-super-120b-a12b",
                "system": "You are a cheerful assistant.", "prompt": "Rewrite this more cheerfully: {{input}}" } },
    { "id": "output_1", "type": "output", "position": { "x": 620, "y": 160 },
      "data": { "label": "Output", "value": "{{llm_1.text}}" } }
  ],
  "edges": [
    { "id": "e1", "source": "trigger_1", "target": "llm_1" },
    { "id": "e2", "source": "llm_1", "target": "output_1" }
  ]
}
```

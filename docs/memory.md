# Memory

The agent sees a **bounded context**: a rolling summary of everything older plus the last `MEMORY_WINDOW` turns
(default 12). Every turn is stored, but the model never receives the whole history, so context does not bloat.

The graph is about your life, not about your use of the bot. A turn that only ran a command — ticking a shopping
list, logging an expense, waking a machine, changing a setting — is never read for facts, so "Buy shampoo" and
"shopping mode enabled" do not become things the bot remembers. Extraction runs on notes, memos, summaries and
tasks, and on plain conversation, which is where "my dentist is Alex" actually turns up. Say "forget that" to drop
an entry that slipped through.

After each exchange a cheap model (`MEMORY_MODEL`) extracts what is worth keeping — people (with role), organisations,
projects, places, recurring topics, stated preferences, durable facts, dated events — into a small **knowledge
graph** in SQLite: nodes with a name, kind and one-line summary (full-text indexed), and typed edges between them.
Tasks and notes the bot creates join the graph too, so a person links to the task that mentions them. Your
categories are seeded as nodes and are never re-created as projects.

Say "clear context" (or "new chat", "start over") to begin a fresh thread: everything said so far drops out of
the window and the rolling summary, while the knowledge graph, tasks, notes and ledger stay. The old turns are kept
in the database, just hidden from the agent.

The agent calls `recall` before answering anything about people, plans or preferences it cannot see in the recent
turns. Recall runs a full-text search (all terms, then any term, with prefix matching), adds one hop of neighbours,
and returns a compact block of a couple of thousand characters at most. Saying "remember that …" stores a fact
directly, and correcting one ("no, Sam moved to Lisbon") rewrites that entry instead of storing a second, conflicting
version — you can rename it or change its kind the same way.

## The weekly sweep

The extractor is cheap and eager, so the graph drifts: "Sam", "Sam Rivera" and "my climbing partner" end up as three
entries with a fragment of the story each. Once a week (`MEMORY_SWEEP_DAYS`) a strong model (`MEMORY_SWEEP_MODEL` —
Opus on the paid tier) reads the entries learned since the last sweep, plus the look-alikes full-text search finds for
each, and folds duplicates into one entry that keeps every distinct fact. It tells you what it merged.

It also removes **command leftovers**: entries about using the bot rather than about you — shopping-list items, its
own modes and features, single purchases, confirmations of what it just did, one-off requests. The test it applies is
"would this still matter in a year, to someone who never used this bot?". Entries that mirror a task or note file are
never dropped, and anything it is unsure about is left alone.

The weekly run only reads what is new, so old clutter sits there until it is touched. Say "tidy up everything in your
memory" for a full pass: it un-stamps the graph and works through it a batch at a time, telling you how many entries
are left after each run.

Cost stays flat as the graph grows because an entry is only ever read once: everything a run looked at is stamped
`swept`, and the next run starts from what has been learned since. At most `MEMORY_SWEEP_MAX` entries (default 60) go
into one run, so no single call can run away with your credits. Your categories are never touched. Say "tidy up your
memory" to run it early, or set `MEMORY_SWEEP=off` to turn it off.

## Deleting, and the bin

The vault is the copy you actually read, so `memory/` is authoritative for deletions: delete a note there and the
memory behind it goes from the database too, within the hour. Renaming a note in Obsidian reads as deleting it, so ask
for the rename instead ("call that memory Sam Rivera") and the file follows. Two guards, since this cannot be undone:
an **empty** `memory/` folder is treated as a wipe or an unfinished sync and rebuilt from the graph, and a pass that
would remove more than half of a graph of five or more is skipped with a warning. Set `MEMORY_VAULT_DELETES=off` to
turn the whole behaviour off.

Nothing the bot drops is destroyed straight away. Deleted memories land in `trash/` for `TRASH_DAYS` (30 by default),
stamped with the date and why they went — merged into something, dropped as a command leftover, deleted in the vault,
or forgotten on request. Ask "what's in the bin?" to see them and "restore Sam" to put one back; the relations come
back too, where the other end still exists. After the month is up they go for good. Notes in the bin carry no
wikilinks, so they never show up as nodes in the graph view.

The graph is mirrored into the vault as `memory/<Name>.md` notes with wikilinks to related nodes and to the task or
note files, so it appears in Obsidian's graph view and in `Memory.base`. Task and note nodes point at their own
files rather than getting a duplicate note. Voice transcripts (`transcripts/`) are deliberately left out, so recall
returns facts and not raw speech. If you wipe `memory/`, the bot rebuilds it on the next start.

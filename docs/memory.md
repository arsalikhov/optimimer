# Memory

The agent sees a **bounded context**: a rolling summary of everything older plus the last `MEMORY_WINDOW` turns
(default 12). Every turn is stored, but the model never receives the whole history, so context does not bloat.

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
directly.

The graph is mirrored into the vault as `memory/<Name>.md` notes with wikilinks to related nodes and to the task or
note files, so it appears in Obsidian's graph view and in `Memory.base`. Task and note nodes point at their own
files rather than getting a duplicate note. If you wipe `memory/`, the bot rebuilds it on the next start.
